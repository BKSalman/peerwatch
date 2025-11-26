use serde::{Deserialize, Serialize};

// {"event":"property-change","name":"pause","data":true}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    PropertyChange(PropertyChange),
    Seek,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "name", content = "data", rename_all = "kebab-case")]
pub enum PropertyChange {
    Pause(bool),
}
