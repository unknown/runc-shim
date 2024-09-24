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
        signal::{kill, Signal},
        stat::Mode,
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{close, dup2, execv, fork, pipe2, setsid, ForkResult, Pid},
};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    signal::unix::{signal, SignalKind},
    time::sleep,
};
use tracing::{debug, error, info, warn};

const PID_FILE: &str = "container.pid";
const STDOUT_FILE: &str = "stdout.log";
const STDERR_FILE: &str = "stderr.log";

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
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "debug".into()))
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
                CString::new("--pid-file")?,
                CString::new(args.bundle.join(PID_FILE).to_string_lossy().to_string())?,
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

    monitor(args.bundle, pipe_out.0, pipe_err.0)
}

#[tokio::main]
async fn monitor(bundle: PathBuf, stdout_fd: OwnedFd, stderr_fd: OwnedFd) -> Result<()> {
    let container_pid = read_pid(bundle.join(PID_FILE)).await?;

    let mut sigchld = signal(SignalKind::child())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigquit = signal(SignalKind::quit())?;

    let mut stdout = unsafe { File::from_raw_fd(stdout_fd.into_raw_fd()) };
    let mut stderr = unsafe { File::from_raw_fd(stderr_fd.into_raw_fd()) };
    let mut stdout_reader = BufReader::new(&mut stdout).lines();
    let mut stderr_reader = BufReader::new(&mut stderr).lines();
    let mut stdout_writer = BufWriter::new(stdio_file(bundle.join(STDOUT_FILE)).await?);
    let mut stderr_writer = BufWriter::new(stdio_file(bundle.join(STDERR_FILE)).await?);

    debug!("Monitoring container");
    let mut container_status = None;
    loop {
        tokio::select! {
            _ = sigchld.recv() => {
                let status = waitpid(container_pid, Some(WaitPidFlag::WNOHANG)).context("Failed to get container status")?;
                container_status = Some(status);
            }
            _ = sigint.recv() => {
                forward_signal(container_pid, Signal::SIGINT);
            }
            _ = sigterm.recv() => {
                forward_signal(container_pid, Signal::SIGTERM);
            }
            _ = sigquit.recv() => {
                forward_signal(container_pid, Signal::SIGQUIT);
            }
            Ok(Some(line)) = stdout_reader.next_line() => {
                stdout_writer.write_all(line.as_bytes()).await?;
                stdout_writer.write_u8(b'\n').await?;
                stdout_writer.flush().await?;
            }
            Ok(Some(line)) = stderr_reader.next_line() => {
                stderr_writer.write_all(line.as_bytes()).await?;
                stderr_writer.write_u8(b'\n').await?;
                stderr_writer.flush().await?;
            }
            // allow for a delay so that all stdio is handled before exiting
            _ = sleep(Duration::from_millis(500)), if container_status.is_some() => {
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

async fn read_pid<P: AsRef<Path>>(path: P) -> Result<Pid> {
    let contents = fs::read_to_string(path).await?;
    Ok(Pid::from_raw(contents.parse()?))
}

fn forward_signal(container_pid: Pid, signal: Signal) -> () {
    match kill(container_pid, signal) {
        Ok(()) => (),
        Err(nix::Error::ESRCH) => {
            warn!("Container {} not found, ignoring signal", container_pid);
        }
        Err(err) => {
            error!(
                "Failed to forward signal to container {}: {}",
                container_pid, err
            );
        }
    }
}

async fn stdio_file<P: AsRef<Path>>(path: P) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .await?;
    Ok(file)
}
