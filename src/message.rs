use std::time::{SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: u32,
    timestamp_ms: u64,
    pub payload: Payload,
    pub peer_id: iroh::PublicKey,
}

impl Message {
    pub fn new(peer_id: EndpointId, payload: Payload) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Message {
            id: rand::random(),
            peer_id,
            payload,
            timestamp_ms,
        }
    }

    pub fn age_ms(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        now.saturating_sub(self.timestamp_ms)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum UpdateTrigger {
    UserAction,
    Periodic,
    Correction,
    Initial,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Payload {
    StateUpdate {
        position: f64,
        is_paused: bool,
        /// Why this update was sent
        trigger: UpdateTrigger,
    },
}
