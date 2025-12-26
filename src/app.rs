use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
};

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use eframe::egui::{self, Color32, Slider, Vec2};

use egui::TextureHandle;

use ffmpeg_next as ffmpeg;

use ffmpeg::frame::Video as FfmpegVideoFrame;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Producer as _, Split as _},
};

use crate::{
    FfmpegFrame, PeerWatchNode,
    player::{Player, PlayerRenderCallback},
    spawn_decoder,
};

struct FrameCache {
    frames: VecDeque<(i64, Arc<FfmpegVideoFrame>)>,
    max_size: usize,
    time_base: f64,
}

impl FrameCache {
    fn new(time_base: f64, max_size: usize) -> Self {
        Self {
            time_base,
            frames: VecDeque::with_capacity(max_size),
            max_size,
        }
    }

    fn insert(&mut self, timestamp: i64, frame: Arc<FfmpegVideoFrame>) {
        self.frames.push_back((timestamp, frame));
    }

    fn get_and_evict(&mut self, timestamp: f64) -> Option<Arc<FfmpegVideoFrame>> {
        let timestamp = (timestamp / self.time_base) as i64;

        self.frames.retain(|(t, _)| *t >= timestamp);

        self.frames.front().map(|(_, f)| f.clone())
    }

    fn is_full(&self) -> bool {
        self.frames.len() >= self.max_size
    }
}

pub struct App {
    rt: tokio::runtime::Runtime,
    peerwatch_node: Option<PeerWatchNode>,
    texture_handle: Option<TextureHandle>,
    frame_cache: FrameCache,
    frame_rx: crossbeam::channel::Receiver<(i64, FfmpegFrame)>,
    seek_tx: crossbeam::channel::Sender<i64>,
    total_samples: Arc<AtomicU64>,
    stream: cpal::Stream,
    volume_atomic: Arc<std::sync::atomic::AtomicU32>,
    volume: f32,
    audio_config: cpal::StreamConfig,
    video_time_base: ffmpeg_next::Rational,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, video: PathBuf) -> Self {
        let (frame_tx, frame_rx) = crossbeam::channel::bounded(5);
        let (seek_tx, seek_rx) = crossbeam::channel::bounded(5);

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .expect("no output device available");
        let mut supported_configs_range = device
            .supported_output_configs()
            .expect("error while querying configs");
        let supported_config = supported_configs_range
            .next()
            .expect("no supported config?!")
            .with_sample_rate(48000);
        let config: cpal::StreamConfig = supported_config.into();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        const LATENCY: f32 = 150.0;

        let latency_frames = (LATENCY / 1_000.0) * config.sample_rate as f32;
        let latency_samples = latency_frames as usize * config.channels as usize;

        let ring = HeapRb::<f32>::new(latency_samples * 2);
        let (mut producer, mut consumer) = ring.split();

        // Pre-fill with silence
        for _ in 0..latency_samples {
            producer.try_push(0.0).unwrap();
        }

        let volume = 10.0;

        let volume_atomic = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(
            (volume / 100.0f32).to_bits(),
        ));
        let volume_clone = volume_atomic.clone();

        let total_samples = Arc::new(AtomicU64::new(0));

        let stream = {
            let total_samples = total_samples.clone();
            let ctx = cc.egui_ctx.clone();

            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let volume_gain =
                            f32::from_bits(volume_clone.load(std::sync::atomic::Ordering::Relaxed));

                        let mut samples = 0;

                        for sample in data.iter_mut() {
                            *sample = match consumer.try_pop() {
                                Some(s) => {
                                    samples += 1;
                                    s * volume_gain
                                }
                                None => 0.0,
                            };
                        }

                        total_samples.fetch_add(samples, std::sync::atomic::Ordering::Relaxed);
                        ctx.request_repaint();
                    },
                    move |err| {
                        tracing::error!("{err}");
                    },
                    None,
                )
                .unwrap()
        };

        stream.play().unwrap();

        let (video_time_base, width, height) = spawn_decoder(
            video,
            frame_tx,
            seek_rx,
            config.clone(),
            producer,
            cc.egui_ctx.clone(),
        )
        .unwrap();

        Player::new(cc, width, height);

        Self {
            rt,
            peerwatch_node: None,
            texture_handle: None,
            frame_rx,
            seek_tx,
            stream,
            audio_config: config,
            video_time_base,
            frame_cache: FrameCache::new(f64::from(video_time_base), 10),
            total_samples,
            volume,
            volume_atomic,
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

        egui::CentralPanel::default().show(ctx, |ui| {
            // if let Some(tex) = &self.texture_handle {
            //     ui.image(tex);
            // }

            let samples = self
                .total_samples
                .load(std::sync::atomic::Ordering::Relaxed);

            let timestamp = samples as f64
                / (self.audio_config.sample_rate as f64 * self.audio_config.channels as f64);

            if let Some(frame) = self.frame_cache.get_and_evict(timestamp) {
                let (rect, response) = ui.allocate_exact_size(
                    ui.available_size() - Vec2::new(0., 40.),
                    egui::Sense::drag(),
                );

                ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                    rect,
                    PlayerRenderCallback {
                        frame: Arc::clone(&frame),
                    },
                ));
            }

            if ui.add(Slider::new(&mut self.volume, 0.0..=100.0)).changed() {
                self.volume_atomic.store(
                    (self.volume / 100.).to_bits(),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }

            if ui.button("+5").clicked() {
                let new_timestamp = timestamp + 5.;
                self.seek_tx
                    .send((new_timestamp * f64::from(self.video_time_base)) as i64)
                    .unwrap();

                let samples = (new_timestamp
                    * (self.audio_config.sample_rate as f64 * self.audio_config.channels as f64))
                    as u64;

                self.total_samples
                    .store(samples, std::sync::atomic::Ordering::Relaxed);
            }
        });

        if !self.frame_cache.is_full() {
            while let Ok((timestamp, frame)) = self.frame_rx.try_recv() {
                match frame {
                    FfmpegFrame::Video(frame) => {
                        self.frame_cache.insert(timestamp, frame);
                    }
                }
            }
        }
    }
}
