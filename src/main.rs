use std::{
    ffi::CString,
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::{exit, ExitCode},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use nix::{
    fcntl::{open, OFlag},
    libc,
    sys::{
        prctl::set_child_subreaper,
        stat::Mode,
        wait::{waitpid, WaitStatus},
    },
    unistd::{close, dup2, execv, fork, pipe2, setsid, ForkResult, Pid},
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    signal::unix::{signal, SignalKind},
    time::sleep,
};
use tracing::{debug, error, info};

/// Shim process for running containers.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Path to runtime executable.
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
            info!("Shim PID: {}", child);
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

    let pipe_out = pipe2(OFlag::O_CLOEXEC).context("Failed to create pipe")?;
    let pipe_err = pipe2(OFlag::O_CLOEXEC).context("Failed to create pipe")?;

    let pid = match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => child,
        Ok(ForkResult::Child) => {
            let dev_null = open("/dev/null", OFlag::O_RDWR, Mode::empty())?;
            dup2(dev_null, libc::STDIN_FILENO).context("Failed to dup stdin")?;
            dup2(pipe_out.1.as_raw_fd(), libc::STDOUT_FILENO).context("Failed to dup stdout")?;
            dup2(pipe_err.1.as_raw_fd(), libc::STDERR_FILENO).context("Failed to dup stderr")?;
            close(dev_null).context("Failed to close dev null")?;
            let argv = [
                CString::new(args.runtime.to_string_lossy().to_string())?,
                CString::new("create")?,
                CString::new("--bundle")?,
                CString::new(args.bundle.to_string_lossy().to_string())?,
                CString::new(args.id.as_str())?,
            ];
            if let Err(err) = execv(&argv[0], &argv) {
                bail!("Failed to exec runc: {}", err);
            }
            unsafe { libc::_exit(127) }
        }
        Err(err) => bail!("Failed to fork: {}", err),
    };

    drop(pipe_out.1);
    drop(pipe_err.1);

    match waitpid(pid, None) {
        Ok(WaitStatus::Exited(_, 0)) => {}
        Ok(WaitStatus::Exited(_, status)) => bail!("Runtime exited with status {}", status),
        Ok(WaitStatus::Signaled(_, signal, _)) => bail!("Runtime exited with signal {}", signal),
        _ => bail!("Runtime exited with unknown status"),
    };

    monitor(pipe_out.0, pipe_err.0)
}

#[tokio::main]
async fn monitor(stdout_fd: OwnedFd, stderr_fd: OwnedFd) -> Result<()> {
    let mut sigchld = signal(SignalKind::child())?;
    let mut stdout = unsafe { File::from_raw_fd(stdout_fd.into_raw_fd()) };
    let mut stderr = unsafe { File::from_raw_fd(stderr_fd.into_raw_fd()) };
    let mut stdout_reader = BufReader::new(&mut stdout).lines();
    let mut stderr_reader = BufReader::new(&mut stderr).lines();
    let mut stdout_writer = BufWriter::new(stdio_file("/tmp/stdout.log").await?);
    let mut stderr_writer = BufWriter::new(stdio_file("/tmp/stderr.log").await?);

    debug!("Monitoring container");
    let mut container_status = None;
    loop {
        tokio::select! {
            _ = sigchld.recv() => {
                let status = waitpid(Pid::from_raw(-1), None).context("Failed to get container status")?;
                container_status = Some(status);
            }
            Ok(Some(line)) = stdout_reader.next_line() => {
                stdout_writer.write_all(line.as_bytes()).await?;
                stdout_writer.write_u8(b'\n').await?;
            }
            Ok(Some(line)) = stderr_reader.next_line() => {
                stderr_writer.write_all(line.as_bytes()).await?;
                stderr_writer.write_u8(b'\n').await?;
            }
            // allow for a delay so that all stdio is handled before exiting
            Some(_) = delayed_exit(container_status, Duration::from_millis(500)) => {
                stdout_writer.flush().await?;
                stderr_writer.flush().await?;
                break;
            }
        }
    }

    match container_status.unwrap() {
        WaitStatus::Exited(pid, status) => {
            info!("Container {} exited with status {}", pid, status);
        }
        WaitStatus::Signaled(pid, signal, _) => {
            info!("Container {} exited with signal {}", pid, signal);
        }
        _ => {
            info!("Container exited with unknown status");
        }
    };

    Ok(())
}

async fn stdio_file<P: AsRef<Path>>(path: P) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .await?;
    Ok(file)
}

async fn delayed_exit(status: Option<WaitStatus>, duration: Duration) -> Option<()> {
    match status {
        Some(_) => Some(sleep(duration).await),
        None => None,
    }
}
