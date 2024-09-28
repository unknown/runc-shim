use std::{
    hash::{DefaultHasher, Hash, Hasher},
    io::Write,
    path::PathBuf,
    process::{exit, ExitCode},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use nix::{
    fcntl::{open, OFlag},
    libc,
    sys::{prctl::set_child_subreaper, stat::Mode},
    unistd::{close, dup2, fork, setsid, ForkResult},
};
use service::TaskService;
use shim_protos::proto::task_server::TaskServer;
use tokio::{fs, net::UnixListener};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::error;
use utils::ExitSignal;

mod container;
mod service;
mod signal;
mod utils;

pub const SOCKET_ROOT: &str = "/run/shim";

/// Shim process for running containers.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to OCI runtime executable.
    #[arg(short, long, default_value = "/usr/sbin/runc")]
    runtime: PathBuf,

    /// ID of the task.
    #[arg(short, long)]
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
    let hash = {
        let mut hasher = DefaultHasher::new();
        args.id.hash(&mut hasher);
        hasher.finish()
    };
    let addr = format!("{}/{}.sock", SOCKET_ROOT, hash);

    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => {
            let mut stdout = std::io::stdout();
            let full_addr = format!("unix://{}", addr);
            stdout
                .write_all(full_addr.as_bytes())
                .context("Failed to write address to stdout")?;
            stdout.flush().context("Failed to flush stdout")?;
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

    start_service(args, &addr).context("Failed to start task service")?;
    Ok(())
}

#[tokio::main]
async fn start_service(args: Args, addr: &str) -> Result<()> {
    fs::create_dir_all(SOCKET_ROOT)
        .await
        .context("Failed to create socket root")?;
    let uds = UnixListener::bind(addr).context("Failed to bind socket")?;
    let uds_stream = UnixListenerStream::new(uds);

    let shutdown_signal = Arc::new(ExitSignal::default());
    let task_service = TaskService::new(args.runtime, shutdown_signal.clone());
    Server::builder()
        .add_service(TaskServer::new(task_service))
        .serve_with_incoming_shutdown(uds_stream, shutdown_signal.wait())
        .await?;

    Ok(())
}
