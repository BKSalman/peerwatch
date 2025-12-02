use interprocess::local_socket::tokio::{RecvHalf, SendHalf};
use iroh::EndpointId;
use iroh_gossip::api::GossipSender;
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;

use crate::{
    SKIP_NEXT_SEEK_EVENT,
    message::{StateUpdate, UpdateTrigger},
    send_mpv_command,
};

pub mod events;

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MpvMessage {
    Event(events::Event),
}

pub async fn get_playback_time(
    mpv_writer: &mut SendHalf,
    mpv_reader: &mut BufReader<RecvHalf>,
) -> anyhow::Result<f64> {
    let (_, property_reply) = send_mpv_command(
        mpv_writer,
        mpv_reader,
        r#"{ "command": ["get_property", "playback-time"] }"#,
    )
    .await?;

    let reply = serde_json::from_str::<serde_json::Value>(&property_reply)?;

    let seek = reply["data"].as_f64().ok_or(anyhow::anyhow!("not f64"))?;

    Ok(seek)
}

pub async fn get_paused(
    mpv_writer: &mut SendHalf,
    mpv_reader: &mut BufReader<RecvHalf>,
) -> anyhow::Result<bool> {
    let (_, property_reply) = send_mpv_command(
        mpv_writer,
        mpv_reader,
        r#"{ "command": ["get_property", "pause"] }"#,
    )
    .await?;

    let reply = serde_json::from_str::<serde_json::Value>(&property_reply)?;

    let is_paused = reply["data"].as_bool().ok_or(anyhow::anyhow!("not bool"))?;

    Ok(is_paused)
}

pub async fn handle_mpv_messages(
    message: MpvMessage,
    peer_id: EndpointId,
    sender: &GossipSender,
    mpv_writer: &mut SendHalf,
    mpv_reader: &mut BufReader<RecvHalf>,
) -> anyhow::Result<()> {
    match message {
        MpvMessage::Event(event) => match event {
            events::Event::PropertyChange(property_change) => match property_change {
                events::PropertyChange::Pause(is_paused) => {
                    sender
                        .broadcast(
                            postcard::to_allocvec(&crate::message::Event::StateUpdate(
                                StateUpdate::new(
                                    peer_id,
                                    get_playback_time(mpv_writer, mpv_reader).await?,
                                    is_paused,
                                    UpdateTrigger::UserAction,
                                ),
                            ))?
                            .into(),
                        )
                        .await?;
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

                if let Some(seek) = reply["data"].as_f64() {
                    sender
                        .broadcast(
                            postcard::to_allocvec(&crate::message::Event::StateUpdate(
                                StateUpdate::new(
                                    peer_id,
                                    seek,
                                    get_paused(mpv_writer, mpv_reader).await?,
                                    UpdateTrigger::UserAction,
                                ),
                            ))?
                            .into(),
                        )
                        .await?;
                }
            }
        },
    }

    Ok(())
}
