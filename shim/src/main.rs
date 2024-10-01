use std::{
    env,
    hash::{DefaultHasher, Hash, Hasher},
    io::{stdout, Write},
    os::{
        fd::{FromRawFd, RawFd},
        unix::net::UnixListener,
    },
    path::PathBuf,
    process::{ExitCode, Stdio},
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use command_fds::{CommandFdExt, FdMapping};
use nix::{sys::prctl::set_child_subreaper, unistd::setsid};
use service::TaskService;
use shim_protos::proto::task_server::TaskServer;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::error;
use utils::ExitSignal;

mod container;
mod service;
mod signal;
mod utils;

const SOCKET_ROOT: &str = "/run/shim";
const SOCKET_FD: RawFd = 3;

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

    /// Command to run.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start a task.
    Start,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    let result = match args.command {
        Some(Command::Start) => start(args),
        // when the shim is started as a daemon, the command is not specified
        None => start_daemon(args),
    };

    match result {
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
    let socket_path = PathBuf::from(SOCKET_ROOT).join(format!("{}.sock", hash));
    std::fs::create_dir_all(SOCKET_ROOT).context("Failed to create socket root")?;
    let uds = UnixListener::bind(&socket_path).context("Failed to bind socket")?;
    let socket_addr = format!("unix://{}", socket_path.display());
    stdout().write_all(socket_addr.as_bytes())?;
    stdout().flush()?;
    let cmd = env::current_exe().context("Failed to get current executable")?;
    let cwd = env::current_dir().context("Failed to get current directory")?;
    let mut command = std::process::Command::new(cmd);
    command.current_dir(cwd);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .arg("--runtime")
        .arg(args.runtime)
        .arg("--id")
        .arg(args.id);
    command
        .fd_mappings(vec![FdMapping {
            parent_fd: uds.into(),
            child_fd: SOCKET_FD,
        }])
        .context("Failed to set fd mapping")?;
    let _child = command.spawn().context("Failed to spawn shim")?;
    Ok(())
}

#[tokio::main]
async fn start_daemon(args: Args) -> Result<()> {
    setsid().context("Failed to setsid")?;
    set_child_subreaper(true).context("Failed to set subreaper")?;

    let shutdown_signal = Arc::new(ExitSignal::default());
    let task_service = TaskService::new(args.runtime, shutdown_signal.clone());

    let std_uds = unsafe { UnixListener::from_raw_fd(SOCKET_FD) };
    std_uds.set_nonblocking(true)?;
    let uds = tokio::net::UnixListener::from_std(std_uds)?;
    let uds_stream = UnixListenerStream::new(uds);

    Server::builder()
        .add_service(TaskServer::new(task_service))
        .serve_with_incoming_shutdown(uds_stream, shutdown_signal.wait())
        .await?;

    // TODO: delete socket

    Ok(())
}
