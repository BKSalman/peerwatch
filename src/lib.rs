use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::BTreeSet, path::Path, time::Duration};

use bao_tree::ChunkNum;
use clap::{Parser, Subcommand};
use eframe::egui::{self, ColorImage};
use ffmpeg::{
    frame::Video as FfmpegFrame,
    software::scaling::{Context, Flags},
};
use ffmpeg_next as ffmpeg;
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::video::VideoFrame;

pub mod app;
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerWatchTicket {
    pub topic_id: TopicId,
    pub bootstrap: BTreeSet<EndpointId>,
    pub blob_ticket: BlobTicket,
    pub seek_map: SeekMap,
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
    pub fn new_random(video: &Path, blob_ticket: BlobTicket) -> Self {
        let topic_id = TopicId::from_bytes(rand::random());
        Self::new(topic_id, video, blob_ticket)
    }

    pub fn new(topic_id: TopicId, video: &Path, blob_ticket: BlobTicket) -> Self {
        let seek_map = SeekMap::generate(video).unwrap();
        Self {
            topic_id,
            bootstrap: Default::default(),
            seek_map,
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

        let ticket = PeerWatchTicket::new_random(&video, blob_ticket);

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
                        println!("got gossip message: {msg:?}");
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeekMap {
    /// Total duration in seconds
    duration_secs: f64,
    /// List of keyframes: (Time in Seconds, Byte Offset)
    keyframes: Vec<(f64, i64)>,
}

impl SeekMap {
    /// Host runs this on their local file to generate the map
    pub fn generate(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        ffmpeg::init()?;

        // Open the file using FFmpeg context
        let mut context = ffmpeg::format::input(&path)?;

        // Find the "best" video stream (highest resolution/bitrate usually)
        let stream = context
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| anyhow::anyhow!("No video stream found"))?;

        let stream_index = stream.index();
        let time_base = stream.time_base();

        // This calculates the duration of the file
        let duration = context.duration() as f64 / ffmpeg::ffi::AV_TIME_BASE as f64;

        let mut keyframes = Vec::new();

        // Iterate over every packet in the file (efficient, doesn't decode the image)
        for (stream, packet) in context.packets() {
            if stream.index() == stream_index {
                // We only care about Keyframes (I-frames).
                // Browsers/Players can usually only seek directly to these.
                if packet.is_key() {
                    let pts = packet.pts().unwrap_or(0);

                    // Convert internal time base to seconds
                    let time_secs = pts as f64 * f64::from(time_base);

                    // packet.pos() returns the byte offset in the file
                    // Some containers might return None (-1), but most file-based ones work.
                    let pos = packet.position();
                    if pos > 0 {
                        keyframes.push((time_secs, pos as i64));
                    } else {
                        println!("alo: {pos}");
                    }
                }
            }
        }

        Ok(Self {
            duration_secs: duration,
            keyframes,
        })
    }

    /// Peer uses this to find the byte offset for a timestamp
    pub fn get_offset_for_time(&self, time: f64) -> Option<i64> {
        // Find the keyframe closest to (but not after) the target time
        // We use partition_point to perform a binary search
        let idx = self.keyframes.partition_point(|(t, _)| *t <= time);

        if idx == 0 {
            // Requesting time before first keyframe, return start
            return self.keyframes.first().map(|(_, offset)| *offset);
        }

        // partition_point returns the first element *greater*, so go back one
        Some(self.keyframes[idx - 1].1)
    }
}

pub fn spawn_decoder(
    path: impl AsRef<Path>,
    frame_tx: std::sync::mpsc::Sender<(i64, Arc<FfmpegFrame>)>,
    seek_rx: crossbeam::channel::Receiver<i64>,
    ctx: egui::Context,
) -> anyhow::Result<(u32, u32)> {
    let path = path.as_ref().to_path_buf();
    let mut ictx = ffmpeg::format::input(&path).unwrap();
    let input = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(anyhow::anyhow!("No video stream"))?;
    let video_stream_index = input.index();

    let context_decoder =
        ffmpeg::codec::context::Context::from_parameters(input.parameters()).unwrap();
    let mut decoder = context_decoder.decoder().video().unwrap();
    let ret = (decoder.width(), decoder.height());

    std::thread::spawn(move || {
        let mut frame = FfmpegFrame::empty();

        let ticker = crossbeam::channel::tick(Duration::from_micros(100));
        let mut scaler = None;

        loop {
            crossbeam::select! {
                recv(seek_rx) -> cmd => {
                    match cmd {
                        Ok(cmd) => {
                            ictx.seek(cmd, cmd..).unwrap();
                            decoder.flush();
                        },
                        Err(e) => {
                            tracing::error!("{e}");
                        },
                    }
                }
                recv(ticker) -> _ => {
                    match ictx.packets().next() {
                        Some((stream, packet)) => {
                            if stream.index() == video_stream_index {
                                decoder.send_packet(&packet).unwrap();
                                while decoder.receive_frame(&mut frame).is_ok() {
                                    let frame = {
                                        let mut rgb_frame = FfmpegFrame::empty();
                                        let scaler = if let Some(scaler) = &mut scaler {
                                            scaler
                                        } else {
                                            let new_scaler = Context::get(
                                                frame.format(),
                                                frame.width(),
                                                frame.height(),
                                                ffmpeg::format::Pixel::RGBA,
                                                frame.width(),
                                                frame.height(),
                                                Flags::BILINEAR,
                                            )?;
                                            scaler = Some(new_scaler);

                                            scaler.as_mut().unwrap()
                                        };

                                        scaler.run(&frame, &mut rgb_frame)?;
                                        rgb_frame
                                    };

                                    let frame_arc = Arc::new(frame);
                                    frame_tx.send((
                                        frame_arc.timestamp().unwrap_or(0),
                                        frame_arc,
                                    )).unwrap();

                                    ctx.request_repaint();
                                }
                            }
                        }
                        None => return anyhow::Ok(()), // EOF
                    }
                }
            }
        }
    });

    Ok(ret)
}

pub fn frame_to_colorimage(frame: &FfmpegFrame) -> anyhow::Result<ColorImage> {
    let frame = {
        let mut rgb_frame = FfmpegFrame::empty();
        let mut scaler = Context::get(
            frame.format(),
            frame.width(),
            frame.height(),
            ffmpeg::format::Pixel::RGB8,
            frame.width(),
            frame.height(),
            Flags::BILINEAR,
        )?;
        scaler.run(frame, &mut rgb_frame)?;
        rgb_frame
    };

    let (w, h) = (frame.width(), frame.height());

    let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], frame.data(0));

    Ok(image)
}
