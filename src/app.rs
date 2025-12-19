use std::{collections::VecDeque, num::NonZeroU64, path::PathBuf, sync::Arc};

use eframe::egui::{self, Color32, ColorImage};

use egui::TextureHandle;

use ffmpeg_next as ffmpeg;

use ffmpeg::frame::Video as FfmpegFrame;
use wgpu::util::DeviceExt;

use crate::{
    Cli, PeerWatchNode,
    player::{PlayerRenderCallback, PlayerRenderResources, VERTICES, Vertex},
    spawn_decoder,
    video::VideoFrame,
};

struct FrameCache {
    frames: VecDeque<(i64, Arc<FfmpegFrame>)>,
    max_size: usize,
}

impl FrameCache {
    fn new(max_size: usize) -> Self {
        Self {
            frames: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    fn insert(&mut self, timestamp: i64, frame: Arc<FfmpegFrame>) {
        // Evict oldest frame if at capacity
        if self.frames.len() >= self.max_size {
            self.frames.pop_front();
        }
        self.frames.push_back((timestamp, frame));
    }

    fn latest(&self) -> Option<&Arc<FfmpegFrame>> {
        self.frames.back().map(|(_, frame)| frame)
    }

    fn clear(&mut self) {
        self.frames.clear();
    }
}

pub struct App {
    rt: tokio::runtime::Runtime,
    peerwatch_node: Option<PeerWatchNode>,
    texture_handle: Option<TextureHandle>,
    frame_cache: FrameCache,
    frame_rx: std::sync::mpsc::Receiver<(i64, Arc<FfmpegFrame>)>,
    seek_tx: crossbeam::channel::Sender<i64>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, video: PathBuf) -> Self {
        let (frame_tx, frame_rx) = std::sync::mpsc::channel();
        let (seek_tx, seek_rx) = crossbeam::channel::bounded(32);
        let (width, height) = spawn_decoder(video, frame_tx, seek_rx, cc.egui_ctx.clone()).unwrap();

        {
            let wgpu_render_state = cc.wgpu_render_state.as_ref().unwrap();

            let device = &wgpu_render_state.device;

            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
            });

            let texture_size = wgpu::Extent3d {
                width,
                height,
                // All textures are stored as 3D, we represent our 2D texture
                // by setting depth to 1.
                depth_or_array_layers: 1,
            };

            let player_texture = device.create_texture(&wgpu::TextureDescriptor {
                size: texture_size,
                mip_level_count: 1, // We'll talk about this a little later
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // Most images are stored using sRGB, so we need to reflect that here.
                format: wgpu::TextureFormat::Rgba8Unorm,
                // TEXTURE_BINDING tells wgpu that we want to use this texture in shaders
                // COPY_DST means that we want to copy data to this texture
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                label: Some("player_texture"),
                // This is the same as with the SurfaceConfig. It
                // specifies what texture formats can be used to
                // create TextureViews for this texture. The base
                // texture format (Rgba8UnormSrgb in this case) is
                // always supported. Note that using a different
                // texture format is not supported on the WebGL2
                // backend.
                view_formats: &[],
            });

            let view = player_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Nearest,
                mipmap_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            });

            let texture_bind_group_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                multisampled: false,
                                view_dimension: wgpu::TextureViewDimension::D2,
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
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
                    label: Some("texture_bind_group_layout"),
                });

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("player_bind_group"),
                layout: &texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });

            let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::cast_slice(VERTICES),
                usage: wgpu::BufferUsages::VERTEX,
            });

            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("player"),
                bind_group_layouts: &[&texture_bind_group_layout],
                push_constant_ranges: &[],
            });

            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("player_pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[Vertex::desc()],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu_render_state.target_format.into())],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            });

            // Because the graphics pipeline must have the same lifetime as the egui render pass,
            // instead of storing the pipeline in our `Custom3D` struct, we insert it into the
            // `paint_callback_resources` type map, which is stored alongside the render pass.
            wgpu_render_state
                .renderer
                .write()
                .callback_resources
                .insert(PlayerRenderResources {
                    pipeline,
                    bind_group,
                    texture: player_texture,
                    view,
                    sampler,
                    vertex_buffer,
                });
        }

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        Self {
            rt,
            peerwatch_node: None,
            texture_handle: None,
            frame_rx,
            seek_tx,
            frame_cache: FrameCache::new(3),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.texture_handle.is_none() {
            let texture_name = "video-texture";

            let size = egui::vec2(100., 100.);

            let color_image =
                egui::ColorImage::filled([size.x as usize, size.y as usize], Color32::BLACK);
            let texture =
                ctx.load_texture(texture_name, color_image, egui::TextureOptions::default());

            self.texture_handle = Some(texture);
        }

        while let Ok((timestamp, frame)) = self.frame_rx.try_recv() {
            self.frame_cache.insert(timestamp, frame);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // if let Some(tex) = &self.texture_handle {
            //     ui.image(tex);
            // }

            if let Some(frame) = self.frame_cache.latest() {
                let (rect, response) =
                    ui.allocate_exact_size(ui.available_size(), egui::Sense::drag());

                ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                    rect,
                    PlayerRenderCallback {
                        frame: Arc::clone(frame),
                    },
                ));
            }
        });
    }
}
