//! Full-GPU renderer: the chaos game runs as a compute shader (see
//! `compute.wgsl`). Threads each iterate an independent trajectory and
//! atomically accumulate into a histogram; downsample+max and tone-map are also
//! compute passes. Nothing but genome packing happens on the CPU.
//!
//! [`GpuContext`] holds the device + pipelines so many renders (e.g. animation
//! frames, or a breeding population) amortize the ~100ms GPU init.
//!
//! Note: the GPU uses a per-thread PCG RNG, so individual pixels differ from the
//! CPU renderer's stream — but the *flame* (the attractor) is the same. The
//! "(genome, seed) is byte-identical" guarantee is about CPU-native vs CPU-wasm;
//! the GPU is its own (faster) path.

use bytemuck::{Pod, Zeroable};
use flame_core::Genome;
use wgpu::util::DeviceExt;

use crate::{readback_rgba8, GpuOpts};

const STRIDE: usize = 36; // floats per transform
const COLOR_SCALE: f32 = 256.0;
const THREADS: u32 = 1 << 18; // parallel trajectories

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Params {
    dims: [u32; 4],     // hw, hh, n_transforms, has_final
    ctrl: [u32; 4],     // plot_iters, burn_in, seed, ss
    out_dims: [u32; 4], // out_w, out_h, _, _
    cam: [f32; 4],      // world->image affine a, b, c, d
    cam2: [f32; 4],     // e, f, color_scale, total_weight
    tone: [f32; 4],     // gamma, brightness, vibrancy, _
    bg: [f32; 4],
}

fn push_transform(v: &mut Vec<f32>, t: &flame_core::Transform) {
    v.push(t.weight as f32);
    v.push(t.color as f32);
    let a = &t.affine;
    v.extend_from_slice(&[a.a as f32, a.b as f32, a.c as f32, a.d as f32, a.e as f32, a.f as f32]);
    let p = &t.post;
    v.extend_from_slice(&[p.a as f32, p.b as f32, p.c as f32, p.d as f32, p.e as f32, p.f as f32]);
    for i in 0..22 {
        v.push(t.variations.get(i).copied().unwrap_or(0.0) as f32);
    }
}

/// Holds the GPU device and the three compute pipelines so it can render many
/// genomes/frames without re-initializing.
pub struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    chaos: wgpu::ComputePipeline,
    downs: wgpu::ComputePipeline,
    tonemap: wgpu::ComputePipeline,
}

impl GpuContext {
    pub async fn new() -> GpuContext {
        let mut idesc = wgpu::InstanceDescriptor::new_without_display_handle();
        idesc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(idesc);
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .expect("no suitable GPU adapter found");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("flame-compute"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::Performance,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .expect("failed to create device");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("compute"),
            source: wgpu::ShaderSource::Wgsl(include_str!("compute.wgsl").into()),
        });
        let make = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: None,
                module: &shader,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let chaos = make("cs_chaos");
        let downs = make("cs_downsample");
        let tonemap = make("cs_tonemap");
        GpuContext { device, queue, chaos, downs, tonemap }
    }

    /// Render one genome to RGBA8 (`width*height*4`).
    pub async fn render(&self, genome: &Genome, opts: &GpuOpts) -> Vec<u8> {
        let (ow, oh, ss) = (opts.width, opts.height, opts.ss);
        let hw = (ow * ss) as u32;
        let hh = (oh * ss) as u32;
        let device = &self.device;

        // ---- pack genome --------------------------------------------------
        let n = genome.transforms.len();
        let mut gbuf: Vec<f32> = Vec::with_capacity((n + 1) * STRIDE);
        for t in &genome.transforms {
            push_transform(&mut gbuf, t);
        }
        let has_final = if let Some(ft) = &genome.final_transform {
            push_transform(&mut gbuf, ft);
            1u32
        } else {
            0u32
        };
        let total_weight: f32 = genome.transforms.iter().map(|t| t.weight as f32).sum();

        let mut palette = Vec::with_capacity(256 * 3);
        for i in 0..256 {
            let rgb = genome.palette.color(i as f64 / 255.0);
            palette.extend_from_slice(&[rgb[0] as f32, rgb[1] as f32, rgb[2] as f32]);
        }

        let cam = genome.camera.world_to_image(hw as usize, hh as usize);
        let plot_iters = opts.samples.div_ceil(THREADS as u64).max(1) as u32;

        let params = Params {
            dims: [hw, hh, n as u32, has_final],
            ctrl: [plot_iters, opts.burn_in as u32, opts.seed as u32, ss as u32],
            out_dims: [ow as u32, oh as u32, 0, 0],
            cam: [cam.a as f32, cam.b as f32, cam.c as f32, cam.d as f32],
            cam2: [cam.e as f32, cam.f as f32, COLOR_SCALE, total_weight],
            tone: [genome.gamma as f32, genome.brightness as f32, genome.vibrancy as f32, 0.0],
            bg: [
                genome.background[0] as f32,
                genome.background[1] as f32,
                genome.background[2] as f32,
                1.0,
            ],
        };

        // ---- buffers ------------------------------------------------------
        let storage = |contents: &[u8]| {
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents,
                usage: wgpu::BufferUsages::STORAGE,
            })
        };
        let genome_buf = storage(bytemuck::cast_slice(&gbuf));
        let palette_buf = storage(bytemuck::cast_slice(&palette));
        let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("params"),
            contents: bytemuck::bytes_of(&params),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let zeroed = |size: u64| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            })
        };
        let hist_buf = zeroed((hw as u64 * hh as u64 * 4) * 4);
        let down_buf = zeroed((ow as u64 * oh as u64 * 4) * 4);
        let max_buf = zeroed(4);

        let out_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("out"),
            size: wgpu::Extent3d {
                width: ow as u32,
                height: oh as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out_view = out_tex.create_view(&wgpu::TextureViewDescriptor::default());

        // ---- bind groups --------------------------------------------------
        let chaos_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.chaos.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: genome_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: params_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: hist_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: palette_buf.as_entire_binding() },
            ],
        });
        let down_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.downs.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 1, resource: params_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: hist_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: down_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: max_buf.as_entire_binding() },
            ],
        });
        let tone_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.tonemap.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 1, resource: params_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: down_buf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 5, resource: max_buf.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&out_view),
                },
            ],
        });

        // ---- dispatch -----------------------------------------------------
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut p = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("chaos"),
                timestamp_writes: None,
            });
            p.set_pipeline(&self.chaos);
            p.set_bind_group(0, &chaos_bg, &[]);
            p.dispatch_workgroups(THREADS / 64, 1, 1);
        }
        {
            let mut p = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("downsample"),
                timestamp_writes: None,
            });
            p.set_pipeline(&self.downs);
            p.set_bind_group(0, &down_bg, &[]);
            p.dispatch_workgroups(((ow * oh) as u32).div_ceil(64), 1, 1);
        }
        {
            let mut p = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("tonemap"),
                timestamp_writes: None,
            });
            p.set_pipeline(&self.tonemap);
            p.set_bind_group(0, &tone_bg, &[]);
            p.dispatch_workgroups((ow as u32).div_ceil(8), (oh as u32).div_ceil(8), 1);
        }
        self.queue.submit(Some(encoder.finish()));

        readback_rgba8(&self.device, &self.queue, &out_tex, ow as u32, oh as u32).await
    }
}

/// Render a genome entirely on the GPU and return RGBA8 (`width*height*4`).
pub fn render_gpu_compute(genome: &Genome, opts: &GpuOpts) -> Vec<u8> {
    pollster::block_on(async {
        let ctx = GpuContext::new().await;
        ctx.render(genome, opts).await
    })
}

/// Render `frames` of a genome spun a full turn, reusing one GPU context.
pub fn render_frames_compute(genome: &Genome, opts: &GpuOpts, frames: usize) -> Vec<Vec<u8>> {
    pollster::block_on(async {
        let ctx = GpuContext::new().await;
        let mut out = Vec::with_capacity(frames);
        for f in 0..frames {
            let mut g = genome.clone();
            g.camera.rotate += (f as f64 / frames as f64) * std::f64::consts::TAU;
            out.push(ctx.render(&g, opts).await);
        }
        out
    })
}
