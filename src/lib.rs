use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{collections::BTreeSet, path::Path, time::Duration};

use interprocess::local_socket::tokio::{RecvHalf, SendHalf};
use iroh::protocol::Router;
use iroh::{EndpointId, SecretKey};
use iroh_gossip::{Gossip, TopicId};
use iroh_tickets::Ticket;
use n0_future::StreamExt;
use n0_future::boxed::BoxStream;
use notify::Watcher as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

pub mod app;
pub mod mpv;
pub mod peer_event;

pub const ALPN: &[u8] = b"BKSalman/peerplay/0";

pub static SKIP_NEXT_SEEK_EVENT: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub enum Event {
    MpvEvent(mpv::events::Event),
    PeerEvent(peer_event::PeerEvent),
    RedrawRequested,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PeerWatchTicket {
    pub topic_id: TopicId,
    pub bootstrap: BTreeSet<EndpointId>,
    pub video_sha256: Vec<u8>,
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
    pub fn new_random(video: &Path) -> Self {
        let topic_id = TopicId::from_bytes(rand::random());
        Self::new(topic_id, video)
    }

    pub fn new(topic_id: TopicId, video: &Path) -> Self {
        Self {
            topic_id,
            bootstrap: Default::default(),
            video_sha256: sha256_from_file(video).expect("failed to read video file"),
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

pub fn wait_until_file_created(file_path: impl AsRef<Path>) -> anyhow::Result<()> {
    let file_path = file_path.as_ref();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    // Watcher can't be registered for file that don't exists.
    // I use its parent directory instead, because I'm sure that it always exists
    let file_dir = file_path.parent().unwrap();
    watcher.watch(&file_dir, notify::RecursiveMode::NonRecursive)?;
    if !file_path.exists() {
        loop {
            let event = rx.recv_timeout(Duration::from_secs(2))??;
            match event.kind {
                notify::EventKind::Create(_) => {
                    if let Some(path) = event.paths.first() {
                        if path == file_path {
                            break;
                        }
                    }
                }
                _ => continue,
            }
        }
    }
    watcher.unwatch(file_dir)?;
    Ok(())
}

pub struct PeerWatchNode {
    pub gossip: Gossip,
    router: Router,
    peers: BTreeSet<EndpointId>,
    pub leader: Option<EndpointId>,
}

impl PeerWatchNode {
    pub async fn spawn() -> anyhow::Result<Self> {
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

        Ok(Self {
            gossip,
            router,
            peers,
            leader: None,
        })
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.router.endpoint().id()
    }

    pub async fn join(
        &mut self,
        ticket: &PeerWatchTicket,
    ) -> anyhow::Result<(
        iroh_gossip::api::GossipSender,
        BoxStream<anyhow::Result<peer_event::PeerEvent>>,
    )> {
        self.peers.extend(ticket.bootstrap.clone());

        if self.peers.len() == 1 {
            println!("I'm the leader");
            self.leader = Some(self.endpoint_id());
        }

        let bootstrap: Vec<_> = ticket.bootstrap.iter().cloned().collect();

        let (sender, receiver) = if bootstrap.is_empty() {
            self.gossip
                .subscribe(ticket.topic_id, bootstrap)
                .await?
                .split()
        } else {
            self.gossip
                .subscribe_and_join(ticket.topic_id, bootstrap)
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
        println!("current leader: {:?}", self.leader);
        if Some(endpoint_id) == self.leader.as_ref() {
            self.elect_leader();
        }
    }

    pub fn elect_leader(&mut self) {
        self.leader = self.peers.iter().next_back().cloned();
        println!("new leader: {:?}", self.leader);
    }

    pub fn is_leader(&self) -> bool {
        self.leader == Some(self.endpoint_id())
    }
}

pub async fn send_mpv_command(
    writer: &mut SendHalf,
    reader: &mut BufReader<RecvHalf>,
    command: &str,
) -> anyhow::Result<(usize, String)> {
    let mut write_buf = Vec::new();
    let mut read_buf = String::with_capacity(128);

    writeln!(&mut write_buf, "{command}")?;

    writer.write_all(&write_buf).await?;
    let bytes_read = reader.read_line(&mut read_buf).await?;

    Ok((bytes_read, read_buf))
}
