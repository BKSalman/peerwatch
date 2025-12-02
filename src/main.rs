use clap::{Parser, Subcommand};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, NameType, ToFsName, ToNsName, traits::tokio::Stream as _,
};
use n0_future::StreamExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::time::Instant;

use peerwatch::{
    PeerWatchNode, PeerWatchTicket, SKIP_NEXT_SEEK_EVENT,
    message::StateUpdate,
    mpv::{MpvMessage, get_paused, get_playback_time, handle_mpv_messages},
    send_mpv_command, sha256_from_file, wait_until_file_created,
};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Create { video: PathBuf },
    Join { video: PathBuf, ticket: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let mut node = PeerWatchNode::spawn().await?;

    let (video, ticket) = match &cli.command {
        Command::Create { video } => (video, PeerWatchTicket::new_random(&video)),
        Command::Join { video, ticket } => {
            let peerwatch_ticket = PeerWatchTicket::deserialize(&ticket).unwrap();
            let video_sha256 = sha256_from_file(&video).expect("Failed to read video file");
            if peerwatch_ticket.video_sha256 != video_sha256 {
                return Err(anyhow::anyhow!(
                    "ERROR: Provided video doesn't match host provided video"
                ));
            }

            (video, peerwatch_ticket)
        }
    };

    let mut our_ticket = ticket.clone();
    our_ticket.bootstrap = [node.endpoint_id()].into_iter().collect();
    println!("* ticket to join this chat:");
    println!("{}", our_ticket.serialize());

    println!("* waiting for peers ...");
    let (sender, mut receiver) = node.join(&ticket).await?;

    let (name_str, name) = if GenericFilePath::is_supported() {
        let id: u16 = rand::random();
        (
            format!("/tmp/peer-{id}"),
            format!("/tmp/peer-{id}")
                .to_fs_name::<GenericFilePath>()
                .unwrap(),
        )
    } else {
        let id: u16 = rand::random();
        (
            format!("peer-{id}"),
            format!("peer-{id}")
                .to_ns_name::<GenericNamespaced>()
                .unwrap(),
        )
    };

    let child = std::process::Command::new("mpv")
        .arg(format!("--input-ipc-server={name_str}"))
        .arg(video)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_until_file_created(name_str).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (mpv_reader, mut mpv_writer) = interprocess::local_socket::tokio::Stream::connect(name)
        .await
        .unwrap()
        .split();
    let mut mpv_reader = BufReader::new(mpv_reader);

    send_mpv_command(
        &mut mpv_writer,
        &mut mpv_reader,
        r#"{ "command": ["observe_property", 0, "pause"] }"#,
    )
    .await
    .unwrap();

    let mut interval = tokio::time::interval(Duration::from_secs(5));

    let mut read_buf = String::with_capacity(128);

    let mut leader_interval = tokio::time::interval_at(
        Instant::now().checked_add(Duration::from_secs(10)).unwrap(),
        Duration::from_secs(10),
    );

    loop {
        tokio::select! {
            Ok(bytes_read) = mpv_reader.read_line(&mut read_buf) => {
                if bytes_read == 0 {
                    println!("mpv disconnected");
                    break Ok(());
                }

                if let Ok(message) = serde_json::from_str::<MpvMessage>(&read_buf) {
                    let _ = handle_mpv_messages(message, node.endpoint_id(), &sender, &mut mpv_writer, &mut mpv_reader).await;
                }

                read_buf.clear();
            }
            event = receiver.try_next() => {
                match event {
                    Ok(Some(peerwatch::message::Event::StateUpdate(state_update))) => {
                        send_mpv_command(
                            &mut mpv_writer,
                            &mut mpv_reader,
                            &format!(r#"{{ "command": ["set_property", "pause", {}] }}"#, state_update.is_paused),
                        )
                        .await
                        .unwrap();

                        let age_ms = state_update.age_ms();
                        println!("latency from peer {}: {age_ms}", state_update.peer_id);
                        let their_current_position = if !state_update.is_paused {
                            state_update.position + (age_ms as f64 / 1000.0)
                        } else {
                            state_update.position
                        };
                        let (_,_reply) = send_mpv_command(
                            &mut mpv_writer,
                            &mut mpv_reader,
                            &format!(r#"{{ "command": ["set_property", "playback-time", {their_current_position}] }}"#)
                        ) .await.unwrap();

                        SKIP_NEXT_SEEK_EVENT.store(true, std::sync::atomic::Ordering::Release);
                    }
                    Ok(Some(peerwatch::message::Event::Presence { timestamp_ms: _, endpoint_id })) => {
                        println!("{endpoint_id}: presence");

                        if node.leader == Some(endpoint_id) {
                            leader_interval.reset();
                        }

                        if node.peer_joined(endpoint_id) && node.is_leader() {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;

                            sender
                                .broadcast(
                                    postcard::to_allocvec(&peerwatch::message::Event::AnnounceLeader {
                                        timestamp_ms: now,
                                        endpoint_id: node.endpoint_id()
                                    })
                                    .unwrap()
                                    .into(),
                                )
                                .await
                                .unwrap();
                        }
                    },
                    Ok(Some(peerwatch::message::Event::PeerJoined { timestamp_ms: _, endpoint_id })) => {
                        println!("{endpoint_id}: joined");

                        if node.peer_joined(endpoint_id) && node.is_leader() {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64;
                            sender
                                .broadcast(
                                    postcard::to_allocvec(&peerwatch::message::Event::AnnounceLeader {
                                        timestamp_ms: now, endpoint_id: node.endpoint_id()
                                    })
                                    .unwrap()
                                    .into(),
                                )
                                .await
                                .unwrap();
                        }
                    },
                    Ok(Some(peerwatch::message::Event::PeerLeft { timestamp_ms: _, endpoint_id })) => {
                        println!("{endpoint_id}: left");
                        node.peer_left(&endpoint_id)
                    },
                    Ok(Some(peerwatch::message::Event::AnnounceLeader { timestamp_ms: _, endpoint_id })) => {
                        println!("{endpoint_id}: announcing leadership");
                        // TODO: add some challenge mechanism
                        // instead of just accepting the announced leader
                        node.leader = Some(endpoint_id);
                    },
                    _ => {}
                }
            }
            _ = interval.tick() => {
                if node.is_leader() {
                    let Ok(playback_time) =
                        get_playback_time(&mut mpv_writer, &mut mpv_reader).await else {
                        continue;
                    };

                    let Ok(is_paused) = get_paused(&mut mpv_writer, &mut mpv_reader).await else {
                        continue;
                    };

                    sender
                        .broadcast(
                            postcard::to_allocvec(&peerwatch::message::Event::StateUpdate(StateUpdate::new(
                                node.endpoint_id(),
                                    playback_time,
                                    is_paused,
                                    peerwatch::message::UpdateTrigger::Periodic,
                            )))
                            .unwrap()
                            .into(),
                        )
                        .await
                        .unwrap();
                }

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                sender
                    .broadcast(
                        postcard::to_allocvec(&peerwatch::message::Event::Presence {
                            timestamp_ms: now,
                            endpoint_id: node.endpoint_id(),
                        })
                        .unwrap()
                        .into(),
                    )
                    .await
                    .unwrap();
            }
            _ = leader_interval.tick() => {
                println!("alo");
                if !node.is_leader() {
                    if let Some(leader) = node.leader.clone() {
                        node.peer_left(&leader);
                    } else {
                        node.elect_leader();
                    }
                }
            }
        }
    }
}
