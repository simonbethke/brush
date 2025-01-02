use crate::{
    camera::{focal_to_fov, fov_to_focal, Camera},
    gaussian_splats::Splats,
    safetensor_utils::safetensor_to_burn,
    Backend,
};

use anyhow::{Context, Result};
use brush_rerun::{BurnToImage, BurnToRerun};
use burn::{
    backend::Autodiff,
    tensor::{Float, Tensor, TensorPrimitive},
};
use burn_wgpu::{Wgpu, WgpuDevice};
use safetensors::SafeTensors;
use std::{fs::File, io::Read};

type DiffBack = Autodiff<Wgpu>;

const USE_RERUN: bool = false;

#[tokio::test]
async fn test_reference() -> Result<()> {
    let device = WgpuDevice::DefaultDevice;

    let crab_img = image::open("./test_cases/crab.png")?;
    // Convert the image to RGB format
    // Get the raw buffer
    let raw_buffer = crab_img.to_rgb8().into_raw();
    let crab_tens: Tensor<DiffBack, 3> = Tensor::<_, 1>::from_floats(
        raw_buffer
            .iter()
            .map(|&b| b as f32 / 255.0)
            .collect::<Vec<_>>()
            .as_slice(),
        &device,
    )
    .reshape([crab_img.height() as usize, crab_img.width() as usize, 3]);
    // Concat alpha to tensor.
    let crab_tens = Tensor::cat(
        vec![
            crab_tens,
            Tensor::zeros(
                [crab_img.height() as usize, crab_img.width() as usize, 1],
                &device,
            ),
        ],
        2,
    );

    let rec = if USE_RERUN {
        rerun::RecordingStreamBuilder::new("render test")
            .connect_tcp()
            .ok()
    } else {
        None
    };

    for (i, path) in ["tiny_case", "basic_case", "mix_case"].iter().enumerate() {
        println!("Checking path {path}");

        let mut buffer = Vec::new();
        let _ = File::open(format!("./test_cases/{path}.safetensors"))?.read_to_end(&mut buffer)?;

        let tensors = SafeTensors::deserialize(&buffer)?;
        let splats = Splats::<DiffBack>::from_safetensors(&tensors, &device)?;

        let img_ref = safetensor_to_burn::<DiffBack, 3>(&tensors.tensor("out_img")?, &device);
        let [h, w, _] = img_ref.dims();

        let fov = std::f64::consts::PI * 0.5;

        let focal = fov_to_focal(fov, w as u32);
        let fov_x = focal_to_fov(focal, w as u32);
        let fov_y = focal_to_fov(focal, h as u32);

        let cam = Camera::new(
            glam::vec3(0.123, -0.123, -8.0),
            glam::Quat::IDENTITY,
            fov_x,
            fov_y,
            glam::vec2(0.5, 0.5),
        );

        let (img, aux) = DiffBack::render_splats(
            &cam,
            glam::uvec2(w as u32, h as u32),
            splats.means.val().into_primitive().tensor(),
            splats.xys_dummy.clone().into_primitive().tensor(),
            splats.log_scales.val().into_primitive().tensor(),
            splats.rotation.val().into_primitive().tensor(),
            splats.sh_coeffs.val().into_primitive().tensor(),
            splats.raw_opacity.val().into_primitive().tensor(),
            false,
        );

        let (out, aux) = (Tensor::from_primitive(TensorPrimitive::Float(img)), aux);
        let wrapped_aux = aux.clone().into_wrapped();

        if let Some(rec) = rec.as_ref() {
            rec.set_time_sequence("test case", i as i64);
            rec.log("img/render", &out.clone().into_rerun_image().await)?;
            rec.log("img/ref", &img_ref.clone().into_rerun_image().await)?;
            rec.log(
                "img/dif",
                &(img_ref.clone() - out.clone()).into_rerun().await,
            )?;
            rec.log(
                "images/tile depth",
                &wrapped_aux.calc_tile_depth().into_rerun().await,
            )?;
        }

        // Check if images match.
        assert!(out.clone().all_close(img_ref, Some(1e-5), Some(1e-6)));
        wrapped_aux.clone().debug_assert_valid();

        let num_visible = wrapped_aux.num_visible.into_scalar_async().await as usize;
        let projected_splats =
            Tensor::from_primitive(TensorPrimitive::Float(aux.projected_splats.clone()));

        let gs_ids = wrapped_aux
            .global_from_compact_gid
            .clone()
            .slice([0..num_visible]);

        let xys: Tensor<DiffBack, 2, Float> =
            projected_splats.clone().slice([0..num_visible, 0..2]);
        let xys_ref = safetensor_to_burn::<DiffBack, 2>(&tensors.tensor("xys")?, &device);
        let xys_ref = xys_ref.select(0, gs_ids.clone());

        assert!(xys.all_close(xys_ref, Some(1e-1), Some(1e-6)));

        let conics: Tensor<DiffBack, 2, Float> =
            projected_splats.clone().slice([0..num_visible, 2..5]);
        let conics_ref = safetensor_to_burn::<DiffBack, 2>(&tensors.tensor("conics")?, &device);
        let conics_ref = conics_ref.select(0, gs_ids.clone());

        assert!(conics.all_close(conics_ref, Some(1e-3), Some(1e-6)));

        let grads = (out.clone() - crab_tens.clone())
            .powi_scalar(2.0)
            .mean()
            .backward();

        // XY gradients are also in compact format.
        let v_xys = splats.xys_dummy.grad(&grads).context("no xys grad")?;
        let v_xys_ref =
            safetensor_to_burn::<DiffBack, 2>(&tensors.tensor("v_xy")?, &device).inner();
        let v_xys_ref = v_xys_ref.select(0, gs_ids.inner().clone());
        assert!(v_xys.all_close(v_xys_ref, Some(1e-5), Some(1e-9)));

        let v_opacities_ref =
            safetensor_to_burn::<DiffBack, 1>(&tensors.tensor("v_opacities")?, &device).inner();
        let v_opacities = splats.raw_opacity.grad(&grads).context("opacities grad")?;
        assert!(v_opacities.all_close(v_opacities_ref, Some(1e-4), Some(1e-9)));

        let v_coeffs_ref =
            safetensor_to_burn::<DiffBack, 3>(&tensors.tensor("v_coeffs")?, &device).inner();
        let v_coeffs = splats.sh_coeffs.grad(&grads).context("coeffs grad")?;
        assert!(v_coeffs.all_close(v_coeffs_ref, Some(1e-4), Some(1e-9)));

        let v_means_ref =
            safetensor_to_burn::<DiffBack, 2>(&tensors.tensor("v_means")?, &device).inner();
        let v_means = splats.means.grad(&grads).context("means grad")?;
        assert!(v_means.all_close(v_means_ref, Some(1e-4), Some(1e-9)));

        let v_quats = splats.rotation.grad(&grads).context("quats grad")?;
        let v_quats_ref =
            safetensor_to_burn::<DiffBack, 2>(&tensors.tensor("v_quats")?, &device).inner();
        assert!(v_quats.all_close(v_quats_ref, Some(1e-4), Some(1e-9)));

        let v_scales = splats.log_scales.grad(&grads).context("scales grad")?;
        let v_scales_ref =
            safetensor_to_burn::<DiffBack, 2>(&tensors.tensor("v_scales")?, &device).inner();
        assert!(v_scales.all_close(v_scales_ref, Some(1e-4), Some(1e-9)));
    }
    Ok(())
}
