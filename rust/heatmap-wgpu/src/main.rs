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
use hic_core::{Chromosome, HicFile};
use session_core::{LegacySessionState, read_legacy_session};
use tile_engine::{
    DatasetLaunch, MatrixType, Normalization, TileEngine, TileResult, rendered_viewport_for,
};
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
const INITIAL_WINDOW_SIDE: u32 = 900;
const INTERACTION_REFRESH_MS: u64 = 16;
const INTERACTION_SETTLE_MS: u64 = 48;
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
        color_mode: f32,
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
            color_range: [color_range[0], color_range[1], color_mode, 0.0],
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
    /// Coordinates represented by the texture currently sampled by the GPU.
    /// This advances for every streamed Block so the just-arrived pixels are
    /// drawn in their correct genomic position.
    displayed_viewport: GenomeViewport,
    /// The newest viewport whose texture has received every required Block.
    /// Keep this separate from `displayed_viewport`: a streamed first Block is
    /// drawable, but does not yet prove that a later pan is covered.
    completed_viewport: GenomeViewport,
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
    last_submitted_generation: u64,
    last_request_at: Instant,
    last_view_change_at: Instant,
    assembly_editor: Option<AssemblyEditor>,
    assembly_path: Option<PathBuf>,
    selected_scaffold: Option<AssemblyPlacement>,
    debris_anchor: Option<u64>,
    modifiers: winit::keyboard::ModifiersState,
    assembly_version: u64,
    normalization: Normalization,
    control_normalization: Normalization,
    matrix_type: MatrixType,
    control_available: bool,
    resolution_hint: Option<u32>,
}

impl App {
    #[allow(
        clippy::too_many_arguments,
        reason = "application construction keeps the immutable launch/session state explicit"
    )]
    fn new(
        engine: TileEngine,
        viewport: GenomeViewport,
        initial: TileResult,
        base_title: String,
        assembly: Option<AssemblyDocument>,
        assembly_path: Option<PathBuf>,
        control_available: bool,
        resolution_hint: Option<u32>,
        initial_color_range: Option<[f32; 2]>,
        normalization: Normalization,
        control_normalization: Normalization,
        matrix_type: MatrixType,
    ) -> Self {
        Self {
            window: None,
            gpu: None,
            engine,
            requested_viewport: viewport,
            displayed_viewport: initial.viewport,
            completed_viewport: initial.viewport,
            initial_tile: Some(initial.tile),
            color_range: initial_color_range.unwrap_or([0.0, initial.color_max]),
            auto_color_range: initial_color_range.is_none(),
            latest_auto_color_max: initial.color_max,
            dragging: false,
            last_cursor: None,
            cursor: PhysicalPosition::new(0.0, 0.0),
            base_title,
            request_pending: false,
            in_flight_viewport: None,
            last_submitted_generation: viewport.generation,
            last_request_at: Instant::now(),
            last_view_change_at: Instant::now(),
            assembly_editor: assembly.map(AssemblyEditor::new),
            assembly_path,
            selected_scaffold: None,
            debris_anchor: None,
            modifiers: winit::keyboard::ModifiersState::empty(),
            assembly_version: initial.assembly_version,
            normalization,
            control_normalization,
            matrix_type,
            control_available,
            resolution_hint,
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
        self.last_submitted_generation = self.requested_viewport.generation;
        self.request_pending = false;
        self.last_request_at = Instant::now();
    }

    fn active_normalization(&self) -> Normalization {
        if self.matrix_type.uses_control() {
            self.control_normalization
        } else {
            self.normalization
        }
    }

    fn normalization_title(&self) -> String {
        if self.matrix_type.is_comparison() {
            format!(
                "obs {} / ctrl {}",
                self.normalization.label(),
                self.control_normalization.label()
            )
        } else {
            self.active_normalization().label().to_owned()
        }
    }

    fn schedule_request(&mut self) {
        // Overscan keeps the picture continuous while a drag is under way,
        // but it must not turn into a reason to stop asking the worker for
        // data.  Previously a small pan remained inside the old 1.5x texture
        // (and its edge reserve), so no request was ever submitted for the
        // position now under the cursor.  The view could therefore appear to
        // move over a frozen matrix and the two prefetch rings were never
        // warmed for the newly exposed direction.
        //
        // A TileEngine has a single latest-request slot: submitting here does
        // not build a backlog.  Coalescing at INTERACTION_REFRESH_MS gives the
        // worker the current viewport at most once per 16 ms while preserving
        // the old texture until the first streamed result can replace it.
        if should_submit_interaction_viewport(
            self.requested_viewport,
            self.last_submitted_generation,
        ) {
            self.request_pending = true;
            if self.last_request_at.elapsed() >= Duration::from_millis(INTERACTION_REFRESH_MS) {
                self.request_current();
            }
            return;
        }
        if !viewport_needs_refresh(self.requested_viewport, self.completed_viewport) {
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

    /// Finish a drag with a request for the actual final viewport.  While
    /// dragging, a current overscan job is a useful prediction: it avoids
    /// replacing a worker job for every pointer event.  Once the pointer is
    /// released, though, retaining that prediction leaves a subtle hole:
    /// its pixels may cover the screen, yet its cache warm-up and streamed
    /// raster still belong to an intermediate pointer position.  There is no
    /// later input event to make the worker move its center to where the user
    /// stopped.  Give the stopped view its own generation so newly exposed
    /// blocks are always rasterized and displayed after a pan.
    fn request_settled_viewport(&mut self) {
        let request_is_current = self
            .in_flight_viewport
            .is_some_and(|viewport| viewport.generation == self.requested_viewport.generation);
        let final_view_is_complete =
            self.completed_viewport.generation == self.requested_viewport.generation;
        if !final_view_is_complete
            && should_request_settled_viewport(
                self.requested_viewport,
                self.completed_viewport,
                self.in_flight_viewport,
                self.request_pending,
            )
        {
            let interaction_generation = self.requested_viewport.generation;
            self.requested_viewport.generation = self.requested_viewport.generation.wrapping_add(1);
            app_log!(
                "tile settle requested: interaction_generation={} final_generation={} displayed={} in_flight_current={}",
                interaction_generation,
                self.requested_viewport.generation,
                self.displayed_viewport.generation,
                request_is_current
            );
            self.request_current();
        }
    }

    /// Submit the most recent interaction position after pointer input has
    /// paused briefly.  Winit normally delivers a left-button release, but a
    /// release outside the window or an aggressively coalesced event stream
    /// must not leave the final viewport served only by an older overscan
    /// prediction.  Re-centering the worker here also warms the two prefetch
    /// rings around the place where the user actually stopped.
    fn request_idle_viewport(&mut self) {
        if self.last_view_change_at.elapsed() < Duration::from_millis(INTERACTION_SETTLE_MS)
            || !should_request_idle_viewport(
                self.requested_viewport,
                self.completed_viewport,
                self.last_submitted_generation,
            )
        {
            return;
        }
        app_log!(
            "tile idle settle requested: generation={} completed={}",
            self.requested_viewport.generation,
            self.completed_viewport.generation
        );
        self.request_current();
    }

    fn consume_results(&mut self, window: &Window) {
        while let Some(result) = self.engine.try_result() {
            // The first streamed update is always a whole texture; later
            // updates only carry a dirty rectangle.  A superseded whole
            // texture which still covers the cursor is a useful bridge while
            // the newest request decompresses.  Once installed, accept only
            // its own follow-up rectangles.  This prevents an old partial
            // upload from being written into a newer texture, which made a
            // pan appear to stop filling after the first block.
            let result_is_displayable = result.as_ref().ok().is_some_and(|tile| {
                should_display_tile_result(
                    self.requested_viewport,
                    self.displayed_viewport,
                    tile.viewport,
                    tile.dirty_rect,
                )
            });
            match result {
                Ok(result)
                    if result_is_displayable
                        && result.assembly_version == self.assembly_version
                        && result.normalization == self.active_normalization()
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
                        self.completed_viewport = result.viewport;
                        self.latest_auto_color_max = result.color_max;
                        if self.auto_color_range {
                            self.color_range[1] = result.color_max;
                        }
                    }
                    window.set_title(&format!(
                        "{} — {} — {} — {} bp — blocks {}/{}{} — {:.1} ms — cache {}/{} — color {}",
                        self.base_title,
                        self.normalization_title(),
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
                    // Re-evaluate coverage after the exact-current texture is
                    // installed.  This keeps moving the prefetch rings if a
                    // coalesced pointer update advanced again while this tile
                    // was being decompressed.
                    if result.complete
                        && viewport_needs_refresh(self.requested_viewport, self.completed_viewport)
                    {
                        app_log!(
                            "tile follow-up queued: requested generation={} is outside completed generation={} coverage",
                            self.requested_viewport.generation,
                            self.completed_viewport.generation
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
        // An assembly edit changes the meaning of every screen coordinate,
        // even when all required source Blocks are already cached.  Forget
        // the previous completion/in-flight coverage before requesting the
        // new version so no later scheduler decision can treat the old
        // arrangement as a valid refill for the vacated insertion area.
        self.in_flight_viewport = None;
        self.request_pending = false;
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
        if let Some(selected) = self.selected_scaffold
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
            app_log!(
                "assembly moved: scaffold={} from superscaffold={} index={} to superscaffold={} before_index={}",
                selected.scaffold_id,
                selected.superscaffold_index,
                selected.scaffold_index,
                target.superscaffold_index,
                target.scaffold_index
            );
            self.selected_scaffold = None;
            self.debris_anchor = None;
            self.apply_assembly_change()?;
        } else {
            self.selected_scaffold = Some(target);
            self.debris_anchor = None;
            window.set_title(&format!(
                "{} — selected scaffold {}{}; right-click another scaffold to insert before it",
                self.base_title,
                target.scaffold_id,
                if target.reversed { " (reversed)" } else { "" }
            ));
            window.request_redraw();
            app_log!(
                "assembly selected: scaffold={} superscaffold={} index={} reversed={} — right-click another scaffold to insert before it",
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
                        .with_inner_size(PhysicalSize::new(
                            INITIAL_WINDOW_SIDE,
                            INITIAL_WINDOW_SIDE,
                        )),
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
        self.request_idle_viewport();
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
                        self.last_view_change_at = Instant::now();
                        self.schedule_request();
                        window.request_redraw();
                    }
                    self.last_cursor = Some(position);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.resolution_hint.take().is_some() {
                    self.engine.update_resolution_hint(None);
                    app_log!("session resolution lock released; automatic LOD enabled");
                }
                let amount = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    MouseScrollDelta::PixelDelta(position) => position.y / 80.0,
                };
                let anchor = cursor_in_square(self.cursor, window.inner_size());
                self.requested_viewport
                    .zoom_at(1.18_f64.powf(amount), anchor);
                self.last_view_change_at = Instant::now();
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
                        let update_control = normalization_key_targets_control(
                            self.matrix_type,
                            self.modifiers.shift_key(),
                        );
                        if update_control {
                            self.control_normalization = self.control_normalization.next();
                            self.engine
                                .update_control_normalization(self.control_normalization);
                        } else {
                            self.normalization = self.normalization.next();
                            self.engine.update_normalization(self.normalization);
                        }
                        self.requested_viewport.generation =
                            self.requested_viewport.generation.wrapping_add(1);
                        self.request_current();
                        app_log!(
                            "{} normalization changed: {}",
                            if update_control {
                                "control"
                            } else {
                                "observed"
                            },
                            if update_control {
                                self.control_normalization.label()
                            } else {
                                self.normalization.label()
                            }
                        );
                    }
                    PhysicalKey::Code(KeyCode::KeyM) => {
                        self.matrix_type = self.matrix_type.next(self.control_available);
                        if matches!(
                            self.matrix_type,
                            MatrixType::NormSquared | MatrixType::NormSquaredVs
                        ) && self.normalization == Normalization::None
                        {
                            self.normalization = Normalization::Kr;
                            self.engine.update_normalization(self.normalization);
                        }
                        if matches!(
                            self.matrix_type,
                            MatrixType::ControlNormSquared | MatrixType::NormSquaredVs
                        ) && self.control_normalization == Normalization::None
                        {
                            self.control_normalization = Normalization::Kr;
                            self.engine
                                .update_control_normalization(self.control_normalization);
                        }
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
                        matches!(
                            self.matrix_type,
                            MatrixType::Pearson
                                | MatrixType::ControlPearson
                                | MatrixType::PearsonVs
                        )
                        .then_some(1.0)
                        .or_else(|| self.matrix_type.uses_log_ratio_color_scale().then_some(2.0))
                        .unwrap_or(0.0),
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

fn should_display_tile_result(
    requested: GenomeViewport,
    displayed: GenomeViewport,
    result: GenomeViewport,
    dirty_rect: Option<[u32; 4]>,
) -> bool {
    if result.generation == requested.generation {
        return true;
    }
    let covers_requested = viewport_can_serve(requested, result, false);
    covers_requested && (result.generation == displayed.generation || dirty_rect.is_none())
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
        (requested_bounds[2] + margin).min(requested.axis_lengths_bp[0]),
        (requested_bounds[3] + margin).min(requested.axis_lengths_bp[1]),
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
    _in_flight: Option<GenomeViewport>,
    request_pending: bool,
) -> bool {
    // The result of an in-flight overscan request is useful while dragging,
    // but it is never the terminal request for a released gesture: it may
    // have been centered at an earlier point and can only warm cache rings
    // around that earlier point.  Submit one final coalesced generation for
    // the exact stopped viewport.
    request_pending || requested.generation != displayed.generation
}

fn should_request_idle_viewport(
    requested: GenomeViewport,
    completed: GenomeViewport,
    last_submitted_generation: u64,
) -> bool {
    requested.generation != completed.generation
        && requested.generation != last_submitted_generation
}

/// A visible view can be covered by an older overscanned texture and still
/// need a fresh worker request: that request both fills newly exposed genomic
/// pixels at the stopped position and moves the two prefetch rings in the
/// drag direction.  The engine owns one latest-request slot, so this remains
/// coalesced to the 16 ms interaction cadence rather than forming a queue.
fn should_submit_interaction_viewport(
    requested: GenomeViewport,
    last_submitted_generation: u64,
) -> bool {
    requested.generation != last_submitted_generation
}

fn normalization_key_targets_control(matrix_type: MatrixType, shift: bool) -> bool {
    matrix_type.uses_control() || (matrix_type.is_comparison() && shift)
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

struct LaunchConfiguration {
    hic_path: PathBuf,
    matrix_key: String,
    assembly_path: Option<PathBuf>,
    control_path: Option<PathBuf>,
    control_matrix_key: Option<String>,
    viewport: Option<GenomeViewport>,
    normalization: Normalization,
    matrix_type: MatrixType,
    resolution_hint: Option<u32>,
    color_range: Option<[f32; 2]>,
    session_id: Option<String>,
    discover_adjacent_assembly: bool,
    observed_transpose_axes: bool,
    control_transpose_axes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MatrixAxisLookup {
    matrix_key: String,
    x_index: u32,
    y_index: u32,
}

fn matrix_lookup_for_axis_names(
    chromosomes: &[Chromosome],
    x_name: &str,
    y_name: &str,
) -> Result<MatrixAxisLookup> {
    let find = |name: &str| {
        chromosomes
            .iter()
            .find(|chromosome| chromosome.name.eq_ignore_ascii_case(name))
            .with_context(|| format!("chromosome {name:?} is absent from the HIC header"))
    };
    let x = find(x_name)?;
    let y = find(y_name)?;
    Ok(MatrixAxisLookup {
        matrix_key: format!("{}_{}", x.index.min(y.index), x.index.max(y.index)),
        x_index: x.index,
        y_index: y.index,
    })
}

fn transpose_for_matrix_axes(lookup: &MatrixAxisLookup, matrix: &hic_core::Matrix) -> Result<bool> {
    let transpose_axes =
        lookup.x_index == matrix.chromosome_2 && lookup.y_index == matrix.chromosome_1;
    if (lookup.x_index == matrix.chromosome_1 && lookup.y_index == matrix.chromosome_2)
        || transpose_axes
    {
        Ok(transpose_axes)
    } else {
        anyhow::bail!(
            "requested axes {}_{} do not match matrix chromosomes {}_{}",
            lookup.x_index,
            lookup.y_index,
            matrix.chromosome_1,
            matrix.chromosome_2
        )
    }
}

fn dataset_launch_for_axes(
    path: PathBuf,
    file: &HicFile,
    x_name: &str,
    y_name: &str,
) -> Result<DatasetLaunch> {
    let lookup = matrix_lookup_for_axis_names(&file.header.chromosomes, x_name, y_name)
        .with_context(|| format!("failed to resolve axes in {}", path.display()))?;
    let matrix = file.read_matrix(&lookup.matrix_key)?;
    let transpose_axes = transpose_for_matrix_axes(&lookup, &matrix)
        .with_context(|| format!("invalid matrix axes in {}", path.display()))?;
    Ok(DatasetLaunch {
        path,
        matrix_key: lookup.matrix_key,
        transpose_axes,
    })
}

fn run() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let first_path = arguments
        .next()
        .map(PathBuf::from)
        .or_else(|| {
            rfd::FileDialog::new()
                .add_filter("Hi-C map", &["hic"])
                .pick_file()
        })
        .ok_or(UserCancelled)?;
    let mut launch = if first_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("xml"))
    {
        let selected_path = arguments
            .next()
            .map(|value| value.to_string_lossy().into_owned());
        launch_from_legacy_session(&first_path, selected_path.as_deref())?
    } else {
        LaunchConfiguration {
            hic_path: first_path,
            matrix_key: arguments
                .next()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| "1_1".to_owned()),
            assembly_path: arguments.next().map(PathBuf::from),
            control_path: arguments.next().map(PathBuf::from),
            control_matrix_key: None,
            viewport: None,
            normalization: Normalization::None,
            matrix_type: MatrixType::Observed,
            resolution_hint: None,
            color_range: None,
            session_id: None,
            discover_adjacent_assembly: true,
            observed_transpose_axes: false,
            control_transpose_axes: false,
        }
    };
    let hic_path = launch.hic_path.clone();
    let matrix_key = launch.matrix_key.clone();
    let assembly_path = launch.assembly_path.clone();
    let control_path = launch.control_path.clone();
    let file = HicFile::open(&hic_path)?;
    let matrix = file.read_matrix(&matrix_key)?;
    if let Some(control_path) = control_path.as_ref()
        && launch.control_matrix_key.is_none()
    {
        let chromosome_x = file
            .header
            .chromosomes
            .get(matrix.chromosome_1 as usize)
            .context("matrix chromosome 1 is outside the header dictionary")?;
        let chromosome_y = file
            .header
            .chromosomes
            .get(matrix.chromosome_2 as usize)
            .context("matrix chromosome 2 is outside the header dictionary")?;
        let control_file = HicFile::open(control_path)?;
        let control_launch = dataset_launch_for_axes(
            control_path.clone(),
            &control_file,
            &chromosome_x.name,
            &chromosome_y.name,
        )?;
        launch.control_matrix_key = Some(control_launch.matrix_key);
        launch.control_transpose_axes = control_launch.transpose_axes;
    }
    let chromosome = file
        .header
        .chromosomes
        .get(matrix.chromosome_1 as usize)
        .context("matrix chromosome is outside the header dictionary")?;
    let resolved_assembly_path = assembly_path.clone().or_else(|| {
        launch
            .discover_adjacent_assembly
            .then(|| hic_path.with_extension("assembly"))
    });
    let assembly = match resolved_assembly_path.as_deref() {
        Some(path) => load_assembly(path, assembly_path.is_some())?,
        None => None,
    };
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
    let genome_length = assembly_map
        .as_ref()
        .map_or(chromosome.length, AssemblyCoordinateMap::total_length);
    let viewport = launch.viewport.take().map_or_else(
        || GenomeViewport::new(genome_length, INITIAL_SPAN_FRACTION),
        |mut viewport| {
            if assembly_map.is_some() {
                viewport.set_axis_lengths([genome_length, genome_length]);
            }
            viewport
        },
    );
    let control_available = control_path.is_some();
    let engine = TileEngine::spawn(
        DatasetLaunch {
            path: hic_path.clone(),
            matrix_key: matrix_key.clone(),
            transpose_axes: launch.observed_transpose_axes,
        },
        assembly_map,
        control_path.clone().map(|path| DatasetLaunch {
            path,
            matrix_key: launch
                .control_matrix_key
                .clone()
                .expect("control matrix key was not resolved"),
            transpose_axes: launch.control_transpose_axes,
        }),
    )?;
    engine.update_normalization(launch.normalization);
    engine.update_control_normalization(launch.normalization);
    engine.update_matrix_type(launch.matrix_type);
    engine.update_resolution_hint(launch.resolution_hint);
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
        "Juicebox Rust — {} — {} — REAL DYNAMIC HIC{}{}",
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
        control_path
            .as_ref()
            .map(|path| format!(" — CONTROL {}", path.display()))
            .unwrap_or_default(),
    );
    if let Some(session_id) = &launch.session_id {
        app_log!(
            "legacy session restored: id={:?} mode={} normalization={} resolution={:?} tracks_restored=false",
            session_id,
            launch.matrix_type.label(),
            launch.normalization.label(),
            launch.resolution_hint
        );
    }
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
        resolved_assembly_path.filter(|path| path.is_file()),
        control_available,
        launch.resolution_hint,
        launch.color_range,
        launch.normalization,
        launch.normalization,
        launch.matrix_type,
    ))?;
    Ok(())
}

fn launch_from_legacy_session(
    path: &std::path::Path,
    selected_path: Option<&str>,
) -> Result<LaunchConfiguration> {
    let states = read_legacy_session(path)
        .with_context(|| format!("failed to parse legacy session {}", path.display()))?;
    let state = if let Some(selected_path) = selected_path {
        states
            .iter()
            .find(|state| state.id == selected_path || state.map_path == selected_path)
            .with_context(|| {
                let available = states
                    .iter()
                    .map(|state| state.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("legacy session has no state {selected_path:?}; available: {available}")
            })?
    } else {
        let state = states
            .first()
            .context("legacy session contains no saved state")?;
        if states.len() > 1 {
            app_log!(
                "legacy session contains {} states; using first {:?}; pass SelectedPath as the second argument to choose another",
                states.len(),
                state.id
            );
        }
        state
    };
    launch_from_session_state(path, state)
}

fn launch_from_session_state(
    session_path: &std::path::Path,
    state: &LegacySessionState,
) -> Result<LaunchConfiguration> {
    if !state.unit.eq_ignore_ascii_case("BP") {
        anyhow::bail!(
            "legacy session unit {} is not supported; Rust currently requires BP",
            state.unit
        );
    }
    if state.map_urls.len() != 1 || state.control_urls.len() > 1 {
        anyhow::bail!(
            "legacy session uses {} main and {} control maps; multi-map summation is not implemented",
            state.map_urls.len(),
            state.control_urls.len()
        );
    }
    let base = session_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let resolve = |value: &str| {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            base.join(path)
        }
    };
    let hic_path = resolve(&state.map_urls[0]);
    let file = HicFile::open(&hic_path)?;
    let observed_launch = dataset_launch_for_axes(
        hic_path.clone(),
        &file,
        &state.x_chromosome,
        &state.y_chromosome,
    )?;
    let lookup = matrix_lookup_for_axis_names(
        &file.header.chromosomes,
        &state.x_chromosome,
        &state.y_chromosome,
    )?;
    let x = lookup.x_index;
    let y = lookup.y_index;
    let matrix_key = observed_launch.matrix_key.clone();
    let matrix = file.read_matrix(&matrix_key)?;
    let chromosome_x = file
        .header
        .chromosomes
        .iter()
        .find(|chromosome| chromosome.index == x)
        .context("session X chromosome index is outside the header dictionary")?;
    let chromosome_y = file
        .header
        .chromosomes
        .iter()
        .find(|chromosome| chromosome.index == y)
        .context("session Y chromosome index is outside the header dictionary")?;
    let axis_lengths = [chromosome_x.length, chromosome_y.length];
    let genome_length = axis_lengths[0].max(axis_lengths[1]) as f64;
    let visible_bins = f64::from(INITIAL_WINDOW_SIDE) / state.scale_factor;
    let maximum_span = axis_lengths[0].min(axis_lengths[1]) as f64;
    let span_bp = (visible_bins * f64::from(state.bin_size)).clamp(1.0, maximum_span.max(1.0));
    let mut viewport = GenomeViewport::new(genome_length as u64, INITIAL_SPAN_FRACTION);
    viewport.set_axis_lengths(axis_lengths);
    viewport.span_bp = span_bp;
    viewport.center_bp = [
        (state.x_origin_bins + visible_bins * 0.5) * f64::from(state.bin_size),
        (state.y_origin_bins + visible_bins * 0.5) * f64::from(state.bin_size),
    ];
    viewport.clamp_center();
    let matrix_type = MatrixType::from_java_name(&state.display_option)
        .with_context(|| format!("unsupported legacy MatrixType {}", state.display_option))?;
    if x != y
        && (matrix_type.needs_expected() || matrix_type.is_pearson() || matrix_type.is_vs_display())
    {
        anyhow::bail!(
            "legacy MatrixType {} is intrachromosomal and cannot restore axes {}_{}",
            state.display_option,
            state.x_chromosome,
            state.y_chromosome
        );
    }
    let normalization = Normalization::from_label(&state.normalization)
        .with_context(|| format!("unsupported legacy normalization {}", state.normalization))?;
    let color_scale = if state.color_scale_factor.is_finite()
        && state.color_scale_factor > 0.0
        && state.upper_color.is_finite()
    {
        Some([
            (state.lower_color / state.color_scale_factor) as f32,
            (state.upper_color / state.color_scale_factor) as f32,
        ])
    } else {
        None
    };
    if !matrix
        .zooms
        .iter()
        .any(|zoom| zoom.bin_size == state.bin_size && zoom.unit == hic_core::MatrixUnit::BasePairs)
    {
        anyhow::bail!(
            "session resolution {} is absent from matrix {}",
            state.bin_size,
            matrix_key
        );
    }
    let control_launch = state
        .control_urls
        .first()
        .map(|value| {
            let path = resolve(value);
            let file = HicFile::open(&path)?;
            dataset_launch_for_axes(path, &file, &state.x_chromosome, &state.y_chromosome)
        })
        .transpose()?;
    Ok(LaunchConfiguration {
        hic_path: observed_launch.path,
        matrix_key,
        assembly_path: None,
        control_path: control_launch.as_ref().map(|dataset| dataset.path.clone()),
        control_matrix_key: control_launch
            .as_ref()
            .map(|dataset| dataset.matrix_key.clone()),
        viewport: Some(viewport),
        normalization,
        matrix_type,
        resolution_hint: Some(state.bin_size),
        color_range: color_scale,
        session_id: Some(state.id.clone()),
        discover_adjacent_assembly: false,
        observed_transpose_axes: observed_launch.transpose_axes,
        control_transpose_axes: control_launch
            .as_ref()
            .is_some_and(|dataset| dataset.transpose_axes),
    })
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

    fn request_complete_tile(
        engine: &TileEngine,
        viewport: &mut GenomeViewport,
        matrix_type: MatrixType,
    ) -> (TileResult, Vec<u32>) {
        engine.update_matrix_type(matrix_type);
        viewport.generation = viewport.generation.wrapping_add(1);
        engine.request(*viewport);
        let started = Instant::now();
        let mut raster =
            vec![0.0_f32; tile_engine::OUTPUT_SIZE as usize * tile_engine::OUTPUT_SIZE as usize];
        loop {
            if let Some(result) = engine.try_result() {
                let result = result.expect("real-data tile request failed");
                if result.viewport.generation == viewport.generation
                    && result.matrix_type == matrix_type
                {
                    if let Some([x, y, width, height]) = result.dirty_rect {
                        for row in 0..height {
                            let source = (row * width) as usize;
                            let target = ((y + row) * tile_engine::OUTPUT_SIZE + x) as usize;
                            raster[target..target + width as usize].copy_from_slice(
                                &result.tile.values[source..source + width as usize],
                            );
                        }
                    } else {
                        raster.copy_from_slice(&result.tile.values);
                    }
                    if result.complete {
                        let bits = raster.iter().map(|value| value.to_bits()).collect();
                        return (result, bits);
                    }
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "timed out waiting for {matrix_type:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

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
    fn comparison_normalization_shortcuts_target_each_dataset_explicitly() {
        assert!(!normalization_key_targets_control(MatrixType::Vs, false));
        assert!(normalization_key_targets_control(MatrixType::Vs, true));
        assert!(normalization_key_targets_control(
            MatrixType::Control,
            false
        ));
        assert!(!normalization_key_targets_control(
            MatrixType::Observed,
            true
        ));
    }

    #[test]
    fn reordered_control_dictionary_resolves_its_own_key_and_transpose() {
        let observed = vec![
            Chromosome {
                index: 1,
                name: "A".to_owned(),
                length: 100,
            },
            Chromosome {
                index: 2,
                name: "B".to_owned(),
                length: 200,
            },
        ];
        let control = vec![
            Chromosome {
                index: 1,
                name: "B".to_owned(),
                length: 200,
            },
            Chromosome {
                index: 2,
                name: "A".to_owned(),
                length: 100,
            },
        ];
        let observed_lookup = matrix_lookup_for_axis_names(&observed, "A", "B").unwrap();
        let control_lookup = matrix_lookup_for_axis_names(&control, "A", "B").unwrap();
        assert_eq!(observed_lookup.matrix_key, "1_2");
        assert_eq!(control_lookup.matrix_key, "1_2");
        let matrix = hic_core::Matrix {
            chromosome_1: 1,
            chromosome_2: 2,
            zooms: Vec::new(),
        };
        assert!(!transpose_for_matrix_axes(&observed_lookup, &matrix).unwrap());
        assert!(transpose_for_matrix_axes(&control_lookup, &matrix).unwrap());
    }

    #[test]
    fn independently_numbered_control_dictionary_gets_a_distinct_matrix_key() {
        let observed = vec![
            Chromosome {
                index: 1,
                name: "A".to_owned(),
                length: 100,
            },
            Chromosome {
                index: 2,
                name: "B".to_owned(),
                length: 200,
            },
        ];
        let control = vec![
            Chromosome {
                index: 3,
                name: "A".to_owned(),
                length: 100,
            },
            Chromosome {
                index: 7,
                name: "B".to_owned(),
                length: 200,
            },
        ];
        assert_eq!(
            matrix_lookup_for_axis_names(&observed, "A", "B")
                .unwrap()
                .matrix_key,
            "1_2"
        );
        assert_eq!(
            matrix_lookup_for_axis_names(&control, "A", "B")
                .unwrap()
                .matrix_key,
            "3_7"
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
    fn current_generation_stream_updates_are_displayed() {
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.generation = 8;
        let mut displayed = rendered_viewport_for(requested);
        displayed.generation = 7;
        let result = requested;
        assert!(should_display_tile_result(
            requested,
            displayed,
            result,
            Some([10, 10, 20, 20])
        ));
    }

    #[test]
    fn old_partial_update_cannot_overwrite_a_newer_displayed_texture() {
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.generation = 8;
        let mut displayed = rendered_viewport_for(requested);
        displayed.generation = 8;
        let mut stale = rendered_viewport_for(requested);
        stale.generation = 7;
        assert!(!should_display_tile_result(
            requested,
            displayed,
            stale,
            Some([10, 10, 20, 20])
        ));
    }

    #[test]
    fn covering_old_full_texture_can_bridge_to_the_current_generation() {
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.generation = 8;
        let mut displayed = requested;
        displayed.generation = 6;
        let mut covering = rendered_viewport_for(requested);
        covering.generation = 7;
        assert!(should_display_tile_result(
            requested, displayed, covering, None
        ));
        assert!(!should_display_tile_result(
            requested,
            displayed,
            covering,
            Some([10, 10, 20, 20])
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
    fn drag_release_resubmits_current_in_flight_request_at_final_position() {
        let mut displayed = GenomeViewport::new(1_000, 0.6);
        displayed.generation = 3;
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.generation = 8;
        let current_prediction = rendered_viewport_for(requested);

        assert!(should_request_settled_viewport(
            requested,
            displayed,
            Some(current_prediction),
            false
        ));
    }

    #[test]
    fn idle_settle_submits_the_latest_generation_after_coalesced_drag_input() {
        let mut completed = GenomeViewport::new(1_000, 0.6);
        completed.generation = 3;
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.generation = 8;

        assert!(should_request_idle_viewport(requested, completed, 5));
        assert!(!should_request_idle_viewport(requested, completed, 8));
        assert!(!should_request_idle_viewport(completed, completed, 3));
    }

    #[test]
    fn small_pan_requests_current_generation_even_inside_overscan() {
        let mut completed = GenomeViewport::new(1_000, 0.6);
        completed.center_bp = [500.0, 500.0];
        let mut requested = GenomeViewport::new(1_000, 0.4);
        requested.center_bp = [535.0, 500.0];
        requested.generation = 8;

        // The older overscanned texture is still drawable, which is exactly
        // why a coverage-only scheduler would skip a request and never
        // prefetch ahead.
        assert!(viewport_can_serve(requested, completed, true));
        // A fresh generation must be sent even inside overscan; TileEngine
        // replaces its single slot instead of accumulating a work queue.
        assert!(should_submit_interaction_viewport(requested, 5));
        assert!(!should_submit_interaction_viewport(requested, 8));
    }

    #[test]
    #[ignore = "requires JUICEBOX_REAL_HIC and performs full real-data Pearson calculations"]
    fn real_same_file_control_modes_match_observed_raw_bits() {
        let path = PathBuf::from(
            std::env::var_os("JUICEBOX_REAL_HIC")
                .expect("set JUICEBOX_REAL_HIC to a real intrachromosomal .hic file"),
        );
        let file = HicFile::open(&path).expect("failed to open real .hic");
        let matrix = file.read_matrix("1_1").expect("missing matrix 1_1");
        let chromosome = &file.header.chromosomes[matrix.chromosome_1 as usize];
        let engine = TileEngine::spawn(
            DatasetLaunch {
                path: path.clone(),
                matrix_key: "1_1".to_owned(),
                transpose_axes: false,
            },
            None,
            Some(DatasetLaunch {
                path: path.clone(),
                matrix_key: "1_1".to_owned(),
                transpose_axes: false,
            }),
        )
        .expect("same-file control should be compatible");
        let mut viewport = GenomeViewport::new(chromosome.length, INITIAL_SPAN_FRACTION);

        let (observed, observed_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::Observed);
        let (control, control_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::Control);
        assert!(
            observed.cache_misses > 0 && control.cache_misses > 0,
            "both dataset readers must populate their own visible Block cache"
        );
        assert_eq!(observed_bits, control_bits);

        let (_, observed_oe_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpected);
        let (_, control_oe_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ControlOverExpected);
        assert_eq!(observed_oe_bits, control_oe_bits);
        let (_, observed_p1_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpectedP1);
        let (_, control_p1_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ControlOverExpectedP1);
        assert_eq!(observed_p1_bits, control_p1_bits);
        let (_, observed_p1_v2_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpectedP1V2);
        assert_eq!(observed_p1_bits, observed_p1_v2_bits);
        let (_, control_p1_v2_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ControlOverExpectedP1V2);
        assert_eq!(control_p1_bits, control_p1_v2_bits);
        let (_, p1_vs_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpectedVsP1);
        assert_eq!(observed_p1_bits, p1_vs_bits);
        let (_, p1_vs_v2_bits) = request_complete_tile(
            &engine,
            &mut viewport,
            MatrixType::ObservedOverExpectedVsP1V2,
        );
        assert_eq!(p1_vs_bits, p1_vs_v2_bits);

        let (_, observed_log_oe_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::LogObservedExpected);
        let (_, control_log_oe_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::LogControlExpected);
        assert_eq!(observed_log_oe_bits, control_log_oe_bits);

        let (_, observed_pearson_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::Pearson);
        let (_, control_pearson_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ControlPearson);
        assert_eq!(observed_pearson_bits, control_pearson_bits);

        let (_, vs_bits) = request_complete_tile(&engine, &mut viewport, MatrixType::Vs);
        let size = tile_engine::OUTPUT_SIZE as usize;
        for y in 0..size {
            for x in 0..size {
                assert_eq!(
                    vs_bits[y * size + x],
                    vs_bits[x * size + y],
                    "same-file VS must be symmetric across the observed/control split"
                );
            }
        }

        let (_, ratio_bits) = request_complete_tile(&engine, &mut viewport, MatrixType::Ratio);
        assert!(ratio_bits.iter().all(|&bits| {
            let value = f32::from_bits(bits);
            value == 0.0 || value == 1.0
        }));
        assert!(ratio_bits.iter().any(|&bits| f32::from_bits(bits) == 1.0));

        let (_, ratio_v2_bits) = request_complete_tile(&engine, &mut viewport, MatrixType::RatioV2);
        assert_eq!(ratio_bits, ratio_v2_bits);

        let (_, oe_vs_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpectedVs);
        assert_eq!(observed_oe_bits, oe_vs_bits);

        let (_, pearson_vs_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::PearsonVs);
        assert_eq!(observed_pearson_bits, pearson_vs_bits);
    }

    #[test]
    #[ignore = "requires JUICEBOX_ASYMMETRIC_OBSERVED_HIC and JUICEBOX_ASYMMETRIC_CONTROL_HIC"]
    fn real_distinct_control_fixture_exercises_dual_reader_comparison_modes() {
        let observed_path = PathBuf::from(
            std::env::var_os("JUICEBOX_ASYMMETRIC_OBSERVED_HIC")
                .expect("set JUICEBOX_ASYMMETRIC_OBSERVED_HIC"),
        );
        let control_path = PathBuf::from(
            std::env::var_os("JUICEBOX_ASYMMETRIC_CONTROL_HIC")
                .expect("set JUICEBOX_ASYMMETRIC_CONTROL_HIC"),
        );
        let file = HicFile::open(&observed_path).expect("failed to open observed fixture");
        let matrix = file
            .read_matrix("1_1")
            .expect("missing observed matrix 1_1");
        let chromosome = &file.header.chromosomes[matrix.chromosome_1 as usize];
        let engine = TileEngine::spawn(
            DatasetLaunch {
                path: observed_path,
                matrix_key: "1_1".to_owned(),
                transpose_axes: false,
            },
            None,
            Some(DatasetLaunch {
                path: control_path,
                matrix_key: "1_1".to_owned(),
                transpose_axes: false,
            }),
        )
        .expect("distinct fixture datasets should be compatible");
        let mut viewport = GenomeViewport::new(chromosome.length, INITIAL_SPAN_FRACTION);

        let (_, observed_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::Observed);
        let (_, control_bits) = request_complete_tile(&engine, &mut viewport, MatrixType::Control);
        assert_ne!(observed_bits, control_bits);

        let (_, vs_bits) = request_complete_tile(&engine, &mut viewport, MatrixType::Vs);
        assert!(vs_bits.iter().any(|&bits| bits != 0));
        assert_ne!(vs_bits, observed_bits);
        assert_ne!(vs_bits, control_bits);

        let (_, ratio_bits) = request_complete_tile(&engine, &mut viewport, MatrixType::Ratio);
        assert!(ratio_bits.iter().any(|&bits| {
            let value = f32::from_bits(bits);
            value.is_finite() && value > 0.0 && value != 1.0
        }));
        let (_, ratio_v2_bits) = request_complete_tile(&engine, &mut viewport, MatrixType::RatioV2);
        assert_eq!(ratio_bits, ratio_v2_bits);

        let (_, observed_oe_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpected);
        let (_, control_oe_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ControlOverExpected);
        assert_ne!(observed_oe_bits, control_oe_bits);
        let (_, oe_vs_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpectedVs);
        assert_ne!(oe_vs_bits, observed_oe_bits);
        assert_ne!(oe_vs_bits, control_oe_bits);
        let (_, observed_p1_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpectedP1);
        let (_, control_p1_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ControlOverExpectedP1);
        assert_ne!(observed_p1_bits, control_p1_bits);
        let (_, p1_vs_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ObservedOverExpectedVsP1);
        assert_ne!(p1_vs_bits, observed_p1_bits);
        assert_ne!(p1_vs_bits, control_p1_bits);

        let (_, observed_pearson_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::Pearson);
        let (_, control_pearson_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::ControlPearson);
        assert_ne!(observed_pearson_bits, control_pearson_bits);
        let (_, pearson_vs_bits) =
            request_complete_tile(&engine, &mut viewport, MatrixType::PearsonVs);
        assert_ne!(pearson_vs_bits, observed_pearson_bits);
        assert_ne!(pearson_vs_bits, control_pearson_bits);
    }
}
