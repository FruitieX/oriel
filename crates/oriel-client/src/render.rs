//! wgpu renderer: YUV video planes → RGB, letterboxed, with the egui
//! overlay on top.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use oriel_media::{ColorMatrix, DecodedFrame, PlaneFormat};
use winit::window::Window;

const SHADER: &str = r#"
struct Params {
    // kr, kb, full_range, nv12
    m: vec4<f32>,
    // srgb_target
    o: vec4<f32>,
};
@group(0) @binding(0) var tex_y: texture_2d<f32>;
@group(0) @binding(1) var tex_u: texture_2d<f32>;
@group(0) @binding(2) var tex_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.uv = uv;
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    var y = textureSample(tex_y, samp, in.uv).r;
    let c = textureSample(tex_u, samp, in.uv);
    let v = textureSample(tex_v, samp, in.uv).r;
    var cb = c.r;
    var cr = select(v, c.g, params.m.w > 0.5);
    if (params.m.z > 0.5) {
        cb = cb - 0.5;
        cr = cr - 0.5;
    } else {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (cb - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (cr - 128.0 / 255.0) * (255.0 / 224.0);
    }
    let kr = params.m.x;
    let kb = params.m.y;
    let kg = 1.0 - kr - kb;
    let r = y + 2.0 * (1.0 - kr) * cr;
    let b = y + 2.0 * (1.0 - kb) * cb;
    let g = y - (2.0 * kb * (1.0 - kb) * cb + 2.0 * kr * (1.0 - kr) * cr) / kg;
    var rgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
    if (params.o.x > 0.5) {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
"#;

struct VideoTextures {
    format: PlaneFormat,
    size: (u32, u32),
    planes: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
}

/// Pixel rectangle (physical pixels) where the video is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    dummy: wgpu::Texture,
    video: Option<VideoTextures>,
    srgb_target: bool,
    pub egui: egui_wgpu::Renderer,
    pub adapter_info: String,
    /// Copy the next presented frame to this path (debugging aid).
    pub screenshot: Option<std::path::PathBuf>,
}

impl Renderer {
    pub fn new(window: Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window.clone())).with_env(),
        );
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .context("no compatible GPU adapter")?;
        let info = adapter.get_info();
        let adapter_info = format!("{} ({:?})", info.name, info.backend);
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("oriel"),
                ..Default::default()
            }))?;

        let caps = surface.get_capabilities(&adapter);
        // Plain 8-bit formats: the shader outputs gamma-encoded values.
        let preferred = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ];
        tracing::debug!(formats = ?caps.formats, "surface formats");
        let format = preferred
            .into_iter()
            .find(|f| caps.formats.contains(f))
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow!("surface has no formats"))?;
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        let usage = if caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        };
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&device, &config);
        tracing::info!(adapter = %adapter_info, ?format, ?present_mode, "renderer ready");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("yuv"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("yuv"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("yuv"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("yuv"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("yuv params"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let dummy = plane_texture(&device, wgpu::TextureFormat::R8Unorm, 1, 1);
        let egui = egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        Ok(Self {
            device,
            queue,
            surface,
            config,
            pipeline,
            layout,
            sampler,
            params,
            dummy,
            video: None,
            srgb_target: format.is_srgb(),
            egui,
            adapter_info,
            screenshot: None,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.video.as_ref().map(|v| v.size)
    }

    /// Upload a decoded frame's planes.
    pub fn upload(&mut self, frame: &DecodedFrame) {
        let size = (frame.width(), frame.height());
        if self
            .video
            .as_ref()
            .is_none_or(|v| v.size != size || v.format != frame.format)
        {
            self.video = Some(self.create_video(frame.format, size));
        }
        let video = self.video.as_ref().unwrap();
        for i in 0..frame.plane_count() {
            let (data, stride) = frame.plane(i);
            let (w, h) = frame.plane_size(i);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &video.planes[i],
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
        let (kr, kb) = match frame.matrix {
            ColorMatrix::Bt601 => (0.299f32, 0.114f32),
            ColorMatrix::Bt709 => (0.2126, 0.0722),
            ColorMatrix::Bt2020 => (0.2627, 0.0593),
        };
        let params: [f32; 8] = [
            kr,
            kb,
            frame.full_range as u32 as f32,
            (frame.format == PlaneFormat::Nv12) as u32 as f32,
            self.srgb_target as u32 as f32,
            0.0,
            0.0,
            0.0,
        ];
        let bytes: Vec<u8> = params.iter().flat_map(|f| f.to_le_bytes()).collect();
        self.queue.write_buffer(&self.params, 0, &bytes);
    }

    fn create_video(&self, format: PlaneFormat, (w, h): (u32, u32)) -> VideoTextures {
        let (cw, ch) = match format {
            PlaneFormat::Yuv444p => (w, h),
            _ => (w.div_ceil(2), h.div_ceil(2)),
        };
        let planes = match format {
            PlaneFormat::Nv12 => vec![
                plane_texture(&self.device, wgpu::TextureFormat::R8Unorm, w, h),
                plane_texture(&self.device, wgpu::TextureFormat::Rg8Unorm, cw, ch),
            ],
            PlaneFormat::Yuv420p | PlaneFormat::Yuv444p => vec![
                plane_texture(&self.device, wgpu::TextureFormat::R8Unorm, w, h),
                plane_texture(&self.device, wgpu::TextureFormat::R8Unorm, cw, ch),
                plane_texture(&self.device, wgpu::TextureFormat::R8Unorm, cw, ch),
            ],
        };
        let view = |t: &wgpu::Texture| t.create_view(&Default::default());
        let views: Vec<_> = planes.iter().map(view).collect();
        let dummy = view(&self.dummy);
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("yuv"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(views.get(2).unwrap_or(&dummy)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.params.as_entire_binding(),
                },
            ],
        });
        VideoTextures {
            format,
            size: (w, h),
            planes,
            bind_group,
        }
    }

    pub fn render(
        &mut self,
        viewport: Option<Viewport>,
        egui_ctx: &egui::Context,
        output: egui::FullOutput,
    ) -> Result<()> {
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            _ => return Ok(()),
        };
        let view = texture.texture.create_view(&Default::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("video"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let (Some(video), Some(vp)) = (&self.video, viewport) {
                pass.set_viewport(vp.x, vp.y, vp.w, vp.h, 0.0, 1.0);
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &video.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }
        }

        let pixels_per_point = output.pixels_per_point;
        let primitives = egui_ctx.tessellate(output.shapes, pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                self.egui
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        let extra = self.egui.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &primitives,
            &screen,
        );
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
            self.egui.render(&mut pass, &primitives, &screen);
        }
        for id in &output.textures_delta.free {
            self.egui.free_texture(id);
        }

        let readback = self.screenshot.take().and_then(|path| {
            if !self.config.usage.contains(wgpu::TextureUsages::COPY_SRC) {
                tracing::warn!("surface does not support readback; no screenshot");
                return None;
            }
            let (w, h) = (texture.texture.width(), texture.texture.height());
            let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("screenshot"),
                size: (row * h) as u64,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                texture.texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(h),
                    },
                },
                texture.texture.size(),
            );
            Some((path, buffer, row, w, h))
        });

        self.queue
            .submit(extra.into_iter().chain(std::iter::once(encoder.finish())));
        self.queue.present(texture);

        if let Some((path, buffer, row, w, h)) = readback {
            self.save_screenshot(&path, &buffer, row, w, h)?;
        }
        Ok(())
    }

    fn save_screenshot(
        &self,
        path: &std::path::Path,
        buffer: &wgpu::Buffer,
        row: u32,
        w: u32,
        h: u32,
    ) -> Result<()> {
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::PollType::wait_indefinitely())?;
        let data = slice.get_mapped_range()?;
        let bgr = matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let line = &data[(y * row) as usize..(y * row + w * 4) as usize];
            for px in line.chunks(4) {
                if bgr {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                } else {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
        }
        let file = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut png = png::Encoder::new(file, w, h);
        png.set_color(png::ColorType::Rgba);
        png.set_depth(png::BitDepth::Eight);
        png.write_header()?.write_image_data(&rgba)?;
        tracing::info!(path = %path.display(), "saved screenshot");
        Ok(())
    }
}

fn plane_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    w: u32,
    h: u32,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("video plane"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Fit `video` into `window` keeping the aspect ratio. Never upscale
/// fractionally by less than one physical pixel of slack: if the video fits
/// at 1:1 it is drawn at 1:1, keeping text sharp.
pub fn letterbox(window: (u32, u32), video: (u32, u32)) -> Viewport {
    let (ww, wh) = (window.0 as f32, window.1 as f32);
    let (vw, vh) = (video.0 as f32, video.1 as f32);
    let scale = (ww / vw).min(wh / vh);
    let scale = if (scale - 1.0).abs() < 0.02 {
        1.0
    } else {
        scale
    };
    let (w, h) = ((vw * scale).round(), (vh * scale).round());
    Viewport {
        x: ((ww - w) / 2.0).floor(),
        y: ((wh - h) / 2.0).floor(),
        w,
        h,
    }
}
