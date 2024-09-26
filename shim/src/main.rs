use std::{
    net::SocketAddr,
    path::PathBuf,
    process::{exit, ExitCode},
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
use tonic::transport::Server;
use tracing::error;

mod container;
mod service;
mod signal;

/// Shim process for running containers.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to OCI runtime executable.
    #[arg(short, long, default_value = "/usr/sbin/runc")]
    runtime: PathBuf,
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
    let addr = "[::1]:50051".parse().unwrap();

    match unsafe { fork() } {
        Ok(ForkResult::Parent { .. }) => {
            println!("{}", addr);
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

    start_service(args, addr).context("Failed to start container")?;
    Ok(())
}

#[tokio::main]
async fn start_service(args: Args, addr: SocketAddr) -> Result<()> {
    let task_service = TaskService::new(args.runtime);
    Server::builder()
        .add_service(TaskServer::new(task_service))
        .serve(addr)
        .await?;
    Ok(())
}
