#import helpers

@group(0) @binding(0) var<storage, read> gaussian_ids_sorted: array<u32>;
@group(0) @binding(1) var<storage, read> tile_bins: array<vec2u>;
@group(0) @binding(2) var<storage, read> xys: array<vec2f>;
@group(0) @binding(3) var<storage, read> cov2ds: array<vec4f>;
@group(0) @binding(4) var<storage, read> colors: array<vec4f>;
@group(0) @binding(5) var<storage, read> opacities: array<f32>;

@group(0) @binding(6) var<storage, read_write> out_img: array<vec4f>;
@group(0) @binding(7) var<storage, read_write> final_index: array<u32>;

@group(0) @binding(8) var<storage, read> info_array: array<Uniforms>;

const BLOCK_WIDTH: u32 = 16u;
const BLOCK_SIZE: u32 = BLOCK_WIDTH * BLOCK_WIDTH;

// Workgroup variables.
var<workgroup> id_batch: array<u32, BLOCK_SIZE>;
var<workgroup> xy_batch: array<vec2f, BLOCK_SIZE>;
var<workgroup> opacity_batch: array<f32, BLOCK_SIZE>;
var<workgroup> colors_batch: array<vec4f, BLOCK_SIZE>;
var<workgroup> cov2d_batch: array<vec4f, BLOCK_SIZE>;

// Keep track of how many threads are done in this workgroup.
// var<workgroup> count_done: atomic<u32>;

struct Uniforms {
    // Img resolution (w, h)
    img_size: vec2u,
    // Background color behind splats.
    background: vec3f,
}

// kernel function for rasterizing each tile
// each thread treats a single pixel
// each thread group uses the same gaussian data in a tile


@compute
@workgroup_size(BLOCK_WIDTH, BLOCK_WIDTH, 1)
fn main(
    @builtin(global_invocation_id) global_id: vec3u,
    @builtin(local_invocation_id) local_id: vec3u,
    @builtin(local_invocation_index) local_idx: u32,
    @builtin(workgroup_id) workgroup_id: vec3u,
) {
    let info = info_array[0];
    let background = info.background;
    let img_size = info.img_size;

    // each thread draws one pixel, but also timeshares caching gaussians in a
    // shared tile

    // Get index of tile being drawn.
    let tiles_xx = (img_size.x + BLOCK_WIDTH - 1) / BLOCK_WIDTH;
    let tile_id = workgroup_id.x + workgroup_id.y * tiles_xx;

    let pix_id = global_id.x + global_id.y * img_size.x;
    let pixel_coord = vec2f(global_id.xy);

    // return if out of bounds
    // keep not rasterizing threads around for reading data
    let inside = global_id.x < img_size.x && global_id.y < img_size.y;

    var done = false;
    if !inside {
        // this pixel is done
        // atomicAdd(&count_done, 1u);
        done = true;
    }

    // have all threads in tile process the same gaussians in batches
    // first collect gaussians between range.x and range.y in batches
    // which gaussians to look through in this tile
    let range = tile_bins[tile_id];
    let num_batches = (range.y - range.x + BLOCK_SIZE - 1) / BLOCK_SIZE;

    // current visibility left to render
    var T = 1.0;

    // TODO: Is this local_invocation_index?
    var pix_out = vec3f(0.0);
    var final_idx = range.y;

    // collect and process batches of gaussians
    // each thread loads one gaussian at a time before rasterizing its
    // designated pixel
    for (var b = 0u; b < num_batches; b++) {
        // resync all threads before beginning next batch
        // end early out if entire tile is done
        workgroupBarrier();

        // end early out if entire tile is done
        // if count_done >= BLOCK_SIZE {
        //     break;
        // }

        // each thread fetch 1 gaussian from front to back
        // index of gaussian to load
        let batch_start = range.x + b * BLOCK_SIZE;
        let idx = batch_start + local_idx;

        if idx < range.y {
            let g_id = gaussian_ids_sorted[idx];
            id_batch[local_idx] = g_id;
            xy_batch[local_idx] = xys[g_id];
            opacity_batch[local_idx] = opacities[g_id];
            colors_batch[local_idx] = colors[g_id];
            cov2d_batch[local_idx] = cov2ds[g_id];
        }

        // wait for other threads to collect the gaussians in batch
        workgroupBarrier();

        // process gaussians in the current batch for this pixel
        let remaining = min(BLOCK_SIZE, range.y - batch_start);
        
        if !done {
            for (var t = 0u; t < remaining; t++) {
                let cov2d = cov2d_batch[t].xyz;
                let conic =  helpers::cov2d_to_conic(cov2d);
                // Apply compensation for the blurring of 2D covariances.
                let compensation = helpers::cov_compensation(cov2d);
                let xy = xy_batch[t];
                let opac = opacity_batch[t];
                let delta = xy - pixel_coord;
                let sigma = 0.5f * (conic.x * delta.x * delta.x + conic.z * delta.y * delta.y) + conic.y * delta.x * delta.y;
                let alpha = min(0.99f, opac * compensation * exp(-sigma));

                if sigma < 0.0 || alpha < 1.0 / 255.0 {
                    continue;
                }

                let next_T = T * (1.0 - alpha);

                if next_T <= 1e-4f { 
                    // this pixel is done
                    // we want to render the last gaussian that contributes and note
                    // that here idx > range.x so we don't underflow
                    // atomicAdd(&count_done, 1u);
                    done = true;
                    break;
                }

                let vis = alpha * T;

                let c = colors_batch[t].xyz;
                pix_out += c * vis;
                T = next_T;
                final_idx = batch_start + t;
            }
        }
    }

    if inside {
        // add background

        final_index[pix_id] = final_idx; // index of in bin of last gaussian in this pixel
        let final_color = pix_out + T * background;
        out_img[pix_id] = vec4f(final_color, T);
    }
}
