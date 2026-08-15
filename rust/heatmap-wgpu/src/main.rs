#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

macro_rules! app_log {
    ($($argument:tt)*) => {
        crate::write_log(format_args!($($argument)*))
    };
}

mod tile_engine;

use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use assembly_core::{AssemblyCoordinateMap, AssemblyDocument, AssemblyEditor, AssemblyPlacement};
use bytemuck::{Pod, Zeroable};
use heatmap_core::{GenomeViewport, IntensityTile};
use hic_core::HicFile;
use tile_engine::{MatrixType, Normalization, TileEngine, TileResult, rendered_viewport_for};
use wgpu::util::DeviceExt;
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowAttributes, WindowId},
};

const INITIAL_SPAN_FRACTION: f64 = 0.42;
const INTERACTION_REFRESH_MS: u64 = 16;
const MIN_DISPLAYED_TEXTURE_SCALE: f64 = 0.45;
const REFRESH_EDGE_MARGIN_FRACTION: f64 = 0.12;

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn log_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("JuiceboxRust")
        .join("juicebox-rust.log")
}

fn write_log(arguments: std::fmt::Arguments<'_>) {
    let file = LOG_FILE.get_or_init(|| {
        let path = log_path();
        let file = path.parent().and_then(|parent| {
            std::fs::create_dir_all(parent).ok()?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
        });
        Mutex::new(file)
    });
    if let Ok(mut guard) = file.lock()
        && let Some(file) = guard.as_mut()
    {
        let _ = writeln!(file, "{arguments}");
    }
    #[cfg(debug_assertions)]
    eprintln!("{arguments}");
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ViewUniform {
    texture_rect: [f32; 4],
    color_range: [f32; 4],
    surface: [f32; 4],
    selection: [f32; 4],
}

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    texture: wgpu::Texture,
    texture_size: [u32; 2],
}

impl GpuState {
    async fn new(
        window: Arc<Window>,
        tile: &IntensityTile,
        force_fallback_adapter: bool,
    ) -> Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter,
            })
            .await
            .with_context(|| {
                if force_fallback_adapter {
                    "no compatible software/CPU adapter found"
                } else {
                    "no compatible hardware GPU adapter found"
                }
            })?;
        let adapter_info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("juicebox-rust-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await?;
        app_log!(
            "render adapter ready: mode={} name={} backend={:?} device_type={:?}",
            if force_fallback_adapter {
                "cpu-fallback"
            } else {
                "hardware"
            },
            adapter_info.name,
            adapter_info.backend,
            adapter_info.device_type
        );
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or(capabilities.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let texture = create_intensity_texture(&device, tile.width, tile.height);
        write_intensity_texture(&queue, &texture, tile);
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("non-filtering heatmap sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport uniform"),
            contents: bytemuck::bytes_of(&ViewUniform {
                texture_rect: [0.0, 0.0, 1.0, 1.0],
                color_range: [0.0, 1.0, 0.0, 0.0],
                surface: [1.0, 0.0, 0.0, 0.0],
                selection: [0.0; 4],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("heatmap bindings"),
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
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("heatmap bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform_buffer.as_entire_binding(),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("heatmap shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("heatmap pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("heatmap pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group,
            uniform_buffer,
            texture,
            texture_size: [tile.width, tile.height],
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    fn update_tile(&mut self, tile: &IntensityTile, dirty_rect: Option<[u32; 4]>) {
        if let Some(rect) = dirty_rect {
            // Streamed results contain just the changed rectangle, not an
            // entire 1024x1024 texture.  Comparing the rectangle dimensions
            // with the *texture* dimensions here silently discarded every
            // partial update after the first Block, so newly panned-in areas
            // could remain blank until a later full upload.
            if partial_upload_is_valid(self.texture_size, tile, rect) {
                write_intensity_texture_rect(
                    &self.queue,
                    &self.texture,
                    self.texture_size,
                    tile,
                    rect,
                );
            }
        } else if self.texture_size == [tile.width, tile.height] {
            write_intensity_texture(&self.queue, &self.texture, tile);
        }
    }

    fn render(
        &mut self,
        requested: GenomeViewport,
        displayed: GenomeViewport,
        color_range: [f32; 2],
        selected_range: Option<[u64; 2]>,
        pearson_colors: bool,
    ) -> Result<(), wgpu::SurfaceError> {
        let requested_bounds = requested.bounds_bp();
        let displayed_bounds = displayed.bounds_bp();
        let texture_rect = [
            ((requested_bounds[0] - displayed_bounds[0]) / displayed.span_bp) as f32,
            ((requested_bounds[1] - displayed_bounds[1]) / displayed.span_bp) as f32,
            (requested.span_bp / displayed.span_bp) as f32,
            (requested.span_bp / displayed.span_bp) as f32,
        ];
        let aspect = self.config.width as f32 / self.config.height.max(1) as f32;
        let uniform = ViewUniform {
            texture_rect,
            color_range: [
                color_range[0],
                color_range[1],
                f32::from(pearson_colors),
                0.0,
            ],
            surface: [
                aspect,
                self.config.width as f32,
                self.config.height as f32,
                0.0,
            ],
            selection: selected_range.map_or([0.0; 4], |range| {
                [
                    ((range[0] as f64 - requested_bounds[0]) / requested.span_bp) as f32,
                    ((range[1] as f64 - requested_bounds[0]) / requested.span_bp) as f32,
                    1.0,
                    0.0,
                ]
            }),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));
        let output = self.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("heatmap frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("heatmap pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}

fn cpu_fallback_requested(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| {
        let value = value.to_string_lossy();
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

async fn initialize_renderer(window: Arc<Window>, tile: &IntensityTile) -> Result<GpuState> {
    if cpu_fallback_requested(std::env::var_os("JUICEBOX_FORCE_CPU").as_deref()) {
        app_log!("CPU fallback forced by JUICEBOX_FORCE_CPU");
        return GpuState::new(window, tile, true).await;
    }

    match GpuState::new(Arc::clone(&window), tile, false).await {
        Ok(gpu) => Ok(gpu),
        Err(hardware_error) => {
            app_log!(
                "hardware GPU initialization failed: {hardware_error:#}; trying CPU/software fallback"
            );
            GpuState::new(window, tile, true).await.with_context(|| {
                format!(
                    "CPU/software fallback also failed after hardware error: {hardware_error:#}"
                )
            })
        }
    }
}

fn create_intensity_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("R32F intensity view"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_intensity_texture(queue: &wgpu::Queue, texture: &wgpu::Texture, tile: &IntensityTile) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(&tile.values),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(tile.width * 4),
            rows_per_image: Some(tile.height),
        },
        wgpu::Extent3d {
            width: tile.width,
            height: tile.height,
            depth_or_array_layers: 1,
        },
    );
}

fn write_intensity_texture_rect(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    texture_size: [u32; 2],
    tile: &IntensityTile,
    [x, y, width, height]: [u32; 4],
) {
    if !partial_upload_is_valid(texture_size, tile, [x, y, width, height]) {
        return;
    }
    // WebGPU requires buffer row pitches to be 256-byte aligned. Queue
    // uploads still validate that constraint on some backends, so pad each
    // R32F row instead of assuming `width * 4` is accepted.
    let unpadded_bytes_per_row = width * 4;
    let bytes_per_row = unpadded_bytes_per_row.div_ceil(256) * 256;
    let mut bytes = vec![0_u8; bytes_per_row as usize * height as usize];
    let source = bytemuck::cast_slice::<f32, u8>(&tile.values);
    for row in 0..height as usize {
        let source_start = row * unpadded_bytes_per_row as usize;
        let target_start = row * bytes_per_row as usize;
        bytes[target_start..target_start + unpadded_bytes_per_row as usize]
            .copy_from_slice(&source[source_start..source_start + unpadded_bytes_per_row as usize]);
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

fn partial_upload_is_valid(
    texture_size: [u32; 2],
    tile: &IntensityTile,
    [x, y, width, height]: [u32; 4],
) -> bool {
    width != 0
        && height != 0
        && [tile.width, tile.height] == [width, height]
        && x.checked_add(width)
            .is_some_and(|right| right <= texture_size[0])
        && y.checked_add(height)
            .is_some_and(|bottom| bottom <= texture_size[1])
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    engine: TileEngine,
    requested_viewport: GenomeViewport,
    displayed_viewport: GenomeViewport,
    initial_tile: Option<IntensityTile>,
    color_range: [f32; 2],
    auto_color_range: bool,
    latest_auto_color_max: f32,
    dragging: bool,
    last_cursor: Option<PhysicalPosition<f64>>,
    cursor: PhysicalPosition<f64>,
    base_title: String,
    request_pending: bool,
    in_flight_viewport: Option<GenomeViewport>,
    last_request_at: Instant,
    assembly_editor: Option<AssemblyEditor>,
    assembly_path: Option<PathBuf>,
    selected_scaffold: Option<AssemblyPlacement>,
    debris_anchor: Option<u64>,
    modifiers: winit::keyboard::ModifiersState,
    assembly_version: u64,
    normalization: Normalization,
    matrix_type: MatrixType,
}

impl App {
    fn new(
        engine: TileEngine,
        viewport: GenomeViewport,
        initial: TileResult,
        base_title: String,
        assembly: Option<AssemblyDocument>,
        assembly_path: Option<PathBuf>,
    ) -> Self {
        Self {
            window: None,
            gpu: None,
            engine,
            requested_viewport: viewport,
            displayed_viewport: initial.viewport,
            initial_tile: Some(initial.tile),
            color_range: [0.0, initial.color_max],
            auto_color_range: true,
            latest_auto_color_max: initial.color_max,
            dragging: false,
            last_cursor: None,
            cursor: PhysicalPosition::new(0.0, 0.0),
            base_title,
            request_pending: false,
            in_flight_viewport: None,
            last_request_at: Instant::now(),
            assembly_editor: assembly.map(AssemblyEditor::new),
            assembly_path,
            selected_scaffold: None,
            debris_anchor: None,
            modifiers: winit::keyboard::ModifiersState::empty(),
            assembly_version: initial.assembly_version,
            normalization: initial.normalization,
            matrix_type: initial.matrix_type,
        }
    }

    fn request_current(&mut self) {
        app_log!(
            "tile requested: generation={} center=({:.0},{:.0}) span={:.0}",
            self.requested_viewport.generation,
            self.requested_viewport.center_bp[0],
            self.requested_viewport.center_bp[1],
            self.requested_viewport.span_bp,
        );
        self.engine.request(self.requested_viewport);
        self.in_flight_viewport = Some(rendered_viewport_for(self.requested_viewport));
        self.request_pending = false;
        self.last_request_at = Instant::now();
    }

    fn schedule_request(&mut self) {
        if !viewport_needs_refresh(self.requested_viewport, self.displayed_viewport) {
            self.request_pending = false;
            return;
        }
        // A request which can only cover the literal visible rectangle is not
        // sufficient here.  Keeping it would leave the cursor at the edge of
        // that texture while the worker finishes an increasingly stale tile;
        // if the pointer then stops, no later pointer event is available to
        // advance the request.  The same edge margin used for the displayed
        // texture makes a request a valid prediction only while it still has
        // room for the next drag updates.  Once that margin is consumed, the
        // latest request replaces the old one (coalesced to 60 Hz).
        if self
            .in_flight_viewport
            .is_some_and(|viewport| viewport_can_serve(self.requested_viewport, viewport, true))
        {
            self.request_pending = false;
            return;
        }
        self.request_pending = true;
        if self.last_request_at.elapsed() >= Duration::from_millis(INTERACTION_REFRESH_MS) {
            self.request_current();
        }
    }

    /// Finish a drag with a request for the actual final viewport whenever the
    /// newest submitted work was merely an earlier overscan prediction.  While
    /// dragging that prediction is useful: it avoids replacing a worker job
    /// for every pointer event.  On release, however, the pointer is no longer
    /// going to generate another event which could advance the request.
    fn request_settled_viewport(&mut self) {
        let request_is_current = self
            .in_flight_viewport
            .is_some_and(|viewport| viewport.generation == self.requested_viewport.generation);
        if should_request_settled_viewport(
            self.requested_viewport,
            self.displayed_viewport,
            self.in_flight_viewport,
            self.request_pending,
        ) {
            app_log!(
                "tile settle requested: generation={} displayed={} in_flight_current={}",
                self.requested_viewport.generation,
                self.displayed_viewport.generation,
                request_is_current
            );
            self.request_current();
        }
    }

    fn consume_results(&mut self, window: &Window) {
        while let Some(result) = self.engine.try_result() {
            match result {
                Ok(result)
                    if result.viewport.generation >= self.displayed_viewport.generation
                        && result.viewport.generation <= self.requested_viewport.generation
                        && result.assembly_version == self.assembly_version
                        && result.normalization == self.normalization
                        && result.matrix_type == self.matrix_type =>
                {
                    // Streaming results share a generation.  Keep the
                    // in-flight coverage prediction until its final Block has
                    // arrived; otherwise the first partial upload makes the
                    // scheduler believe there is no current request and it
                    // can needlessly replace a useful progressive fill.
                    if result.complete
                        && self.in_flight_viewport.is_some_and(|viewport| {
                            viewport.generation == result.viewport.generation
                        })
                    {
                        self.in_flight_viewport = None;
                    }
                    if let Some(gpu) = self.gpu.as_mut() {
                        gpu.update_tile(&result.tile, result.dirty_rect);
                    }
                    self.displayed_viewport = result.viewport;
                    // Avoid visible colour-scale pulsing while individual
                    // Blocks progressively fill this same viewport.
                    if result.complete {
                        self.latest_auto_color_max = result.color_max;
                        if self.auto_color_range {
                            self.color_range[1] = result.color_max;
                        }
                    }
                    window.set_title(&format!(
                        "{} — {} — {} — {} bp — blocks {}/{}{} — {:.1} ms — cache {}/{} — color {}",
                        self.base_title,
                        result.normalization.label(),
                        result.matrix_type.label(),
                        result.resolution,
                        result.loaded_blocks,
                        result.total_blocks,
                        if result.complete { "" } else { " loading" },
                        result.elapsed_ms,
                        result.cache_hits,
                        result.cache_hits + result.cache_misses,
                        if self.auto_color_range {
                            "auto"
                        } else {
                            "manual"
                        },
                    ));
                    if result.complete {
                        app_log!(
                            "tile displayed: generation={} center=({:.0},{:.0}) span={:.0} resolution={} blocks={}/{} load={:.1} ms cache={}/{} upload={} bytes",
                            result.viewport.generation,
                            result.viewport.center_bp[0],
                            result.viewport.center_bp[1],
                            result.viewport.span_bp,
                            result.resolution,
                            result.loaded_blocks,
                            result.total_blocks,
                            result.elapsed_ms,
                            result.cache_hits,
                            result.cache_hits + result.cache_misses,
                            result.upload_bytes,
                        );
                    } else {
                        app_log!(
                            "tile progress: generation={} blocks={}/{} elapsed={:.1} ms upload={} bytes rect={:?}",
                            result.viewport.generation,
                            result.loaded_blocks,
                            result.total_blocks,
                            result.elapsed_ms,
                            result.upload_bytes,
                            result.dirty_rect,
                        );
                    }
                    // A completed tile can legitimately be older than the
                    // most recent cursor position: drag events are coalesced
                    // while the worker is reading/decompressing blocks.  Do
                    // not leave that newer viewport dependent on the old
                    // prediction forever.  Re-evaluate coverage *after* the
                    // texture is installed, and queue the next tile if this
                    // texture cannot keep an edge margin around it.
                    //
                    // Without this second check, `schedule_request` may have
                    // cleared `request_pending` because the then-in-flight
                    // overscan looked sufficient.  Once an older result wins
                    // the display, no new pointer event is required to reveal
                    // the stale state, so uncovered portions can remain dark.
                    if result.complete
                        && viewport_needs_refresh(self.requested_viewport, self.displayed_viewport)
                    {
                        app_log!(
                            "tile follow-up queued: requested generation={} is outside completed generation={} coverage",
                            self.requested_viewport.generation,
                            self.displayed_viewport.generation
                        );
                        self.schedule_request();
                    }
                    window.request_redraw();
                }
                Ok(result) => app_log!(
                    "tile result ignored: generation={} requested={} displayed={} assembly={} normalization={} matrix_type={}",
                    result.viewport.generation,
                    self.requested_viewport.generation,
                    self.displayed_viewport.generation,
                    result.assembly_version,
                    result.normalization.label(),
                    result.matrix_type.label(),
                ),
                Err(error) => {
                    self.in_flight_viewport = None;
                    app_log!("tile request failed: {error}");
                }
            }
        }
    }

    fn apply_assembly_change(&mut self) -> Result<()> {
        let editor = self
            .assembly_editor
            .as_ref()
            .context("no assembly is loaded")?;
        let map = editor.document().coordinate_map()?;
        self.assembly_version = editor.document().version;
        self.engine.update_assembly(map, self.assembly_version);
        self.requested_viewport.generation = self.requested_viewport.generation.wrapping_add(1);
        self.request_current();
        Ok(())
    }

    fn assembly_coordinate_at_cursor(&self, window: &Window) -> u64 {
        let anchor = cursor_in_square(self.cursor, window.inner_size());
        let bounds = self.requested_viewport.bounds_bp();
        (bounds[0] + anchor[0] * self.requested_viewport.span_bp)
            .clamp(0.0, self.requested_viewport.genome_length_bp - 1.0) as u64
    }

    fn select_or_move_scaffold(&mut self, window: &Window) -> Result<()> {
        let coordinate = self.assembly_coordinate_at_cursor(window);
        let target = self
            .assembly_editor
            .as_ref()
            .context("no assembly is loaded")?
            .document()
            .placement_at(coordinate)
            .context("cursor is outside the assembly layout")?;
        if self.modifiers.shift_key()
            && let Some(selected) = self.selected_scaffold
            && selected.scaffold_id != target.scaffold_id
        {
            self.assembly_editor
                .as_mut()
                .expect("assembly editor disappeared")
                .move_scaffold(
                    selected.superscaffold_index,
                    selected.scaffold_index,
                    target.superscaffold_index,
                    target.scaffold_index,
                )?;
            self.selected_scaffold = None;
            self.debris_anchor = None;
            self.apply_assembly_change()?;
        } else {
            self.selected_scaffold = Some(target);
            self.debris_anchor = None;
            window.set_title(&format!(
                "{} — selected scaffold {}{}",
                self.base_title,
                target.scaffold_id,
                if target.reversed { " (reversed)" } else { "" }
            ));
            window.request_redraw();
            app_log!(
                "assembly selected: scaffold={} superscaffold={} index={} reversed={}",
                target.scaffold_id,
                target.superscaffold_index,
                target.scaffold_index,
                target.reversed
            );
        }
        Ok(())
    }

    fn mark_or_extract_debris(&mut self, window: &Window) -> Result<()> {
        let selected = self.selected_scaffold.context("select a scaffold first")?;
        let coordinate = self.assembly_coordinate_at_cursor(window);
        if coordinate < selected.start || coordinate >= selected.end {
            anyhow::bail!("both debris endpoints must be inside the selected scaffold");
        }
        let Some(anchor) = self.debris_anchor.take() else {
            self.debris_anchor = Some(coordinate);
            window.set_title(&format!(
                "{} — debris start set; move cursor and press D again",
                self.base_title
            ));
            app_log!(
                "assembly debris start: scaffold={} coordinate={}",
                selected.scaffold_id,
                coordinate
            );
            return Ok(());
        };
        let assembly_start = anchor.min(coordinate) - selected.start;
        let assembly_end = anchor.max(coordinate) - selected.start;
        let length = selected.end - selected.start;
        let (source_start, source_end) = if selected.reversed {
            (length - assembly_end, length - assembly_start)
        } else {
            (assembly_start, assembly_end)
        };
        let ids = self
            .assembly_editor
            .as_mut()
            .context("no assembly is loaded")?
            .extract_debris(
                selected.superscaffold_index,
                selected.scaffold_index,
                source_start,
                source_end,
            )?;
        self.selected_scaffold = None;
        self.apply_assembly_change()?;
        app_log!(
            "assembly debris extracted: source_scaffold={} fragments={:?} cuts=[{}, {})",
            selected.scaffold_id,
            ids,
            source_start,
            source_end
        );
        Ok(())
    }

    fn split_selected_group(&mut self) -> Result<()> {
        let selected = self.selected_scaffold.context("select a scaffold first")?;
        self.assembly_editor
            .as_mut()
            .context("no assembly is loaded")?
            .split_superscaffold_after(selected.superscaffold_index, selected.scaffold_index)?;
        self.selected_scaffold = None;
        self.debris_anchor = None;
        self.apply_assembly_change()?;
        app_log!(
            "assembly group split: superscaffold={} after_index={}",
            selected.superscaffold_index,
            selected.scaffold_index
        );
        Ok(())
    }

    fn merge_selected_group_with_next(&mut self) -> Result<()> {
        let selected = self.selected_scaffold.context("select a scaffold first")?;
        self.assembly_editor
            .as_mut()
            .context("no assembly is loaded")?
            .merge_superscaffold_with_next(selected.superscaffold_index)?;
        self.selected_scaffold = None;
        self.debris_anchor = None;
        self.apply_assembly_change()?;
        app_log!(
            "assembly groups merged: first_superscaffold={}",
            selected.superscaffold_index
        );
        Ok(())
    }

    fn save_modified_assembly(&self) -> Result<PathBuf> {
        let editor = self
            .assembly_editor
            .as_ref()
            .context("no assembly is loaded")?;
        let source = self
            .assembly_path
            .as_ref()
            .context("assembly path is unavailable")?;
        let output = source.with_extension("modified.assembly");
        editor.document().save(&output)?;
        Ok(output)
    }

    fn open_modified_assembly(&mut self, path: PathBuf) -> Result<()> {
        let document = AssemblyDocument::open(&path)?;
        let map = document.coordinate_map()?;
        if map.total_length() != self.requested_viewport.genome_length_bp as u64 {
            anyhow::bail!(
                "assembly length {} does not match current matrix length {:.0}",
                map.total_length(),
                self.requested_viewport.genome_length_bp
            );
        }
        self.assembly_version = self.assembly_version.wrapping_add(1).max(1);
        self.engine.update_assembly(map, self.assembly_version);
        self.assembly_editor = Some(AssemblyEditor::new(document));
        self.assembly_path = Some(path.clone());
        self.selected_scaffold = None;
        self.debris_anchor = None;
        self.requested_viewport.generation = self.requested_viewport.generation.wrapping_add(1);
        self.request_current();
        app_log!("assembly opened: {}", path.display());
        Ok(())
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title(&self.base_title)
                        .with_inner_size(PhysicalSize::new(1000, 820)),
                )
                .expect("window creation failed"),
        );
        let tile = self
            .initial_tile
            .take()
            .expect("initial tile already consumed");
        self.gpu = Some(
            pollster::block_on(initialize_renderer(Arc::clone(&window), &tile))
                .expect("hardware and CPU render initialization both failed"),
        );
        self.window = Some(window);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        self.consume_results(&window);
        if self.request_pending
            && self.last_request_at.elapsed() >= Duration::from_millis(INTERACTION_REFRESH_MS)
        {
            self.request_current();
        }
        window.request_redraw();
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(INTERACTION_REFRESH_MS),
        ));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
                window.request_redraw();
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.dragging = state == ElementState::Pressed;
                if self.dragging {
                    self.last_cursor = Some(self.cursor);
                } else {
                    self.last_cursor = None;
                    self.request_settled_viewport();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                if let Err(error) = self.select_or_move_scaffold(&window) {
                    app_log!("assembly selection failed: {error:#}");
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                if self.dragging {
                    if let Some(previous) = self.last_cursor {
                        let side = f64::from(
                            window
                                .inner_size()
                                .width
                                .min(window.inner_size().height)
                                .max(1),
                        );
                        self.requested_viewport.pan_fraction([
                            -(position.x - previous.x) / side,
                            -(position.y - previous.y) / side,
                        ]);
                        self.schedule_request();
                        window.request_redraw();
                    }
                    self.last_cursor = Some(position);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(position) => position.y / 80.0,
                };
                let anchor = cursor_in_square(self.cursor, window.inner_size());
                self.requested_viewport
                    .zoom_at(1.18_f64.powf(amount), anchor);
                self.schedule_request();
                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                match event.physical_key {
                    PhysicalKey::Code(KeyCode::ArrowUp | KeyCode::Equal) => {
                        self.color_range[1] *= 1.12;
                        self.auto_color_range = false;
                    }
                    PhysicalKey::Code(KeyCode::ArrowDown | KeyCode::Minus) => {
                        self.color_range[1] = (self.color_range[1] / 1.12).max(f32::EPSILON);
                        self.auto_color_range = false;
                    }
                    PhysicalKey::Code(KeyCode::KeyA) => {
                        self.auto_color_range = true;
                        self.color_range[1] = self.latest_auto_color_max;
                    }
                    PhysicalKey::Code(KeyCode::KeyR) => {
                        self.requested_viewport.reset(INITIAL_SPAN_FRACTION);
                        self.request_current();
                    }
                    PhysicalKey::Code(KeyCode::KeyN) => {
                        self.normalization = self.normalization.next();
                        self.engine.update_normalization(self.normalization);
                        self.requested_viewport.generation =
                            self.requested_viewport.generation.wrapping_add(1);
                        self.request_current();
                        app_log!("normalization changed: {}", self.normalization.label());
                    }
                    PhysicalKey::Code(KeyCode::KeyM) => {
                        self.matrix_type = self.matrix_type.next();
                        self.engine.update_matrix_type(self.matrix_type);
                        self.requested_viewport.generation =
                            self.requested_viewport.generation.wrapping_add(1);
                        self.request_current();
                        app_log!("matrix type changed: {}", self.matrix_type.label());
                    }
                    PhysicalKey::Code(KeyCode::KeyI) => {
                        if let (Some(editor), Some(selected)) =
                            (self.assembly_editor.as_mut(), self.selected_scaffold)
                        {
                            match editor.toggle_scaffold_orientation(
                                selected.superscaffold_index,
                                selected.scaffold_index,
                            ) {
                                Ok(()) => {
                                    self.selected_scaffold = None;
                                    if let Err(error) = self.apply_assembly_change() {
                                        app_log!("assembly inversion failed: {error:#}");
                                    }
                                }
                                Err(error) => app_log!("assembly inversion failed: {error}"),
                            }
                        }
                    }
                    PhysicalKey::Code(KeyCode::KeyD) => {
                        if let Err(error) = self.mark_or_extract_debris(&window) {
                            self.debris_anchor = None;
                            app_log!("assembly debris extraction failed: {error:#}");
                        }
                    }
                    PhysicalKey::Code(KeyCode::KeyB) => {
                        if let Err(error) = self.split_selected_group() {
                            app_log!("assembly group split failed: {error:#}");
                        }
                    }
                    PhysicalKey::Code(KeyCode::KeyJ) => {
                        if let Err(error) = self.merge_selected_group_with_next() {
                            app_log!("assembly group merge failed: {error:#}");
                        }
                    }
                    PhysicalKey::Code(KeyCode::KeyZ) if self.modifiers.control_key() => {
                        if self
                            .assembly_editor
                            .as_mut()
                            .is_some_and(AssemblyEditor::undo)
                            && let Err(error) = self.apply_assembly_change()
                        {
                            app_log!("assembly undo failed: {error:#}");
                        }
                    }
                    PhysicalKey::Code(KeyCode::KeyY) if self.modifiers.control_key() => {
                        if self
                            .assembly_editor
                            .as_mut()
                            .is_some_and(AssemblyEditor::redo)
                            && let Err(error) = self.apply_assembly_change()
                        {
                            app_log!("assembly redo failed: {error:#}");
                        }
                    }
                    PhysicalKey::Code(KeyCode::KeyS) if self.modifiers.control_key() => {
                        match self.save_modified_assembly() {
                            Ok(path) => app_log!("assembly saved: {}", path.display()),
                            Err(error) => app_log!("assembly save failed: {error:#}"),
                        }
                    }
                    PhysicalKey::Code(KeyCode::KeyO) if self.modifiers.control_key() => {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("Juicebox assembly", &["assembly"])
                            .pick_file()
                            && let Err(error) = self.open_modified_assembly(path)
                        {
                            app_log!("assembly open failed: {error:#}");
                        }
                    }
                    _ => {}
                }
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                if let Some(gpu) = self.gpu.as_mut() {
                    match gpu.render(
                        self.requested_viewport,
                        self.displayed_viewport,
                        self.color_range,
                        self.selected_scaffold
                            .map(|placement| [placement.start, placement.end]),
                        self.matrix_type == MatrixType::Pearson,
                    ) {
                        Ok(()) => {}
                        Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                            let size = window.inner_size();
                            gpu.resize(size.width, size.height);
                        }
                        Err(wgpu::SurfaceError::OutOfMemory) => event_loop.exit(),
                        Err(wgpu::SurfaceError::Timeout | wgpu::SurfaceError::Other) => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn cursor_in_square(cursor: PhysicalPosition<f64>, size: PhysicalSize<u32>) -> [f64; 2] {
    let width = f64::from(size.width.max(1));
    let height = f64::from(size.height.max(1));
    let side = width.min(height);
    let left = (width - side) * 0.5;
    let top = (height - side) * 0.5;
    [
        ((cursor.x - left) / side).clamp(0.0, 1.0),
        ((cursor.y - top) / side).clamp(0.0, 1.0),
    ]
}

fn viewport_needs_refresh(requested: GenomeViewport, displayed: GenomeViewport) -> bool {
    !viewport_can_serve(requested, displayed, true)
}

fn viewport_can_serve(
    requested: GenomeViewport,
    displayed: GenomeViewport,
    reserve_edge_margin: bool,
) -> bool {
    let requested_bounds = requested.bounds_bp();
    let displayed_bounds = displayed.bounds_bp();
    let margin = if reserve_edge_margin {
        requested.span_bp * REFRESH_EDGE_MARGIN_FRACTION
    } else {
        0.0
    };
    // A viewport at a chromosome edge cannot possibly retain a margin on the
    // outside of the matrix.  Before this clamp, every completed overscanned
    // tile at an edge failed the reserve check (for example, `0 - margin >=
    // 0`), re-requested the identical tile forever, and could starve a later
    // drag request.  Only reserve pixels that can actually be reached.
    let extended_requested_bounds = [
        (requested_bounds[0] - margin).max(0.0),
        (requested_bounds[1] - margin).max(0.0),
        (requested_bounds[2] + margin).min(requested.genome_length_bp),
        (requested_bounds[3] + margin).min(requested.genome_length_bp),
    ];
    let covered = extended_requested_bounds[0] >= displayed_bounds[0]
        && extended_requested_bounds[1] >= displayed_bounds[1]
        && extended_requested_bounds[2] <= displayed_bounds[2]
        && extended_requested_bounds[3] <= displayed_bounds[3];
    let texture_scale = requested.span_bp / displayed.span_bp.max(1.0);
    covered && texture_scale >= MIN_DISPLAYED_TEXTURE_SCALE
}

fn should_request_settled_viewport(
    requested: GenomeViewport,
    displayed: GenomeViewport,
    in_flight: Option<GenomeViewport>,
    request_pending: bool,
) -> bool {
    request_pending
        || (in_flight.is_none_or(|viewport| viewport.generation != requested.generation)
            && requested.generation != displayed.generation)
}

fn main() {
    std::panic::set_hook(Box::new(|panic| {
        app_log!("panic: {panic}");
    }));
    if let Err(error) = run() {
        if error.downcast_ref::<UserCancelled>().is_some() {
            return;
        }
        app_log!("fatal error: {error:#}");
        rfd::MessageDialog::new()
            .set_title("Juicebox Rust")
            .set_description(format!("{error:#}\n\nLog: {}", log_path().display()))
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
}

#[derive(Debug)]
struct UserCancelled;

impl std::fmt::Display for UserCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("file selection was cancelled")
    }
}

impl std::error::Error for UserCancelled {}

fn run() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let hic_path = arguments
        .next()
        .map(PathBuf::from)
        .or_else(|| {
            rfd::FileDialog::new()
                .add_filter("Hi-C map", &["hic"])
                .pick_file()
        })
        .ok_or(UserCancelled)?;
    let matrix_key = arguments
        .next()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "1_1".to_owned());
    let assembly_path = arguments.next().map(PathBuf::from);
    let file = HicFile::open(&hic_path)?;
    let matrix = file.read_matrix(&matrix_key)?;
    let chromosome = file
        .header
        .chromosomes
        .get(matrix.chromosome_1 as usize)
        .context("matrix chromosome is outside the header dictionary")?;
    let resolved_assembly_path = resolve_assembly_path(&hic_path, assembly_path.as_deref());
    let assembly = load_assembly(&resolved_assembly_path, assembly_path.is_some())?;
    let assembly_map = assembly
        .as_ref()
        .map(AssemblyCoordinateMap::new)
        .transpose()?;
    if let Some(map) = &assembly_map
        && map.total_length() != chromosome.length
    {
        anyhow::bail!(
            "assembly length {} does not match matrix chromosome length {}",
            map.total_length(),
            chromosome.length
        );
    }
    let viewport = GenomeViewport::new(
        assembly_map
            .as_ref()
            .map_or(chromosome.length, AssemblyCoordinateMap::total_length),
        INITIAL_SPAN_FRACTION,
    );
    let engine = TileEngine::spawn(hic_path.clone(), matrix_key.clone(), assembly_map)?;
    engine.request(viewport);
    let initial = loop {
        if let Some(result) = engine.try_result() {
            let result = result.map_err(anyhow::Error::msg)?;
            // The UI deliberately streams subsequent viewport updates, but
            // the first frame has no previous texture to preserve.  Wait for
            // a complete initial raster so launch never opens as a partly
            // populated matrix merely because one compressed Block happened
            // to finish first.
            if result.complete {
                break result;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let base_title = format!(
        "Juicebox Rust — {} — {} — REAL DYNAMIC HIC{}",
        hic_path.display(),
        matrix_key,
        assembly
            .as_ref()
            .map(|document| format!(
                " — ASSEMBLY {} scaffolds / {} superscaffolds",
                document.scaffolds.len(),
                document.superscaffolds.len()
            ))
            .unwrap_or_default(),
    );
    app_log!(
        "dynamic viewport ready: span={:.0} bp resolution={} blocks={} load={:.1} ms",
        viewport.span_bp,
        initial.resolution,
        initial.visible_blocks,
        initial.elapsed_ms
    );
    let event_loop = EventLoop::new()?;
    event_loop.run_app(&mut App::new(
        engine,
        viewport,
        initial,
        base_title,
        assembly,
        resolved_assembly_path
            .is_file()
            .then_some(resolved_assembly_path),
    ))?;
    Ok(())
}

fn resolve_assembly_path(
    hic_path: &std::path::Path,
    explicit_path: Option<&std::path::Path>,
) -> PathBuf {
    explicit_path
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| hic_path.with_extension("assembly"))
}

fn load_assembly(
    assembly_path: &std::path::Path,
    explicit: bool,
) -> Result<Option<AssemblyDocument>> {
    if !assembly_path.is_file() {
        if explicit {
            anyhow::bail!("assembly file does not exist: {}", assembly_path.display());
        }
        return Ok(None);
    }
    AssemblyDocument::open(assembly_path)
        .map(Some)
        .with_context(|| format!("failed to load {}", assembly_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn parses_cpu_fallback_environment_switch() {
        assert!(!cpu_fallback_requested(None));
        assert!(!cpu_fallback_requested(Some(OsStr::new(""))));
        assert!(!cpu_fallback_requested(Some(OsStr::new("0"))));
        assert!(!cpu_fallback_requested(Some(OsStr::new("FALSE"))));
        assert!(cpu_fallback_requested(Some(OsStr::new("1"))));
        assert!(cpu_fallback_requested(Some(OsStr::new("yes"))));
    }

    #[test]
    fn maps_cursor_into_centered_square() {
        let size = PhysicalSize::new(1000, 800);
        assert_eq!(
            cursor_in_square(PhysicalPosition::new(100.0, 0.0), size),
            [0.0, 0.0]
        );
        assert_eq!(
            cursor_in_square(PhysicalPosition::new(500.0, 400.0), size),
            [0.5, 0.5]
        );
        assert_eq!(
            cursor_in_square(PhysicalPosition::new(900.0, 800.0), size),
            [1.0, 1.0]
        );
    }

    #[test]
    fn accepts_a_streamed_sub_rectangle_for_upload() {
        let tile = IntensityTile::new(
            heatmap_core::TileKey {
                dataset: 1,
                matrix_type: 0,
                normalization: 0,
                assembly_version: 0,
                resolution: 1,
                x: 0,
                y: 0,
            },
            3,
            2,
            vec![0.0; 6],
        );
        assert!(partial_upload_is_valid(
            [1024, 1024],
            &tile,
            [731, 741, 3, 2]
        ));
        assert!(!partial_upload_is_valid(
            [1024, 1024],
            &tile,
            [1023, 741, 3, 2]
        ));
    }

    #[test]
    fn reuses_overscanned_texture_until_coverage_or_detail_runs_out() {
        let requested = GenomeViewport::new(1_000, 0.4);
        let mut displayed = requested;
        displayed.span_bp = 600.0;
        assert!(viewport_can_serve(requested, displayed, false));

        let mut panned = requested;
        panned.center_bp = [650.0, 500.0];
        assert!(!viewport_can_serve(panned, displayed, false));

        let mut zoomed = requested;
        zoomed.span_bp = 200.0;
        assert!(!viewport_can_serve(zoomed, displayed, false));
    }

    #[test]
    fn refreshes_before_the_visible_view_reaches_the_texture_edge() {
        let mut displayed = GenomeViewport::new(1_000, 0.6);
        displayed.center_bp = [500.0, 500.0];
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.center_bp = [585.0, 500.0];

        assert!(viewport_can_serve(requested, displayed, false));
        assert!(viewport_needs_refresh(requested, displayed));
    }

    #[test]
    fn edge_viewport_does_not_request_an_impossible_outside_margin() {
        let mut displayed = GenomeViewport::new(1_000, 0.6);
        displayed.center_bp = [300.0, 300.0];
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.center_bp = [200.0, 200.0];

        // Both viewports are centered at the left/top genome edge after
        // clamping.  The texture already covers everything that exists on
        // that side, so asking it to reserve negative genome coordinates
        // would produce an endless identical request loop.
        assert!(!viewport_needs_refresh(requested, displayed));
    }

    #[test]
    fn one_in_flight_overscan_can_serve_multiple_drag_updates() {
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.center_bp = [600.0, 500.0];
        let in_flight = rendered_viewport_for(requested);

        requested.center_bp = [665.0, 500.0];
        assert!(viewport_can_serve(requested, in_flight, false));
        requested.center_bp = [710.0, 500.0];
        assert!(!viewport_can_serve(requested, in_flight, false));
    }

    #[test]
    fn in_flight_prediction_is_replaced_before_the_texture_edge_is_reached() {
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.center_bp = [600.0, 500.0];
        let in_flight = rendered_viewport_for(requested);

        // The requested view remains drawable from the old overscan, but has
        // consumed the edge reserve.  A continuing drag must replace the
        // pending request with this newer position instead of waiting until
        // pixels are already outside of the texture.
        requested.center_bp = [665.0, 500.0];
        assert!(viewport_can_serve(requested, in_flight, false));
        assert!(!viewport_can_serve(requested, in_flight, true));
    }

    #[test]
    fn completed_stale_tile_requests_follow_up_for_current_drag_position() {
        let mut completed = GenomeViewport::new(1_000, 0.6);
        completed.center_bp = [300.0, 500.0];

        let mut current = GenomeViewport::new(1_000, 0.4);
        current.center_bp = [700.0, 500.0];
        current.generation = completed.generation + 4;

        // This models a result that started before the final coalesced drag
        // position.  Rendering it is useful, but it must not be the terminal
        // state: the currently requested right edge is no longer covered.
        assert!(!viewport_can_serve(current, completed, false));
        assert!(viewport_needs_refresh(current, completed));
    }

    #[test]
    fn drag_release_replaces_an_older_overscan_prediction() {
        let mut displayed = GenomeViewport::new(1_000, 0.6);
        displayed.generation = 3;
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.generation = 8;
        let mut old_prediction = rendered_viewport_for(requested);
        old_prediction.generation = 5;

        // The old prediction may still cover the visible area, which is fine
        // during a gesture.  When the gesture stops it must not suppress the
        // request for generation 8.
        assert!(viewport_can_serve(requested, old_prediction, false));
        assert!(should_request_settled_viewport(
            requested,
            displayed,
            Some(old_prediction),
            false
        ));
    }

    #[test]
    fn drag_release_keeps_the_current_in_flight_request() {
        let mut displayed = GenomeViewport::new(1_000, 0.6);
        displayed.generation = 3;
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.generation = 8;
        let current_prediction = rendered_viewport_for(requested);

        assert!(!should_request_settled_viewport(
            requested,
            displayed,
            Some(current_prediction),
            false
        ));
    }
}
