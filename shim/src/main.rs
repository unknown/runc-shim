use std::{
    path::PathBuf,
    process::{exit, ExitCode},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use container::Container;
use nix::{
    fcntl::{open, OFlag},
    libc,
    sys::{prctl::set_child_subreaper, stat::Mode},
    unistd::{close, dup2, fork, setsid, ForkResult},
};
use tokio::sync::{mpsc, Mutex};
use tracing::error;

mod container;

/// Shim process for running containers.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to OCI runtime executable.
    #[arg(short, long, default_value = "/usr/sbin/runc")]
    runtime: PathBuf,

    /// Path to the bundle directory.
    #[arg(short, long)]
    bundle: PathBuf,

    /// Name of the container.
    #[arg(long)]
    id: String,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    match start(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{:?}", err);
            ExitCode::FAILURE
        }
    }
}

fn start(args: Args) -> Result<()> {
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            println!("Shim PID: {}", child);
            exit(0);
        }
        Ok(ForkResult::Child) => (),
        Err(err) => bail!("Failed to fork: {}", err),
    }

    let dev_null = open("/dev/null", OFlag::O_RDWR, Mode::empty())?;
    dup2(dev_null, libc::STDIN_FILENO).context("Failed to dup stdin")?;
    dup2(dev_null, libc::STDOUT_FILENO).context("Failed to dup stdout")?;
    dup2(dev_null, libc::STDERR_FILENO).context("Failed to dup stderr")?;
    close(dev_null).context("Failed to close dev null")?;

    setsid().context("Failed to setsid")?;
    set_child_subreaper(true).context("Failed to set subreaper")?;

    start_container(args).context("Failed to start container")
}

#[tokio::main]
async fn start_container(args: Args) -> Result<()> {
    let wait_channels = Arc::new(Mutex::new(Vec::new()));
    let mut container = Container::new(&args.id, &args.bundle);
    container
        .start(&args.runtime)
        .await
        .context("Failed to start container")?;

    let wait_channels_clone = wait_channels.clone();
    tokio::spawn(async move { container.handle_signals(wait_channels_clone).await });

    let (tx, mut rx) = mpsc::unbounded_channel();
    wait_channels.lock().await.push(tx);

    rx.recv().await.context("Failed to receive exit signal")?;

    Ok(())
}
