// While most wgsl binding code is autogenerated, some more glue is needed for
// wgsl burn interop. This file contains some of this glue code, it's mainly
// generated by the macro below.
mod shaders;

use burn::tensor::{DType, Shape};
pub use burn_cubecl::cubecl::prelude::ExecutionMode;
pub use burn_cubecl::cubecl::{
    CubeCount, CubeDim, KernelId, client::ComputeClient, compute::CompiledKernel,
    compute::CubeTask, server::ComputeServer,
};
pub use burn_cubecl::{CubeRuntime, cubecl::Compiler, tensor::CubeTensor};

use bytemuck::Pod;
use wgpu::naga;

pub fn calc_cube_count<const D: usize>(sizes: [u32; D], workgroup_size: [u32; 3]) -> CubeCount {
    CubeCount::Static(
        sizes.first().unwrap_or(&1).div_ceil(workgroup_size[0]),
        sizes.get(1).unwrap_or(&1).div_ceil(workgroup_size[1]),
        sizes.get(2).unwrap_or(&1).div_ceil(workgroup_size[2]),
    )
}

pub fn module_to_compiled<C: Compiler>(
    debug_name: &'static str,
    module: &naga::Module,
    workgroup_size: [u32; 3],
) -> CompiledKernel<C> {
    let info = naga::valid::Validator::new(
        naga::valid::ValidationFlags::empty(),
        naga::valid::Capabilities::all(),
    )
    .validate(module)
    .expect("Failed to compile kernel");

    let shader_string =
        naga::back::wgsl::write_string(module, &info, naga::back::wgsl::WriterFlags::empty())
            .expect("failed to convert naga module to source");

    // Dawn annoyingly wants some extra syntax to enable subgroups,
    // so just hack this in when running on wasm.
    #[cfg(target_family = "wasm")]
    let shader_string = if shader_string.contains("subgroupAdd") {
        "enable subgroups;\n".to_owned() + &shader_string
    } else {
        shader_string
    };

    CompiledKernel {
        entrypoint_name: "main".to_owned(),
        debug_name: Some(debug_name),
        source: shader_string,
        repr: None,
        cube_dim: CubeDim::new(workgroup_size[0], workgroup_size[1], workgroup_size[2]),
        debug_info: None,
    }
}

pub fn calc_kernel_id<T: 'static>(values: &[bool]) -> KernelId {
    let mut kernel_id = KernelId::new::<T>();

    for val in values.iter().copied() {
        kernel_id = kernel_id.info(val);
    }

    kernel_id
}

#[macro_export]
macro_rules! kernel_source_gen {
    ($struct_name:ident { $($field_name:ident),* }, $module:ident) => {
        #[derive(Debug, Copy, Clone)]
        pub(crate) struct $struct_name {
            $(
                $field_name: bool,
            )*
        }

        impl $struct_name {
            pub fn task($($field_name: bool),*) -> Box<$struct_name> {
                let kernel = Self {
                    $(
                        $field_name,
                    )*
                };

                Box::new(kernel)
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

            pub const WORKGROUP_SIZE: [u32; 3] = $module::WORKGROUP_SIZE;

            fn source(&self) -> wgpu::naga::Module {
                let shader_defs = self.create_shader_hashmap();
                $module::create_shader_source(shader_defs)
            }
        }

        impl<C: brush_kernel::Compiler> brush_kernel::CubeTask<C> for $struct_name {
            fn id(&self) -> brush_kernel::KernelId {
                brush_kernel::calc_kernel_id::<Self>(&[$(self.$field_name),*])
            }

            fn compile(
                &self,
                _compiler: &mut C,
                _compilation_options: &C::CompilationOptions,
                _mode: brush_kernel::ExecutionMode
            ) -> brush_kernel::CompiledKernel<C> {
                let module = self.source();
                brush_kernel::module_to_compiled(stringify!($struct_name), &module, Self::WORKGROUP_SIZE)
            }
        }
    };
}

// Reserve a buffer from the client for the given shape.
pub fn create_tensor<const D: usize, R: CubeRuntime>(
    shape: [usize; D],
    device: &R::Device,
    client: &ComputeClient<R::Server, R::Channel>,
    dtype: DType,
) -> CubeTensor<R> {
    let shape = Shape::from(shape.to_vec());
    let bufsize = shape.num_elements() * dtype.size();
    let mut buffer = client.empty(bufsize);

    if cfg!(test) {
        use burn::tensor::ops::FloatTensorOps;
        use burn_cubecl::CubeBackend;
        // for tests - make doubly sure we're not accidentally relying on values
        // being initialized to zero by adding in some random noise.
        let f = CubeTensor::<R>::new_contiguous(
            client.clone(),
            device.clone(),
            shape.clone(),
            buffer,
            DType::F32,
        );
        let noised = CubeBackend::<R, f32, i32, u32>::float_add_scalar(f, -12345.0);
        buffer = noised.handle;
    }

    CubeTensor::new_contiguous(client.clone(), device.clone(), shape, buffer, dtype)
}

/// Create a buffer to use as a shader uniform, from a structure.
pub fn create_uniform_buffer<R: CubeRuntime, T: Pod>(
    val: T,
    device: &R::Device,
    client: &ComputeClient<R::Server, R::Channel>,
) -> CubeTensor<R> {
    let bytes = bytemuck::bytes_of(&val);
    CubeTensor::new_contiguous(
        client.clone(),
        device.clone(),
        Shape::new([bytes.len() / 4]),
        client.create(bytes),
        DType::I32,
    )
}

use shaders::wg;

#[derive(Debug, Copy, Clone)]
pub(crate) struct CreateDispatchBuffer {}

impl<C: Compiler> CubeTask<C> for CreateDispatchBuffer {
    fn id(&self) -> KernelId {
        KernelId::new::<Self>()
    }

    fn compile(
        &self,
        _compiler: &mut C,
        _compilation_options: &C::CompilationOptions,
        _mode: ExecutionMode,
    ) -> CompiledKernel<C> {
        module_to_compiled(
            "CreateDispatchBuffer",
            &wg::create_shader_source(Default::default()),
            [1, 1, 1],
        )
    }
}

pub fn create_dispatch_buffer<R: CubeRuntime>(
    thread_nums: CubeTensor<R>,
    wg_size: [u32; 3],
) -> CubeTensor<R> {
    let client = thread_nums.client;
    let uniforms_buffer = create_uniform_buffer::<R, _>(
        wg::Uniforms {
            wg_size_x: wg_size[0] as i32,
            wg_size_y: wg_size[1] as i32,
            wg_size_z: wg_size[2] as i32,
        },
        &thread_nums.device,
        &client,
    );
    let ret = create_tensor([3], &thread_nums.device, &client, DType::I32);

    // SAFETY: wgsl FFI, kernel checked to have no OOB.
    unsafe {
        client.execute_unchecked(
            Box::new(CreateDispatchBuffer {}),
            CubeCount::Static(1, 1, 1),
            vec![
                uniforms_buffer.handle.binding(),
                thread_nums.handle.binding(),
                ret.clone().handle.binding(),
            ],
        );
    }

    ret
}
