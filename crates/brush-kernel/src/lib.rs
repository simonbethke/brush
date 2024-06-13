// While most wgsl binding code is autogenerated, some more glue is needed for
// wgsl burn interop. This file contains some of this glue code, it's mainly
// generated by the macro below.
use std::mem::size_of;

use burn_compute::{
    channel::ComputeChannel,
    client::ComputeClient,
    server::{Binding, ComputeServer},
};

use burn::{
    backend::{
        wgpu::{AutoGraphicsApi, CubeCount, CubeDim, WgpuDevice, WgpuRuntime},
        Autodiff,
    },
    tensor::Shape,
};
use burn_cube::{
    compute::{CompiledKernel, CubeTask},
    Runtime,
};

use burn_jit::{tensor::JitTensor, JitBackend, JitElement};
use bytemuck::NoUninit;
use glam::uvec3;
use tracing::info_span;

pub type BurnRuntime = WgpuRuntime<AutoGraphicsApi>;
type BurnClient =
    ComputeClient<<BurnRuntime as Runtime>::Server, <BurnRuntime as Runtime>::Channel>;

pub type BurnBack = JitBackend<BurnRuntime, f32, i32>;
pub type BurnBackDiff = Autodiff<BurnBack>;

pub trait SplatKernel
where
    Self: Sized + Clone + Send + Sync + 'static,
{
    const SPAN_NAME: &'static str;
    const WORKGROUP_SIZE: [u32; 3];
    type Uniforms: NoUninit;

    fn id(&self) -> String;
    fn source(&self) -> naga::Module;
    fn label(&self) -> Option<&'static str>;

    fn execute<
        S: ComputeServer<Kernel = Box<dyn CubeTask>>,
        C: ComputeChannel<S>,
        const D: usize,
    >(
        self,
        client: &ComputeClient<S, C>,
        uniforms: Self::Uniforms,
        read_handles: &[Binding<S>],
        write_handles: &[Binding<S>],
        executions: [u32; D],
    ) {
        let _span = info_span!("Executing", "{}", Self::SPAN_NAME).entered();

        let execs = uvec3(
            executions
                .first()
                .unwrap_or(&1)
                .div_ceil(Self::WORKGROUP_SIZE[0]),
            executions
                .get(1)
                .unwrap_or(&1)
                .div_ceil(Self::WORKGROUP_SIZE[1]),
            executions
                .get(2)
                .unwrap_or(&1)
                .div_ceil(Self::WORKGROUP_SIZE[2]),
        );

        let wg = CubeCount::new(execs.x, execs.y, execs.z);

        let kernel = Box::new(WrapKernel {
            cube_count: wg,
            splat: self,
        });

        if size_of::<Self::Uniforms>() != 0 {
            let uniform_data = client.create(bytemuck::bytes_of(&uniforms)).binding();
            let total_handles = [[uniform_data].as_slice(), read_handles, write_handles].concat();
            client.execute(kernel, total_handles);
        } else {
            let total_handles = [read_handles, write_handles].concat();
            client.execute(kernel, total_handles);
        }
    }
}

struct WrapKernel<T> {
    cube_count: CubeCount,
    splat: T,
}

impl<T: SplatKernel> CubeTask for WrapKernel<T> {
    fn id(&self) -> String {
        self.splat.id()
    }

    fn compile(&self) -> CompiledKernel {
        let module = self.splat.source();
        let info = naga::valid::Validator::new(
            naga::valid::ValidationFlags::empty(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap();

        let shader_string =
            naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
                .expect("failed to convert naga module to source");

        CompiledKernel {
            source: shader_string,
            cube_dim: CubeDim::new(
                T::WORKGROUP_SIZE[0],
                T::WORKGROUP_SIZE[1],
                T::WORKGROUP_SIZE[2],
            ),
            // This is just a compiler hint for burn, but doesn't have to be set.
            shared_mem_bytes: 0,
        }
    }

    fn launch_settings(&self) -> burn_cube::compute::LaunchSettings {
        burn_cube::compute::LaunchSettings {
            cube_count: self.cube_count.clone(),
        }
    }
}

#[macro_export]
macro_rules! kernel_source_gen {
    ($struct_name:ident { $($field_name:ident),* }, $module:ident, $uniforms:ty) => {
        #[derive(Debug, Copy, Clone)]
        pub(crate) struct $struct_name {
            $(
                $field_name: bool,
            )*
        }

        impl $struct_name {
            pub fn new($($field_name: bool),*) -> Self {
                Self {
                    $(
                        $field_name,
                    )*
                }
            }

            fn create_shader_hashmap(&self) -> std::collections::HashMap<String, naga_oil::compose::ShaderDefValue> {
                let map = std::collections::HashMap::new();
                $(
                    let mut map = map;

                    if self.$field_name {
                        map.insert(stringify!($field_name).to_owned().to_uppercase(), naga_oil::compose::ShaderDefValue::Bool(true));
                    }
                )*
                map
            }
        }

        impl SplatKernel for $struct_name {
            const SPAN_NAME: &'static str = stringify!($struct_name);
            type Uniforms = $uniforms;
            const WORKGROUP_SIZE: [u32; 3] = $module::WORKGROUP_SIZE;

            fn source(&self) -> naga::Module {
                let shader_defs = self.create_shader_hashmap();
                $module::create_shader_source(shader_defs)
            }

            fn id(&self) -> String {
                let ids = stringify!($struct_name).to_owned();
                $(
                    let mut ids = ids;
                    ids.push(
                        if self.$field_name {
                            '0'
                        } else {
                            '1'
                        }
                    );
                )*
                ids
            }

            fn label(&self) -> Option<&'static str> {
                Some(stringify!($struct_name))
            }
        }
    };
}

// Convert a tensors type. This only reinterprets the data, and doesn't
// do any actual conversions.
pub fn bitcast_tensor<const D: usize, EIn: JitElement, EOut: JitElement>(
    tensor: JitTensor<BurnRuntime, EIn, D>,
) -> JitTensor<BurnRuntime, EOut, D> {
    JitTensor::new(tensor.client, tensor.device, tensor.shape, tensor.handle)
}

// Reserve a buffer from the client for the given shape.
pub fn create_tensor<E: JitElement, const D: usize>(
    shape: [usize; D],
    device: &WgpuDevice,
    client: &BurnClient,
) -> JitTensor<BurnRuntime, E, D> {
    let shape = Shape::new(shape);
    let bufsize = shape.num_elements() * core::mem::size_of::<E>();
    let buffer = client.empty(bufsize);

    #[cfg(test)]
    {
        use burn::tensor::ops::FloatTensorOps;

        // for tests - make doubly sure we're not accidentally relying on values
        // being initialized to zero by adding in some random noise.
        let f =
            JitTensor::<BurnRuntime, f32, D>::new(client.clone(), device.clone(), shape, buffer);

        bitcast_tensor(BurnBack::float_add_scalar(f, -12345.0))
    }

    #[cfg(not(test))]
    JitTensor::new(client.clone(), device.clone(), shape, buffer)
}
