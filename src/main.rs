use clap::{Parser, Subcommand};
use glutin::prelude::GlDisplay;
use interprocess::local_socket::{
    GenericFilePath, GenericNamespaced, NameType, ToFsName, ToNsName, traits::tokio::Stream as _,
};
use libmpv2::Mpv;
use libmpv2::render::OpenGLInitParams;
use libmpv2::render::RenderContext;
use libmpv2::render::RenderParam;
use libmpv2::render::RenderParamApiType;
use n0_future::StreamExt;
use peerwatch::app::App;
use std::ffi::CString;
use std::ffi::c_void;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::time::Instant;
use winit::event_loop::ControlFlow;

use peerwatch::{
    PeerWatchNode, PeerWatchTicket, SKIP_NEXT_SEEK_EVENT,
    mpv::{MpvMessage, get_paused, get_playback_time, handle_mpv_messages},
    peer_event::StateUpdate,
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // let mut node = PeerWatchNode::spawn().await?;

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

    // let mut our_ticket = ticket.clone();
    // our_ticket.bootstrap = [node.endpoint_id()].into_iter().collect();
    // println!("* ticket to join this chat:");
    // println!("{}", our_ticket.serialize());

    // println!("* waiting for peers ...");
    // let (sender, mut receiver) = node.join(&ticket).await?;

    // let (name_str, name) = if GenericFilePath::is_supported() {
    //     let id: u16 = rand::random();
    //     (
    //         format!("/tmp/peer-{id}"),
    //         format!("/tmp/peer-{id}")
    //             .to_fs_name::<GenericFilePath>()
    //             .unwrap(),
    //     )
    // } else {
    //     let id: u16 = rand::random();
    //     (
    //         format!("peer-{id}"),
    //         format!("peer-{id}")
    //             .to_ns_name::<GenericNamespaced>()
    //             .unwrap(),
    //     )
    // };

    // let child = std::process::Command::new("mpv")
    //     .arg(format!("--input-ipc-server={name_str}"))
    //     .arg(video)
    //     .stdout(Stdio::null())
    //     .stderr(Stdio::null())
    //     .spawn()
    //     .unwrap();

    // wait_until_file_created(name_str).unwrap();
    // tokio::time::sleep(Duration::from_secs(1)).await;

    // let (mpv_reader, mut mpv_writer) = interprocess::local_socket::tokio::Stream::connect(name)
    //     .await
    //     .unwrap()
    //     .split();
    // let mut mpv_reader = BufReader::new(mpv_reader);

    // send_mpv_command(
    //     &mut mpv_writer,
    //     &mut mpv_reader,
    //     r#"{ "command": ["observe_property", 0, "pause"] }"#,
    // )
    // .await
    // .unwrap();

    // let mut interval = tokio::time::interval(Duration::from_secs(5));

    // let mut read_buf = String::with_capacity(128);

    // let mut leader_interval = tokio::time::interval_at(
    //     Instant::now().checked_add(Duration::from_secs(10)).unwrap(),
    //     Duration::from_secs(10),
    // );

    let (gl_display, gl_surface, gl_context, _shader_version, _window, event_loop) = {
        use glutin::{
            config::{ConfigTemplateBuilder, GlConfig},
            context::{ContextApi, ContextAttributesBuilder, NotCurrentGlContext},
            display::{GetGlDisplay, GlDisplay},
            surface::{GlSurface, SwapInterval},
        };
        use glutin_winit::{DisplayBuilder, GlWindow};
        use raw_window_handle::HasWindowHandle;
        use std::num::NonZeroU32;

        let event_loop = winit::event_loop::EventLoop::<peerwatch::Event>::with_user_event()
            .build()
            .unwrap();
        let window_attributes = winit::window::Window::default_attributes().with_title("PeerWatch");

        let template = ConfigTemplateBuilder::new();

        let display_builder = DisplayBuilder::new().with_window_attributes(Some(window_attributes));

        let (window, gl_config) = display_builder
            .build(&event_loop, template, |configs| {
                configs
                    .reduce(|accum, config| {
                        if config.num_samples() > accum.num_samples() {
                            config
                        } else {
                            accum
                        }
                    })
                    .unwrap()
            })
            .unwrap();

        let raw_window_handle = window
            .as_ref()
            .map(|window| window.window_handle())
            .transpose()
            .unwrap()
            .map(|w| w.as_raw());

        let gl_display = gl_config.display();
        let context_attributes = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(glutin::context::Version {
                major: 4,
                minor: 1,
            })))
            .build(raw_window_handle);

        let not_current_gl_context = unsafe {
            gl_display
                .create_context(&gl_config, &context_attributes)
                .unwrap()
        };

        let window = window.unwrap();

        let attrs = window.build_surface_attributes(Default::default()).unwrap();
        let gl_surface = unsafe {
            gl_display
                .create_window_surface(&gl_config, &attrs)
                .unwrap()
        };

        let gl_context = not_current_gl_context.make_current(&gl_surface).unwrap();

        // let gl =
        //     unsafe { glow::Context::from_loader_function_cstr(|s| gl_display.get_proc_address(s)) };

        gl_surface
            .set_swap_interval(&gl_context, SwapInterval::Wait(NonZeroU32::new(1).unwrap()))
            .unwrap();

        (
            // gl,
            gl_display,
            gl_surface,
            gl_context,
            "#version 410",
            window,
            event_loop,
        )
    };

    event_loop.set_control_flow(ControlFlow::Wait);

    let mut mpv = Mpv::with_initializer(|init| {
        init.set_property("vo", "libmpv")?;
        Ok(())
    })
    .unwrap();

    let mut render_context = RenderContext::new(
        unsafe { mpv.ctx.as_mut() },
        vec![
            RenderParam::ApiType(RenderParamApiType::OpenGl),
            RenderParam::InitParams(OpenGLInitParams {
                get_proc_address,
                ctx: gl_display,
            }),
        ],
    )
    .expect("Failed creating render context");

    mpv.disable_deprecated_events().unwrap();

    let event_loop_proxy = event_loop.create_proxy();
    render_context.set_update_callback(move || {
        event_loop_proxy
            .send_event(peerwatch::Event::RedrawRequested)
            .unwrap();
    });

    mpv.command("loadfile", &[video.to_str().unwrap(), "replace"])
        .unwrap();

    let mpv = Arc::new(mpv);

    let mut app = App::new(render_context, gl_surface, gl_context, mpv);
    event_loop.run_app(&mut app).unwrap();

    // loop {
    //     tokio::select! {
    //         Ok(bytes_read) = mpv_reader.read_line(&mut read_buf) => {
    //             if bytes_read == 0 {
    //                 println!("mpv disconnected");
    //                 break Ok(());
    //             }

    //             if let Ok(message) = serde_json::from_str::<MpvMessage>(&read_buf) {
    //                 let _ = handle_mpv_messages(message, node.endpoint_id(), &sender, &mut mpv_writer, &mut mpv_reader).await;
    //             }

    //             read_buf.clear();
    //         }
    //         event = receiver.try_next() => {
    //             match event {
    //                 Ok(Some(peerwatch::peer_event::PeerEvent::StateUpdate(state_update))) => {
    //                     send_mpv_command(
    //                         &mut mpv_writer,
    //                         &mut mpv_reader,
    //                         &format!(r#"{{ "command": ["set_property", "pause", {}] }}"#, state_update.is_paused),
    //                     )
    //                     .await
    //                     .unwrap();

    //                     let age_ms = state_update.age_ms();
    //                     println!("latency from peer {}: {age_ms}", state_update.peer_id);
    //                     let their_current_position = if !state_update.is_paused {
    //                         state_update.position + (age_ms as f64 / 1000.0)
    //                     } else {
    //                         state_update.position
    //                     };
    //                     let (_,_reply) = send_mpv_command(
    //                         &mut mpv_writer,
    //                         &mut mpv_reader,
    //                         &format!(r#"{{ "command": ["set_property", "playback-time", {their_current_position}] }}"#)
    //                     ) .await.unwrap();

    //                     SKIP_NEXT_SEEK_EVENT.store(true, std::sync::atomic::Ordering::Release);
    //                 }
    //                 Ok(Some(peerwatch::peer_event::PeerEvent::Presence { timestamp_ms: _, endpoint_id })) => {
    //                     println!("{endpoint_id}: presence");

    //                     if node.leader == Some(endpoint_id) {
    //                         leader_interval.reset();
    //                     }

    //                     if node.peer_joined(endpoint_id) && node.is_leader() {
    //                         let now = SystemTime::now()
    //                             .duration_since(UNIX_EPOCH)
    //                             .unwrap()
    //                             .as_millis() as u64;

    //                         sender
    //                             .broadcast(
    //                                 postcard::to_allocvec(&peerwatch::peer_event::PeerEvent::AnnounceLeader {
    //                                     timestamp_ms: now,
    //                                     endpoint_id: node.endpoint_id()
    //                                 })
    //                                 .unwrap()
    //                                 .into(),
    //                             )
    //                             .await
    //                             .unwrap();
    //                     }
    //                 },
    //                 Ok(Some(peerwatch::peer_event::PeerEvent::PeerJoined { timestamp_ms: _, endpoint_id })) => {
    //                     println!("{endpoint_id}: joined");

    //                     if node.peer_joined(endpoint_id) && node.is_leader() {
    //                         let now = SystemTime::now()
    //                             .duration_since(UNIX_EPOCH)
    //                             .unwrap()
    //                             .as_millis() as u64;
    //                         sender
    //                             .broadcast(
    //                                 postcard::to_allocvec(&peerwatch::peer_event::PeerEvent::AnnounceLeader {
    //                                     timestamp_ms: now, endpoint_id: node.endpoint_id()
    //                                 })
    //                                 .unwrap()
    //                                 .into(),
    //                             )
    //                             .await
    //                             .unwrap();
    //                     }
    //                 },
    //                 Ok(Some(peerwatch::peer_event::PeerEvent::PeerLeft { timestamp_ms: _, endpoint_id })) => {
    //                     println!("{endpoint_id}: left");
    //                     node.peer_left(&endpoint_id)
    //                 },
    //                 Ok(Some(peerwatch::peer_event::PeerEvent::AnnounceLeader { timestamp_ms: _, endpoint_id })) => {
    //                     println!("{endpoint_id}: announcing leadership");
    //                     // TODO: add some challenge mechanism
    //                     // instead of just accepting the announced leader
    //                     node.leader = Some(endpoint_id);
    //                 },
    //                 _ => {}
    //             }
    //         }
    //         _ = interval.tick() => {
    //             if node.is_leader() {
    //                 let Ok(playback_time) =
    //                     get_playback_time(&mut mpv_writer, &mut mpv_reader).await else {
    //                     continue;
    //                 };

    //                 let Ok(is_paused) = get_paused(&mut mpv_writer, &mut mpv_reader).await else {
    //                     continue;
    //                 };

    //                 sender
    //                     .broadcast(
    //                         postcard::to_allocvec(&peerwatch::peer_event::PeerEvent::StateUpdate(StateUpdate::new(
    //                             node.endpoint_id(),
    //                                 playback_time,
    //                                 is_paused,
    //                                 peerwatch::peer_event::UpdateTrigger::Periodic,
    //                         )))
    //                         .unwrap()
    //                         .into(),
    //                     )
    //                     .await
    //                     .unwrap();
    //             }

    //             let now = SystemTime::now()
    //                 .duration_since(UNIX_EPOCH)
    //                 .unwrap()
    //                 .as_millis() as u64;

    //             sender
    //                 .broadcast(
    //                     postcard::to_allocvec(&peerwatch::peer_event::PeerEvent::Presence {
    //                         timestamp_ms: now,
    //                         endpoint_id: node.endpoint_id(),
    //                     })
    //                     .unwrap()
    //                     .into(),
    //                 )
    //                 .await
    //                 .unwrap();
    //         }
    //         _ = leader_interval.tick() => {
    //             println!("alo");
    //             if !node.is_leader() {
    //                 if let Some(leader) = node.leader.clone() {
    //                     node.peer_left(&leader);
    //                 } else {
    //                     node.elect_leader();
    //                 }
    //             }
    //         }
    //     }
    // }

    Ok(())
}

pub fn get_proc_address(display: &glutin::display::Display, name: &str) -> *mut c_void {
    let name = CString::new(name).unwrap();
    display.get_proc_address(&name).cast_mut()
}
