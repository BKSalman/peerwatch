use iroh::SecretKey;
use iroh_gossip::api::Event;
use n0_future::StreamExt;
use peerwatch::{PeerPlayTicket, send_mpv_command, wait_until_file_created};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::time::interval;

use clap::{Parser, Subcommand};
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, NameType, ToFsName, ToNsName, traits::tokio::Stream as _,
};
use iroh::protocol::Router;
use iroh_gossip::Gossip;
use tokio::io::BufReader;

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

    let (read, mut write) = interprocess::local_socket::tokio::Stream::connect(name)
        .await
        .unwrap()
        .split();
    let mut read = BufReader::new(read);

    send_mpv_command(
        &mut write,
        &mut read,
        r#"{ "command": ["observe_property", 0, "pause"] }"#,
    )
    .await
    .unwrap();

    send_mpv_command(
        &mut write,
        &mut read,
        r#"{ "command": ["get_property", "playback-time"] }"#,
    )
    .await
    .unwrap();

    let mut interval = interval(Duration::from_secs(5));

    let mut read_buf = String::with_capacity(128);

    loop {
        tokio::select! {
            Ok(bytes_read) = read.read_line(&mut read_buf) => {
                if bytes_read == 0 {
                    break;
                }

                if let Ok(message) = serde_json::from_str::<peerwatch::mpv::Message>(&read_buf) {
                    match message {
                        peerwatch::mpv::Message::Event(event) => {
                            match event {
                                peerwatch::mpv::events::Event::PropertyChange(property_change) => {
                                    match property_change {
                                        peerwatch::mpv::events::PropertyChange::Pause(is_paused) => {
                                            if is_paused {
                                                sender
                                                    .broadcast(
                                                        postcard::to_allocvec(&peerwatch::message::Message::new(peerwatch::message::Payload::Pause))
                                                            .unwrap()
                                                            .into(),
                                                    )
                                                    .await
                                                    .unwrap();
                                            } else {
                                                sender
                                                    .broadcast(
                                                        postcard::to_allocvec(&peerwatch::message::Message::new(peerwatch::message::Payload::Play))
                                                            .unwrap()
                                                            .into(),
                                                    )
                                                    .await
                                                    .unwrap();
                                            }
                                            println!("sent: {is_paused}");
                                        },
                                    }
                                },
                            }
                        },
                    }
                } else {
                    println!("read {bytes_read} bytes, message: {read_buf}");
                }

                read_buf.clear();
            }
            event = receiver.try_next() => {
                match event {
                    Ok(Some(Event::Received(message))) => {
                        let message: peerwatch::message::Message =
                            postcard::from_bytes(&message.content).unwrap();
                        println!("got message: {message:?}");
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
                sender
                    .broadcast(
                        postcard::to_allocvec(&peerwatch::message::Message::new(peerwatch::message::Payload::Ping))
                            .unwrap()
                            .into(),
                    )
                    .await
                    .unwrap();
            }
        }
    }
}
