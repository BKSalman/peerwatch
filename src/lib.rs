use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::BTreeSet, path::Path, time::Duration};

use anyhow::Context as _;
use bao_tree::ChunkNum;
use clap::{Parser, Subcommand};
use cpal::StreamConfig;
use eframe::egui;
use ffmpeg::{
    frame::Video as FfmpegVideoFrame,
    software::scaling::{Context, Flags},
};
use ffmpeg_next::{self as ffmpeg, Rational};
use iroh::protocol::Router;
use iroh::{EndpointId, SecretKey};
use iroh_blobs::BlobsProtocol;
use iroh_blobs::protocol::GetRequest;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::ticket::BlobTicket;
use iroh_gossip::{Gossip, TopicId};
use iroh_tickets::Ticket;
use n0_future::StreamExt;
use n0_future::boxed::BoxStream;
use ringbuf::SharedRb;
use ringbuf::storage::Heap;
use ringbuf::traits::Producer;
use ringbuf::wrap::caching::Caching;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::audio::AudioDecoder;
use crate::video::VideoFrame;

pub mod app;
pub mod audio;
pub mod peer_event;
pub mod player;
pub mod video;
pub mod window;

pub const ALPN: &[u8] = b"BKSalman/peerplay/0";

pub static SKIP_NEXT_SEEK_EVENT: AtomicBool = AtomicBool::new(false);

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    Create { video: PathBuf },
    Join { ticket: String },
}

#[derive(Debug)]
pub enum Event {
    PeerEvent(peer_event::PeerEvent),
    RedrawRequested(Duration),
    VideoFrame(VideoFrame),
}

pub enum FfmpegFrame {
    Video(Arc<FfmpegVideoFrame>),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerWatchTicket {
    pub topic_id: TopicId,
    pub bootstrap: BTreeSet<EndpointId>,
    pub blob_ticket: BlobTicket,
}

pub fn sha256_from_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;

    let mut sha256 = Sha256::new();

    let mut buf = [0u8; 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                sha256.update(&buf[..n]);
            }
            Err(e) => return Err(e.into()),
        }
        buf.fill(0);
    }

    Ok(sha256.finalize().to_vec())
}

impl PeerWatchTicket {
    pub fn new_random(blob_ticket: BlobTicket) -> Self {
        let topic_id = TopicId::from_bytes(rand::random());
        Self::new(topic_id, blob_ticket)
    }

    pub fn new(topic_id: TopicId, blob_ticket: BlobTicket) -> Self {
        Self {
            topic_id,
            bootstrap: Default::default(),
            blob_ticket,
        }
    }

    pub fn deserialize(input: &str) -> anyhow::Result<Self> {
        <Self as Ticket>::deserialize(input).map_err(Into::into)
    }

    pub fn serialize(&self) -> String {
        <Self as Ticket>::serialize(self)
    }
}

impl Ticket for PeerWatchTicket {
    const KIND: &'static str = "peerplay";

    fn to_bytes(&self) -> Vec<u8> {
        postcard::to_stdvec(&self).unwrap()
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, iroh_tickets::ParseError> {
        let ticket = postcard::from_bytes(bytes)?;
        Ok(ticket)
    }
}

pub struct PeerWatchNode {
    pub gossip: Gossip,
    router: Router,
    peers: BTreeSet<EndpointId>,
    store: MemStore,
    pub ticket: PeerWatchTicket,
}

impl PeerWatchNode {
    pub async fn spawn_host(video: PathBuf) -> anyhow::Result<Self> {
        let secret_key = SecretKey::generate(&mut rand::rng());

        let endpoint = iroh::Endpoint::builder()
            .secret_key(secret_key.clone())
            .alpns(vec![iroh_gossip::ALPN.to_vec(), iroh_blobs::ALPN.to_vec()])
            .bind()
            .await
            .unwrap();

        let store = MemStore::new();
        let blobs = BlobsProtocol::new(&store, None);
        let tag = store.add_path(&video).await?;

        let gossip = Gossip::builder().spawn(endpoint.clone());

        let router = Router::builder(endpoint)
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(iroh_blobs::ALPN, blobs)
            .spawn();

        let mut peers = BTreeSet::new();

        peers.insert(router.endpoint().id());
        let blob_ticket = BlobTicket::new(router.endpoint().addr(), tag.hash, tag.format);

        let ticket = PeerWatchTicket::new_random(blob_ticket);

        Ok(Self {
            gossip,
            peers,
            router,
            store,
            ticket,
        })
    }

    pub async fn spawn_peer(ticket: String) -> anyhow::Result<Self> {
        let store = MemStore::new();

        let secret_key = SecretKey::generate(&mut rand::rng());

        let endpoint = iroh::Endpoint::builder()
            .secret_key(secret_key.clone())
            .alpns(vec![iroh_gossip::ALPN.to_vec()])
            .bind()
            .await
            .unwrap();

        let gossip = Gossip::builder().spawn(endpoint.clone());

        let router = Router::builder(endpoint)
            .accept(iroh_gossip::ALPN, gossip.clone())
            .spawn();

        let mut peers = BTreeSet::new();

        peers.insert(router.endpoint().id());

        let ticket = PeerWatchTicket::deserialize(&ticket)?;

        Ok(Self {
            gossip,
            peers,
            router,
            store,
            ticket,
        })
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.router.endpoint().id()
    }

    pub async fn join(
        &mut self,
    ) -> anyhow::Result<(
        iroh_gossip::api::GossipSender,
        BoxStream<anyhow::Result<peer_event::PeerEvent>>,
    )> {
        self.peers.extend(self.ticket.bootstrap.clone());

        let bootstrap: Vec<_> = self.ticket.bootstrap.iter().cloned().collect();

        let (sender, receiver) = if bootstrap.is_empty() {
            self.gossip
                .subscribe(self.ticket.topic_id, bootstrap)
                .await?
                .split()
        } else {
            self.gossip
                .subscribe_and_join(self.ticket.topic_id, bootstrap)
                .await?
                .split()
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        sender
            .broadcast(
                postcard::to_allocvec(&crate::peer_event::PeerEvent::Presence {
                    timestamp_ms: now,
                    endpoint_id: self.endpoint_id(),
                })
                .unwrap()
                .into(),
            )
            .await
            .unwrap();

        let receiver = n0_future::stream::try_unfold(receiver, move |mut r| async move {
            loop {
                let Some(msg) = r.try_next().await? else {
                    return Ok(None);
                };

                let event = match msg {
                    iroh_gossip::api::Event::Received(message) => {
                        let Ok(event) =
                            postcard::from_bytes::<crate::peer_event::PeerEvent>(&message.content)
                        else {
                            continue;
                        };

                        event
                    }
                    iroh_gossip::api::Event::NeighborUp(id) => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        crate::peer_event::PeerEvent::PeerJoined {
                            timestamp_ms: now,
                            endpoint_id: id,
                        }
                    }
                    iroh_gossip::api::Event::NeighborDown(id) => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        crate::peer_event::PeerEvent::PeerLeft {
                            timestamp_ms: now,
                            endpoint_id: id,
                        }
                    }
                    msg => {
                        tracing::debug!("got gossip message: {msg:?}");
                        return Ok(None);
                    }
                };

                break Ok(Some((event, r)));
            }
        });

        Ok((sender, Box::pin(receiver)))
    }

    /// Returns true if it's a new peer
    pub fn peer_joined(&mut self, endpoint_id: EndpointId) -> bool {
        self.peers.insert(endpoint_id)
    }

    pub fn peer_left(&mut self, endpoint_id: &EndpointId) {
        self.peers.remove(endpoint_id);
    }

    pub async fn seek(&mut self, byte_offset: usize) -> anyhow::Result<()> {
        let ticket = &self.ticket.blob_ticket;
        let downloader = self.store.downloader(&self.router.endpoint());

        let chunk = byte_offset / 1024;

        downloader
            .download(
                GetRequest::blob_ranges(ticket.hash(), (ChunkNum(chunk as u64)..).into()),
                Some(ticket.addr().id),
            )
            .await?;

        Ok(())
    }

    pub async fn read(&mut self) -> anyhow::Result<Vec<u8>> {
        let ticket = &self.ticket.blob_ticket;
        let export = self.store.blobs().export_ranges(ticket.hash(), ..);

        Ok(export.concatenate().await?)
    }
}

pub fn spawn_decoder(
    path: impl AsRef<Path>,
    frame_tx: crossbeam::channel::Sender<(i64, FfmpegFrame)>,
    seek_rx: crossbeam::channel::Receiver<i64>,
    audio_config: StreamConfig,
    mut audio_producer: Caching<Arc<SharedRb<Heap<f32>>>, true, false>,
    ctx: egui::Context,
) -> anyhow::Result<(Rational, u32, u32)> {
    let path = path.as_ref().to_path_buf();
    let mut ictx = ffmpeg::format::input(&path).unwrap();
    let video_stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(anyhow::anyhow!("No video stream"))?;
    let video_stream_index = video_stream.index();

    let audio_stream = ictx.streams().best(ffmpeg::media::Type::Audio);

    let context_decoder =
        ffmpeg::codec::context::Context::from_parameters(video_stream.parameters()).unwrap();
    let mut video_decoder = context_decoder.decoder().video().unwrap();
    video_decoder.set_time_base(video_stream.time_base());
    let ret = (
        video_stream.time_base(),
        video_decoder.width(),
        video_decoder.height(),
    );

    if let Some(audio_stream) = audio_stream {
        let audio_stream_index = audio_stream.index();
        let audio_params = audio_stream.parameters();
        let (audio_packet_tx, audio_packet_rx) = crossbeam::channel::bounded::<ffmpeg::Packet>(128);
        let (video_packet_tx, video_packet_rx) = crossbeam::channel::unbounded::<ffmpeg::Packet>();
        let seek_rx_audio = seek_rx.clone();
        let seek_rx_video = seek_rx.clone();
        let frame_tx_video = frame_tx.clone();
        let audio_time_base = audio_stream.time_base();

        std::thread::spawn(move || {
            let mut context =
                ffmpeg::codec::context::Context::from_parameters(audio_params).unwrap();
            context.set_time_base(audio_time_base);
            let mut audio_decoder = AudioDecoder::new(&audio_config, context);

            loop {
                crossbeam::select! {
                    recv(seek_rx_audio) -> _cmd => {
                        audio_decoder.flush();
                        // Drain the channel
                        while audio_packet_rx.try_recv().is_ok() {}
                    }
                    recv(audio_packet_rx) -> packet => {
                        match packet {
                            Ok(packet) => {
                                if let Err(e) = audio_decoder.push_packet(packet) {
                                    tracing::error!("Audio decode error: {}", e);
                                    continue;
                                }

                                loop {
                                    match audio_decoder.pop_frame() {
                                        Ok(Some((frame, _pts))) => {
                                            let expected_bytes =
                                                frame.samples() * frame.channels() as usize * core::mem::size_of::<f32>();
                                            let samples = bytemuck::cast_slice(&frame.data(0)[..expected_bytes]);
                                            for sample in samples {
                                                while audio_producer.try_push(*sample).is_err() {
                                                    std::thread::sleep(std::time::Duration::from_micros(100));
                                                    // tracing::debug!("can't push audio");
                                                }
                                            }
                                        }
                                        Ok(None) => break,
                                        Err(e) => {
                                            tracing::error!("Audio resample error: {}", e);
                                            break;
                                        }
                                    }
                                }
                            },
                            Err(e) => {
                                tracing::error!("ERROR: {e}");
                                return;
                            },
                        }
                    }
                }
            }
        });

        std::thread::spawn(move || {
            let thread = move || -> anyhow::Result<()> {
                let mut video_decoder = video_decoder;
                let mut video_frame = FfmpegVideoFrame::empty();
                let mut scaler = None;

                loop {
                    crossbeam::select! {
                        // recv(seek_rx_video) -> _cmd => {
                        //     video_decoder.flush();
                        //     // Drain the channel
                        //     while video_packet_rx.try_recv().is_ok() {}
                        // }
                        recv(video_packet_rx) -> packet => {
                            match packet {
                                Ok(packet) => {
                                    if let Err(e) = video_decoder.send_packet(&packet) {
                                        tracing::error!("Failed to send packet to video decoder: {e}");
                                        continue;
                                    }

                                    while video_decoder.receive_frame(&mut video_frame).is_ok() {
                                        let frame = {
                                            let mut rgb_frame = FfmpegVideoFrame::empty();
                                            let scaler = if let Some(scaler) = &mut scaler {
                                                scaler
                                            } else {
                                                let new_scaler = Context::get(
                                                    video_frame.format(),
                                                    video_frame.width(),
                                                    video_frame.height(),
                                                    ffmpeg::format::Pixel::RGBA,
                                                    video_frame.width(),
                                                    video_frame.height(),
                                                    Flags::BILINEAR,
                                                )?;
                                                scaler = Some(new_scaler);
                                                scaler.as_mut().unwrap()
                                            };

                                            scaler.run(&video_frame, &mut rgb_frame)?;
                                            rgb_frame.set_pts(video_frame.timestamp());
                                            rgb_frame
                                        };

                                        let frame_arc = Arc::new(frame);

                                        frame_tx_video.send((
                                            frame_arc.pts().unwrap_or(0),
                                            FfmpegFrame::Video(frame_arc),
                                        ))?;
                                    }
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }
                }
            };

            if let Err(e) = thread() {
                tracing::error!("video thread: {e}");
            }
        });

        std::thread::spawn(move || {
            let mut thread = move || -> anyhow::Result<()> {
                loop {
                    crossbeam::select! {
                        recv(seek_rx) -> cmd => {
                            match cmd {
                                Ok(cmd) => {
                                    ictx.seek(cmd, cmd..)?;
                                },
                                Err(e) => {
                                    tracing::error!("{e}");
                                },
                            }
                        }
                        default => {
                            for (stream, packet) in ictx.packets() {
                                if stream.index() == video_stream_index {
                                    if let Err(e) = video_packet_tx.send(packet) {
                                        tracing::error!("video packet channel: {e}");
                                    }
                                } else if stream.index() == audio_stream_index {
                                    audio_packet_tx.send(packet)?;
                                }
                            }
                        }
                    }
                }
            };

            if let Err(e) = thread() {
                tracing::error!("ERROR: decoder thread: {e}");
            }
        });
    } else {
        std::thread::spawn(move || -> anyhow::Result<()> {
            let mut video_decoder = video_decoder;
            let mut video_frame = FfmpegVideoFrame::empty();
            let mut scaler = None;

            loop {
                crossbeam::select! {
                    recv(seek_rx) -> cmd => {
                        match cmd {
                            Ok(cmd) => {
                                ictx.seek(cmd, cmd..)?;
                                video_decoder.flush();
                            },
                            Err(e) => {
                                tracing::error!("{e}");
                            },
                        }
                    }
                    default => {
                        match ictx.packets().next() {
                            Some((stream, packet)) => {
                                if stream.index() == video_stream_index {
                                    if let Err(e) = video_decoder.send_packet(&packet) {
                                        tracing::error!("Failed to send packet to video decoder: {e}");
                                        continue;
                                    }

                                    while video_decoder.receive_frame(&mut video_frame).is_ok() {
                                        let frame = {
                                            let mut rgb_frame = FfmpegVideoFrame::empty();
                                            let scaler = if let Some(scaler) = &mut scaler {
                                                scaler
                                            } else {
                                                let new_scaler = Context::get(
                                                    video_frame.format(),
                                                    video_frame.width(),
                                                    video_frame.height(),
                                                    ffmpeg::format::Pixel::RGBA,
                                                    video_frame.width(),
                                                    video_frame.height(),
                                                    Flags::BILINEAR,
                                                )?;
                                                scaler = Some(new_scaler);
                                                scaler.as_mut().unwrap()
                                            };

                                            scaler.run(&video_frame, &mut rgb_frame)?;
                                            rgb_frame.set_pts(video_frame.timestamp());
                                            rgb_frame
                                        };

                                        let frame_arc = Arc::new(frame);

                                        frame_tx.send((
                                            frame_arc.pts().unwrap_or(0),
                                            FfmpegFrame::Video(frame_arc),
                                        ))?;

                                        ctx.request_repaint();
                                    }
                                }
                            }
                            None => return Ok(()), // EOF
                        }
                    }
                }
            }
        });
    }

    Ok(ret)
}
