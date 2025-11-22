use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Message {
    id: u32,
    payload: Payload,
}

impl Message {
    pub fn new(payload: Payload) -> Self {
        Message {
            id: rand::random(),
            payload,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Payload {
    Ping,
    Play,
    Pause,
    Seek(f32),
}
