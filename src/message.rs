use std::time::{SystemTime, UNIX_EPOCH};

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Event {
    StateUpdate(StateUpdate),
    PeerJoined {
        timestamp_ms: u64,
        endpoint_id: EndpointId,
    },
    PeerLeft {
        timestamp_ms: u64,
        endpoint_id: EndpointId,
    },
    Presence {
        timestamp_ms: u64,
        endpoint_id: EndpointId,
    },
    AnnounceLeader {
        timestamp_ms: u64,
        endpoint_id: EndpointId,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum UpdateTrigger {
    UserAction,
    Periodic,
    Correction,
    Initial,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StateUpdate {
    pub peer_id: EndpointId,
    pub timestamp_ms: u64,
    pub position: f64,
    pub is_paused: bool,
    /// Why this update was sent
    pub trigger: UpdateTrigger,
}

impl StateUpdate {
    pub fn new(
        peer_id: EndpointId,
        position: f64,
        is_paused: bool,
        trigger: UpdateTrigger,
    ) -> Self {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        Self {
            peer_id,
            timestamp_ms,
            position,
            is_paused,
            trigger,
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
