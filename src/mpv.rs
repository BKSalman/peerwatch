use interprocess::local_socket::tokio::{RecvHalf, SendHalf};
use iroh_gossip::api::GossipSender;
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;

use crate::{SKIP_NEXT_SEEK_EVENT, send_mpv_command};

pub mod events;

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Event(events::Event),
}

pub async fn handle_mpv_messages(
    sender: &GossipSender,
    read_buf: &str,
    mpv_writer: &mut SendHalf,
    mpv_reader: &mut BufReader<RecvHalf>,
) -> anyhow::Result<()> {
    if let Ok(message) = serde_json::from_str::<Message>(read_buf) {
        match message {
            Message::Event(event) => match event {
                events::Event::PropertyChange(property_change) => match property_change {
                    events::PropertyChange::Pause(is_paused) => {
                        if is_paused {
                            sender
                                .broadcast(
                                    postcard::to_allocvec(&crate::message::Message::new(
                                        crate::message::Payload::Pause,
                                    ))?
                                    .into(),
                                )
                                .await?;
                        } else {
                            sender
                                .broadcast(
                                    postcard::to_allocvec(&crate::message::Message::new(
                                        crate::message::Payload::Play,
                                    ))?
                                    .into(),
                                )
                                .await?;
                        }
                    }
                },
                events::Event::Seek => {
                    if SKIP_NEXT_SEEK_EVENT.swap(false, std::sync::atomic::Ordering::Acquire) {
                        return Ok(());
                    }
                    let (_, property_reply) = send_mpv_command(
                        mpv_writer,
                        mpv_reader,
                        r#"{ "command": ["get_property", "playback-time"] }"#,
                    )
                    .await?;

                    let reply = serde_json::from_str::<serde_json::Value>(&property_reply)?;

                    let seek = reply["data"].as_f64().ok_or(anyhow::anyhow!("not f64"))?;

                    sender
                        .broadcast(
                            postcard::to_allocvec(&crate::message::Message::new(
                                crate::message::Payload::Seek(seek),
                            ))?
                            .into(),
                        )
                        .await?;
                }
            },
        }
    }

    Ok(())
}
