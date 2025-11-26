use clap::{Parser, Subcommand};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, NameType, ToFsName, ToNsName, traits::tokio::Stream as _,
};
use iroh::SecretKey;
use iroh::protocol::Router;
use iroh_gossip::Gossip;
use iroh_gossip::api::Event;
use n0_future::StreamExt;
use peerwatch::message::Message;
use peerwatch::mpv::get_paused;
use peerwatch::mpv::get_playback_time;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::time::interval;

use peerwatch::message::Payload;
use peerwatch::{
    PeerPlayTicket, SKIP_NEXT_SEEK_EVENT, mpv::handle_mpv_messages, send_mpv_command,
    wait_until_file_created,
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
async fn main() {
    let cli = Cli::parse();

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

    let (video, ticket) = match &cli.command {
        Command::Create { video } => (video, PeerPlayTicket::new_random()),
        Command::Join { video, ticket } => (video, PeerPlayTicket::deserialize(&ticket).unwrap()),
    };

    let mut our_ticket = ticket.clone();
    our_ticket.bootstrap = [router.endpoint().id()].into_iter().collect();
    println!("* ticket to join this chat:");
    println!("{}", our_ticket.serialize());

    let bootstrap = ticket.bootstrap.iter().cloned().collect();

    println!("* waiting for peers ...");
    let (sender, mut receiver) = match cli.command {
        Command::Create { .. } => gossip
            .subscribe(ticket.topic_id, bootstrap)
            .await
            .unwrap()
            .split(),
        Command::Join { .. } => gossip
            .subscribe_and_join(ticket.topic_id, bootstrap)
            .await
            .unwrap()
            .split(),
    };

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

    send_mpv_command(
        &mut mpv_writer,
        &mut mpv_reader,
        r#"{ "command": ["get_property", "playback-time"] }"#,
    )
    .await
    .unwrap();

    let mut interval = interval(Duration::from_secs(5));

    let mut read_buf = String::with_capacity(128);

    loop {
        tokio::select! {
            Ok(bytes_read) = mpv_reader.read_line(&mut read_buf) => {
                if bytes_read == 0 {
                    break;
                }

                handle_mpv_messages(router.endpoint().id(), &sender, &read_buf, &mut mpv_writer, &mut mpv_reader).await.unwrap();

                read_buf.clear();
            }
            event = receiver.try_next() => {
                match event {
                    Ok(Some(Event::Received(message))) => {
                        let message: Message = postcard::from_bytes(&message.content).unwrap();
                        match message.payload {
                            peerwatch::message::Payload::StateUpdate { position, is_paused, trigger: _ } => {
                                send_mpv_command(
                                    &mut mpv_writer,
                                    &mut mpv_reader,
                                    &format!(r#"{{ "command": ["set_property", "pause", {is_paused}] }}"#),
                                ).await.unwrap();

                                let age_ms = message.age_ms();
                                let their_current_position = if !is_paused {
                                    position + (age_ms as f64 / 1000.0) // * playback_rate as f64
                                } else {
                                    position
                                };

                                // NOTE: could soft sync by adjusting speed instead of seeking
                                let (_, _reply) = send_mpv_command(
                                    &mut mpv_writer,
                                    &mut mpv_reader,
                                    &format!(r#"{{ "command": ["set_property", "playback-time", {their_current_position}] }}"#),
                                ).await.unwrap();

                                SKIP_NEXT_SEEK_EVENT.store(true, std::sync::atomic::Ordering::Release);
                            },
                        }
                    }
                    Ok(Some(msg)) => {
                        println!("got gossip message: {msg:?}");
                    }
                    Ok(None) => {
                        println!("gossip stream ended");
                        break;
                    }
                    Err(e) => {
                        println!("gossip error: {e:?}");
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                let Ok(playback_time) =
                    get_playback_time(&mut mpv_writer, &mut mpv_reader).await else {
                    continue;
                };

                let Ok(is_paused) = get_paused(&mut mpv_writer, &mut mpv_reader).await else {
                    continue;
                };

                sender
                    .broadcast(
                        postcard::to_allocvec(&Message::new(
                            router.endpoint().id(),
                            Payload::StateUpdate {
                                position: playback_time,
                                is_paused,
                                trigger: peerwatch::message::UpdateTrigger::Periodic,
                            },
                        ))
                        .unwrap()
                        .into(),
                    )
                    .await
                    .unwrap();
            }
        }
    }
}
