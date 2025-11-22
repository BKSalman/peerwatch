use serde::{Deserialize, Serialize};

pub mod events;

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Event(events::Event),
}
