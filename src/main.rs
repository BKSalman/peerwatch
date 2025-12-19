use clap::Parser;
use peerwatch::Cli;
use peerwatch::app::App;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match &cli.command {
        peerwatch::Command::Create { video } => {
            eframe::run_native(
                "PeerWatch",
                eframe::NativeOptions::default(),
                Box::new(|cc| Ok(Box::new(App::new(cc, video.clone())))),
            )
            .unwrap();
        }
        peerwatch::Command::Join { ticket } => {}
    }

    Ok(())
}
