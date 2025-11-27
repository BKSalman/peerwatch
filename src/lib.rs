use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::{collections::BTreeSet, path::Path, time::Duration};

use interprocess::local_socket::tokio::{RecvHalf, SendHalf};
use iroh::EndpointId;
use iroh_gossip::{Gossip, TopicId};
use iroh_tickets::Ticket;
use notify::Watcher as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

pub mod message;
pub mod mpv;

pub const ALPN: &[u8] = b"BKSalman/peerplay/0";

pub static SKIP_NEXT_SEEK_EVENT: AtomicBool = AtomicBool::new(false);

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
    gossip: Gossip,
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
