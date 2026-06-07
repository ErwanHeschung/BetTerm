//! wgpu renderer: composites background image, cell backgrounds, glyphs, cursor
//! and the animated RGB border.

use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::config::Config;
use crate::font::{FontAtlas, ATLAS_SIZE};
use crate::terminal::{xterm_palette, Color, Term};

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GlyphInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CircleInstance {
    center: [f32; 2],
    radius: f32,
    color: [f32; 4],
}

/// A macOS-style window control button.
#[derive(Clone, Copy, PartialEq)]
pub enum Control {
    Close,
    Minimize,
    Maximize,
}

#[derive(Clone, Copy)]
pub struct Button {
    pub control: Control,
    pub cx: f32,
    pub cy: f32,
    pub r: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    screen: [f32; 2],
    _pad: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BgParams {
    color: [f32; 4],
    screen: [f32; 2],
    img_size: [f32; 2],
    opacity: f32,
    has_image: f32,
    radius: f32,
    _pad: f32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BorderParams {
    screen: [f32; 2],
    width: f32,
    radius: f32,
    density: f32,
    phase: f32,
    _pad: [f32; 2],
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surf_config: wgpu::SurfaceConfiguration,
    pub size: winit::dpi::PhysicalSize<u32>,

    globals_buf: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,

    quad_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    bg_pipeline: wgpu::RenderPipeline,
    border_pipeline: wgpu::RenderPipeline,
    circle_pipeline: wgpu::RenderPipeline,

    atlas_tex: wgpu::Texture,
    atlas_bg: wgpu::BindGroup,
    atlas_version: u64,

    bg_buf: wgpu::Buffer,
    bg_bind: wgpu::BindGroup,
    bg_img_size: [f32; 2],

    border_buf: wgpu::Buffer,
    border_bind: wgpu::BindGroup,

    palette: [[u8; 3]; 256],
    cfg: Config,
    /// Inset between window edge and the text grid (left/right/bottom).
    pub pad: f32,
    /// Height of the top title bar holding the window controls.
    pub title_h: f32,
    /// GPU adapter name, surfaced for the startup banner.
    pub gpu_name: String,
}

impl Renderer {
    pub fn new(window: Arc<Window>, cfg: &Config, _atlas: &FontAtlas) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let size = winit::dpi::PhysicalSize::new(size.width.max(1), size.height.max(1));

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // GL is the only backend that composites a transparent window cleanly on
            // Windows: Vulkan ghosts a stale rectangle on resize (gfx-rs/wgpu #5374)
            // and DX12's HWND swapchain ignores per-pixel alpha (no see-through).
            backends: wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| anyhow::anyhow!("no suitable GPU adapter: {e}"))?;

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            }))?;

        let info = adapter.get_info();
        let gpu_name = info.name.clone();
        crate::logx::log(&format!(
            "adapter: {} ({:?}, backend {:?})",
            info.name, info.device_type, info.backend
        ));

        let caps = surface.get_capabilities(&adapter);
        // Prefer a non-sRGB format so our sRGB 0..1 colors map 1:1.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        crate::logx::log(&format!(
            "surface format: {:?}, alpha modes: {:?}, max_tex_dim: {}",
            format,
            caps.alpha_modes,
            device.limits().max_texture_dimension_2d
        ));
        let surf_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::Fifo,
            // Prefer a transparency-capable composite mode for see-through.
            alpha_mode: caps
                .alpha_modes
                .iter()
                .copied()
                .find(|m| {
                    matches!(
                        m,
                        wgpu::CompositeAlphaMode::PreMultiplied
                            | wgpu::CompositeAlphaMode::PostMultiplied
                    )
                })
                .unwrap_or(caps.alpha_modes[0]),
            view_formats: vec![],
            desired_maximum_frame_latency: 1, // keep the swapchain in step on resize
        };
        surface.configure(&device, &surf_config);

        // --- Globals (screen size) shared by quad + glyph pipelines ---
        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals_bg"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        // --- Glyph atlas texture ---
        let atlas_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas_tex.create_view(&Default::default());
        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tex_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let atlas_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas_bg"),
            layout: &tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&linear_sampler),
                },
            ],
        });

        // --- Background (image + opacity) ---
        let (img_view, has_image, img_dims) = load_bg_image(&device, &queue, cfg);
        let bg_radius = if cfg.border.enabled {
            cfg.border.radius
        } else {
            0.0
        };
        let bg_img_size = [img_dims.0 as f32, img_dims.1 as f32];
        let bg_params = BgParams {
            color: [
                cfg.background_color[0],
                cfg.background_color[1],
                cfg.background_color[2],
                1.0,
            ],
            screen: [size.width as f32, size.height as f32],
            img_size: bg_img_size,
            opacity: cfg.background_opacity,
            has_image: if has_image { 1.0 } else { 0.0 },
            radius: bg_radius,
            _pad: 0.0,
        };
        let bg_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("bg_params"),
            contents: bytemuck::bytes_of(&bg_params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let img_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let bg_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bg_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bg_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg_bind"),
            layout: &bg_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: bg_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&img_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&img_sampler),
                },
            ],
        });

        // --- Border ---
        let border_params = BorderParams {
            screen: [size.width as f32, size.height as f32],
            width: cfg.border.width,
            radius: cfg.border.radius,
            density: cfg.border.angle_density,
            phase: 0.0,
            _pad: [0.0, 0.0],
        };
        let border_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("border_params"),
            contents: bytemuck::bytes_of(&border_params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let border_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("border_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let border_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("border_bind"),
            layout: &border_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: border_buf.as_entire_binding(),
            }],
        });

        // --- Pipelines ---
        // Premultiplied alpha so the transparent (see-through) surface composites
        // correctly over the desktop.
        let blend = Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING);
        let quad_pipeline = make_pipeline(
            &device,
            "quad",
            include_str!("shaders/quad.wgsl"),
            &[&globals_layout],
            &[quad_instance_layout()],
            format,
            blend,
        );
        let glyph_pipeline = make_pipeline(
            &device,
            "glyph",
            include_str!("shaders/glyph.wgsl"),
            &[&globals_layout, &tex_layout],
            &[glyph_instance_layout()],
            format,
            blend,
        );
        let bg_pipeline = make_pipeline(
            &device,
            "background",
            include_str!("shaders/background.wgsl"),
            &[&bg_layout],
            &[],
            format,
            None,
        );
        let circle_pipeline = make_pipeline(
            &device,
            "circle",
            include_str!("shaders/circle.wgsl"),
            &[&globals_layout],
            &[circle_instance_layout()],
            format,
            blend,
        );
        let border_pipeline = make_pipeline(
            &device,
            "border",
            include_str!("shaders/border.wgsl"),
            &[&border_layout],
            &[],
            format,
            blend,
        );

        let pad = if cfg.border.enabled {
            (cfg.border.width + 6.0).max(cfg.border.radius + 2.0)
        } else {
            6.0
        };
        let title_h = (pad + 26.0).max(32.0);

        Ok(Self {
            surface,
            device,
            queue,
            surf_config,
            size,
            globals_buf,
            globals_bg,
            quad_pipeline,
            glyph_pipeline,
            bg_pipeline,
            border_pipeline,
            circle_pipeline,
            atlas_tex,
            atlas_bg,
            atlas_version: 0,
            bg_buf,
            bg_bind,
            bg_img_size,
            border_buf,
            border_bind,
            palette: xterm_palette(),
            cfg: cfg.clone(),
            pad,
            title_h,
            gpu_name,
        })
    }

    pub fn resize(&mut self, new: winit::dpi::PhysicalSize<u32>) {
        if new.width == 0 || new.height == 0 {
            return;
        }
        self.size = new;
        self.surf_config.width = new.width;
        self.surf_config.height = new.height;
        self.surface.configure(&self.device, &self.surf_config);
    }

    /// Cols/rows that fit the current window for a given font.
    pub fn grid_size(&self, atlas: &FontAtlas) -> (u16, u16) {
        let usable_w = (self.size.width as f32 - 2.0 * self.pad).max(atlas.cell_w);
        let usable_h = (self.size.height as f32 - self.title_h - self.pad).max(atlas.cell_h);
        let cols = (usable_w / atlas.cell_w).floor().max(1.0) as u16;
        let rows = (usable_h / atlas.cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }

    /// macOS-style window controls, laid out in the top-right corner.
    pub fn buttons(&self) -> [Button; 3] {
        let r = 7.0;
        let gap = 26.0;
        let cy = self.title_h * 0.5;
        let right = self.size.width as f32 - self.pad - 12.0;
        // Windows order, left -> right: minimize, maximize, close (close rightmost).
        [
            Button {
                control: Control::Minimize,
                cx: right - 2.0 * gap,
                cy,
                r,
            },
            Button {
                control: Control::Maximize,
                cx: right - gap,
                cy,
                r,
            },
            Button {
                control: Control::Close,
                cx: right,
                cy,
                r,
            },
        ]
    }

    fn resolve_rgb(&self, c: Color, default: [u8; 3]) -> [f32; 3] {
        norm_rgb(match c {
            Color::Default => default,
            Color::Indexed(i) => self.palette[i as usize],
            Color::Rgb(r, g, b) => [r, g, b],
        })
    }

    pub fn render(
        &mut self,
        term: &Term,
        atlas: &mut FontAtlas,
        time: f32,
        selection: Option<(usize, usize)>,
    ) {
        // Build instances; this may rasterize new glyphs into the atlas.
        let mut quads: Vec<QuadInstance> = Vec::new();
        let mut glyphs: Vec<GlyphInstance> = Vec::new();

        let fg_def = self.cfg.foreground;
        let bg_def = self.cfg.background;

        for y in 0..term.rows {
            for x in 0..term.cols {
                let cell = term.cells[y * term.cols + x];
                let cx = self.pad + x as f32 * atlas.cell_w;
                let cy = self.title_h + y as f32 * atlas.cell_h;

                let fg_rgb = self.resolve_rgb(cell.fg, fg_def);
                let bg_is_default = cell.bg == Color::Default;
                let bg_rgb = self.resolve_rgb(cell.bg, bg_def);

                let (draw_bg, bg_color, text_color);
                if cell.inverse {
                    draw_bg = true;
                    bg_color = fg_rgb;
                    text_color = if bg_is_default {
                        norm_rgb(bg_def)
                    } else {
                        bg_rgb
                    };
                } else {
                    draw_bg = !bg_is_default;
                    bg_color = bg_rgb;
                    text_color = fg_rgb;
                }

                if draw_bg {
                    quads.push(QuadInstance {
                        pos: [cx, cy],
                        size: [atlas.cell_w, atlas.cell_h],
                        color: [bg_color[0], bg_color[1], bg_color[2], 1.0],
                    });
                }

                // Selection highlight.
                if let Some((s, e)) = selection {
                    let idx = y * term.cols + x;
                    if idx >= s && idx <= e {
                        quads.push(QuadInstance {
                            pos: [cx, cy],
                            size: [atlas.cell_w, atlas.cell_h],
                            color: [0.4, 0.6, 1.0, 0.35],
                        });
                    }
                }

                if cell.ch != ' ' && cell.ch != '\0' {
                    if let Some(g) = atlas.glyph(cell.ch) {
                        // Faux-bold: nudge the color brighter (no bold font file).
                        let tc = if cell.bold {
                            [
                                (text_color[0] * 1.25).min(1.0),
                                (text_color[1] * 1.25).min(1.0),
                                (text_color[2] * 1.25).min(1.0),
                            ]
                        } else {
                            text_color
                        };
                        glyphs.push(GlyphInstance {
                            pos: [cx + g.left, cy + g.top],
                            size: [g.w as f32, g.h as f32],
                            uv_pos: [
                                g.x as f32 / ATLAS_SIZE as f32,
                                g.y as f32 / ATLAS_SIZE as f32,
                            ],
                            uv_size: [
                                g.w as f32 / ATLAS_SIZE as f32,
                                g.h as f32 / ATLAS_SIZE as f32,
                            ],
                            color: [tc[0], tc[1], tc[2], 1.0],
                        });
                    }
                }
            }
        }

        if term.cursor_visible && term.cursor_x < term.cols && term.cursor_y < term.rows {
            let cx = self.pad + term.cursor_x as f32 * atlas.cell_w;
            let cy = self.title_h + term.cursor_y as f32 * atlas.cell_h;
            let [r, g, b] = norm_rgb(fg_def);
            quads.push(QuadInstance {
                pos: [cx, cy],
                size: [atlas.cell_w, atlas.cell_h],
                color: [r, g, b, 0.5],
            });
        }

        // Upload the atlas if it grew.
        if atlas.version != self.atlas_version {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &atlas.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(ATLAS_SIZE),
                    rows_per_image: Some(ATLAS_SIZE),
                },
                wgpu::Extent3d {
                    width: ATLAS_SIZE,
                    height: ATLAS_SIZE,
                    depth_or_array_layers: 1,
                },
            );
            self.atlas_version = atlas.version;
        }

        // Update uniforms.
        let globals = Globals {
            screen: [self.size.width as f32, self.size.height as f32],
            _pad: [0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        let border_params = BorderParams {
            screen: [self.size.width as f32, self.size.height as f32],
            width: self.cfg.border.width,
            radius: self.cfg.border.radius,
            density: self.cfg.border.angle_density,
            phase: if self.cfg.border.speed > 0.0 {
                time / self.cfg.border.speed
            } else {
                0.0
            },
            _pad: [0.0, 0.0],
        };
        self.queue
            .write_buffer(&self.border_buf, 0, bytemuck::bytes_of(&border_params));

        let bg_params = BgParams {
            color: [
                self.cfg.background_color[0],
                self.cfg.background_color[1],
                self.cfg.background_color[2],
                1.0,
            ],
            screen: [self.size.width as f32, self.size.height as f32],
            img_size: self.bg_img_size,
            opacity: self.cfg.background_opacity,
            has_image: if self.bg_img_size[0] > 1.0 { 1.0 } else { 0.0 },
            radius: if self.cfg.border.enabled {
                self.cfg.border.radius
            } else {
                0.0
            },
            _pad: 0.0,
        };
        self.queue
            .write_buffer(&self.bg_buf, 0, bytemuck::bytes_of(&bg_params));

        // Instance buffers, rebuilt each frame.
        let quad_buf = (!quads.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("quads"),
                    contents: bytemuck::cast_slice(&quads),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });
        let glyph_buf = (!glyphs.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("glyphs"),
                    contents: bytemuck::cast_slice(&glyphs),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });

        // Window control buttons.
        let circles: Vec<CircleInstance> = self
            .buttons()
            .iter()
            .map(|b| {
                let color = match b.control {
                    Control::Close => [0.96, 0.30, 0.27, 1.0],
                    Control::Minimize => [0.98, 0.80, 0.25, 1.0],
                    Control::Maximize => [0.25, 0.80, 0.35, 1.0],
                };
                CircleInstance {
                    center: [b.cx, b.cy],
                    radius: b.r,
                    color,
                }
            })
            .collect();
        let circle_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("circles"),
                contents: bytemuck::cast_slice(&circles),
                usage: wgpu::BufferUsages::VERTEX,
            });

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            // Surface needs reconfiguring (resized/lost): redo it with the current
            // config and skip this frame; the next redraw will draw cleanly.
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.surf_config);
                return;
            }
            // Transient: skip this frame and try again next redraw.
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => return,
            wgpu::CurrentSurfaceTexture::Validation => {
                crate::logx::log("get_current_texture validation error");
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("enc") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Transparent clear so uncovered pixels show the desktop.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.0,
                            b: 0.0,
                            a: 0.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            // Composited bottom-to-top: background, cell backgrounds, glyphs,
            // border, then window controls.
            pass.set_pipeline(&self.bg_pipeline);
            pass.set_bind_group(0, &self.bg_bind, &[]);
            pass.draw(0..3, 0..1);

            if let Some(buf) = &quad_buf {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..quads.len() as u32);
            }

            if let Some(buf) = &glyph_buf {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.globals_bg, &[]);
                pass.set_bind_group(1, &self.atlas_bg, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..glyphs.len() as u32);
            }

            if self.cfg.border.enabled {
                pass.set_pipeline(&self.border_pipeline);
                pass.set_bind_group(0, &self.border_bind, &[]);
                pass.draw(0..3, 0..1);
            }

            pass.set_pipeline(&self.circle_pipeline);
            pass.set_bind_group(0, &self.globals_bg, &[]);
            pass.set_vertex_buffer(0, circle_buf.slice(..));
            pass.draw(0..6, 0..circles.len() as u32);
        }
        self.queue.submit([enc.finish()]);
        frame.present();
    }
}

/// Map an 8-bit RGB triple to normalized 0..1 floats for the shaders.
#[inline]
fn norm_rgb([r, g, b]: [u8; 3]) -> [f32; 3] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0]
}

fn quad_instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2, // pos
        1 => Float32x2, // size
        2 => Float32x4, // color
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<QuadInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

fn glyph_instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        0 => Float32x2, // pos
        1 => Float32x2, // size
        2 => Float32x2, // uv_pos
        3 => Float32x2, // uv_size
        4 => Float32x4, // color
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<GlyphInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

fn circle_instance_layout() -> wgpu::VertexBufferLayout<'static> {
    const ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2, // center
        1 => Float32,   // radius
        2 => Float32x4, // color
    ];
    wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<CircleInstance>() as u64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &ATTRS,
    }
}

#[allow(clippy::too_many_arguments)]
fn make_pipeline(
    device: &wgpu::Device,
    label: &str,
    src: &str,
    bind_layouts: &[&wgpu::BindGroupLayout],
    buffers: &[wgpu::VertexBufferLayout],
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(src.into()),
    });
    let bind_layouts: Vec<Option<&wgpu::BindGroupLayout>> =
        bind_layouts.iter().map(|l| Some(*l)).collect();
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &bind_layouts,
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            buffers,
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Decode the wallpaper into an RGBA texture, or a 1x1 white fallback.
fn load_bg_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    cfg: &Config,
) -> (wgpu::TextureView, bool, (u32, u32)) {
    let max_dim = device.limits().max_texture_dimension_2d;
    let decoded = cfg.background_image.as_ref().and_then(|p| {
        crate::logx::log(&format!("background_image configured: {p}"));
        match image::open(p) {
            Ok(img) => {
                let mut rgba = img.to_rgba8();
                // Downscale if the wallpaper exceeds the GPU's max texture size.
                if rgba.width() > max_dim || rgba.height() > max_dim {
                    let scale = max_dim as f32 / rgba.width().max(rgba.height()) as f32;
                    let nw = ((rgba.width() as f32 * scale) as u32).max(1);
                    let nh = ((rgba.height() as f32 * scale) as u32).max(1);
                    crate::logx::log(&format!(
                        "image {}x{} exceeds max {max_dim}, downscaling to {nw}x{nh}",
                        rgba.width(),
                        rgba.height()
                    ));
                    rgba = image::imageops::resize(
                        &rgba,
                        nw,
                        nh,
                        image::imageops::FilterType::Triangle,
                    );
                }
                crate::logx::log(&format!(
                    "background image decoded: {}x{}",
                    rgba.width(),
                    rgba.height()
                ));
                Some((rgba.width(), rgba.height(), rgba.into_raw()))
            }
            Err(e) => {
                crate::logx::log(&format!("could not load background image {p}: {e}"));
                None
            }
        }
    });

    let (w, h, data, has) = match decoded {
        Some((w, h, d)) => (w, h, d, true),
        None => (1, 1, vec![255u8, 255, 255, 255], false),
    };
    crate::logx::log(&format!(
        "background has_image={has}, opacity={}",
        cfg.background_opacity
    ));

    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bg_image"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * w),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    (tex.create_view(&Default::default()), has, (w, h))
}
