//! GPU fractal-flame renderer on `wgpu`.
//!
//! The chaos game (point generation) runs on the CPU via [`flame_core::iterate`]
//! — the same code as the reference renderer, so the GPU draws the *same* flame.
//! The GPU does the HDR additive accumulation and the tone-map pass. Moving the
//! chaos game itself into a compute shader (for a big speedup) is the next step;
//! the accumulation + tone-map pipeline here is what that would feed.
//!
//! `render_gpu` is headless (renders to an offscreen texture and reads it back),
//! so it runs without a window — same code path can target a surface for live
//! display and WebGPU in the browser.

use bytemuck::{Pod, Zeroable};
use flame_core::{render::iterate, Genome};
use half::f16;
use wgpu::util::DeviceExt;

mod compute;
pub use compute::{render_frames_compute, render_gpu_compute, GpuContext};

#[derive(Debug, Clone, Copy)]
pub struct GpuOpts {
    pub width: usize,
    pub height: usize,
    pub ss: usize,
    pub samples: u64,
    pub burn_in: u64,
    pub seed: u64,
}

impl Default for GpuOpts {
    fn default() -> Self {
        GpuOpts {
            width: 800,
            height: 800,
            ss: 2,
            samples: 8_000_000,
            burn_in: 20,
            seed: 0,
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    color: [f32; 3],
}

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct ToneUniform {
    dims: [u32; 4],   // out_w, out_h, ss, _pad
    params: [f32; 4], // gamma, brightness, vibrancy, log_max
    bg: [f32; 4],
}

/// Render a genome on the GPU and return an RGBA8 image (`width*height*4` bytes).
pub fn render_gpu(genome: &Genome, opts: &GpuOpts) -> Vec<u8> {
    pollster::block_on(render_gpu_async(genome, opts))
}

async fn render_gpu_async(genome: &Genome, opts: &GpuOpts) -> Vec<u8> {
    let (ow, oh, ss) = (opts.width, opts.height, opts.ss);
    let hw = (ow * ss) as u32;
    let hh = (oh * ss) as u32;

    // ---- build the point cloud on the CPU (mapped to clip space) ----------
    let to_img = genome.camera.world_to_image(hw as usize, hh as usize);
    let mut verts: Vec<Vertex> = Vec::with_capacity(opts.samples as usize);
    iterate(genome, opts.samples, opts.burn_in, opts.seed, |px, py, rgb| {
        let (ix, iy) = to_img.apply(px, py);
        if ix >= 0.0 && iy >= 0.0 && ix < hw as f64 && iy < hh as f64 {
            // pixel -> clip space (flip y so image-top maps to +1)
            let cx = (ix / hw as f64) * 2.0 - 1.0;
            let cy = 1.0 - (iy / hh as f64) * 2.0;
            verts.push(Vertex {
                pos: [cx as f32, cy as f32],
                color: [rgb[0] as f32, rgb[1] as f32, rgb[2] as f32],
            });
        }
    });

    // ---- wgpu init --------------------------------------------------------
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
            label: Some("flame-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("failed to create device");

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("flame-shaders"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
    });

    // ---- accumulation texture (HDR) ---------------------------------------
    let accum_format = wgpu::TextureFormat::Rgba16Float;
    let accum = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("accum"),
        size: wgpu::Extent3d {
            width: hw,
            height: hh,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: accum_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let accum_view = accum.create_view(&wgpu::TextureViewDescriptor::default());

    // ---- pass 1: additive points ------------------------------------------
    let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("verts"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let points_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("points"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_points"),
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Vertex>() as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x3],
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_points"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: accum_format,
                blend: Some(wgpu::BlendState {
                    // additive: out = src*1 + dst*1, for color and alpha
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::One,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::One,
                        dst_factor: wgpu::BlendFactor::One,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::PointList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("points-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &accum_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&points_pipeline);
        pass.set_vertex_buffer(0, vbuf.slice(..));
        pass.draw(0..verts.len() as u32, 0..1);
    }
    queue.submit(Some(encoder.finish()));

    // ---- find max density for log normalization (readback accum alpha) ----
    let max_count = readback_max_alpha(&device, &queue, &accum, hw, hh).await;
    let log_max = (1.0 + max_count as f64).ln().max(1e-12) as f32;

    // ---- pass 2: tone map to RGBA8 ----------------------------------------
    let out_format = wgpu::TextureFormat::Rgba8Unorm;
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
        format: out_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_view = out_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let tone = ToneUniform {
        dims: [ow as u32, oh as u32, ss as u32, 0],
        params: [
            genome.gamma as f32,
            genome.brightness as f32,
            genome.vibrancy as f32,
            log_max,
        ],
        bg: [
            genome.background[0] as f32,
            genome.background[1] as f32,
            genome.background[2] as f32,
            1.0,
        ],
    };
    let tone_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("tone"),
        contents: bytemuck::bytes_of(&tone),
        usage: wgpu::BufferUsages::UNIFORM,
    });

    let tone_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("tone-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let tone_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tone-bg"),
        layout: &tone_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&accum_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: tone_buf.as_entire_binding(),
            },
        ],
    });
    let tone_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("tone-pl"),
        bind_group_layouts: &[Some(&tone_layout)],
        immediate_size: 0,
    });
    let tone_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("tonemap"),
        layout: Some(&tone_pl),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_fullscreen"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_tonemap"),
            compilation_options: Default::default(),
            targets: &[Some(out_format.into())],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tonemap-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &out_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&tone_pipeline);
        pass.set_bind_group(0, &tone_bind, &[]);
        pass.draw(0..3, 0..1);
    }
    queue.submit(Some(encoder.finish()));

    // ---- read back the RGBA8 output ---------------------------------------
    readback_rgba8(&device, &queue, &out_tex, ow as u32, oh as u32).await
}

/// Copy a texture into a CPU-mappable buffer with 256-byte-aligned rows.
pub(crate) async fn copy_texture_to_cpu(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    w: u32,
    h: u32,
    bytes_per_texel: u32,
) -> (Vec<u8>, u32) {
    let unpadded = w * bytes_per_texel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        tx.send(r).ok();
    });
    device.poll(wgpu::PollType::wait_indefinitely()).ok();
    rx.recv().unwrap().unwrap();
    let data = slice.get_mapped_range().to_vec();
    buf.unmap();
    (data, padded)
}

async fn readback_max_alpha(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    w: u32,
    h: u32,
) -> f32 {
    // Rgba16Float = 8 bytes/texel; alpha is the 4th f16 (byte offset 6).
    let (data, padded) = copy_texture_to_cpu(device, queue, tex, w, h, 8).await;
    let mut max = 0.0f32;
    for y in 0..h {
        let row = (y * padded) as usize;
        for x in 0..w {
            let off = row + (x * 8) as usize + 6;
            let a = f16::from_bits(u16::from_le_bytes([data[off], data[off + 1]])).to_f32();
            if a > max {
                max = a;
            }
        }
    }
    max
}

pub(crate) async fn readback_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    tex: &wgpu::Texture,
    w: u32,
    h: u32,
) -> Vec<u8> {
    let (data, padded) = copy_texture_to_cpu(device, queue, tex, w, h, 4).await;
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let src = (y * padded) as usize;
        let dst = (y * w * 4) as usize;
        out[dst..dst + (w * 4) as usize].copy_from_slice(&data[src..src + (w * 4) as usize]);
    }
    out
}
