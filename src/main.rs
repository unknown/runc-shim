use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::{Path, PathBuf},
    process::{exit, ExitCode},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use nix::{
    fcntl::{open, OFlag},
    libc,
    sys::{
        prctl::set_child_subreaper,
        signal::{sigprocmask, Signal},
        signalfd::SignalFd,
        stat::Mode,
        wait::{waitpid, WaitStatus},
    },
    unistd::{close, dup2, execv, fork, pipe2, setsid, ForkResult, Pid},
};
use tracing::{debug, error, info};

const TOKEN_STDOUT: Token = Token(0);
const TOKEN_STDERR: Token = Token(1);
const TOKEN_SIGNAL: Token = Token(2);

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

fn stdio_file<P: AsRef<Path>>(path: P) -> Result<File> {
    Ok(OpenOptions::new().create(true).write(true).open(path)?)
}

fn read_pipe(pipe: &OwnedFd, buf: &mut [u8]) -> Result<usize> {
    let mut pipe_file = unsafe { File::from_raw_fd(pipe.as_raw_fd()) };
    let bytes_read = pipe_file.read(buf)?;
    mem::forget(pipe_file); // prevent closing file descriptor
    Ok(bytes_read)
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

    let runtime_status = match waitpid(pid, None) {
        Ok(WaitStatus::Exited(_, status)) => status,
        _ => bail!("Runtime exited with unknown status"),
    };

    if runtime_status != 0 {
        bail!("Runtime exited with status {}", runtime_status);
    }

    let signals = Signal::SIGCHLD | Signal::SIGINT | Signal::SIGTERM;
    sigprocmask(
        nix::sys::signal::SigmaskHow::SIG_SETMASK,
        Some(&signals),
        None,
    )
    .context("Failed to set signal mask")?;
    let signalfd = SignalFd::new(&signals)?;

    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);

    poll.registry().register(
        &mut SourceFd(&pipe_out.0.as_raw_fd()),
        TOKEN_STDOUT,
        Interest::READABLE,
    )?;

    poll.registry().register(
        &mut SourceFd(&pipe_err.0.as_raw_fd()),
        TOKEN_STDERR,
        Interest::READABLE,
    )?;

    poll.registry().register(
        &mut SourceFd(&signalfd.as_raw_fd()),
        TOKEN_SIGNAL,
        Interest::READABLE,
    )?;

    let mut stdout_file = stdio_file("/tmp/stdout.log").context("Failed to open stdout file")?;
    let mut stderr_file = stdio_file("/tmp/stderr.log").context("Failed to open stderr file")?;
    let mut container_status = None;
    while container_status.is_none() {
        let mut buf = [0; 1024];
        poll.poll(&mut events, Some(Duration::from_millis(5000)))?;
        for event in &events {
            match event.token() {
                TOKEN_STDOUT if event.is_readable() => {
                    let bytes_read = read_pipe(&pipe_out.0, &mut buf)?;
                    stdout_file.write_all(&buf[..bytes_read])?;
                }
                TOKEN_STDERR if event.is_readable() => {
                    let bytes_read = read_pipe(&pipe_err.0, &mut buf)?;
                    stderr_file.write_all(&buf[..bytes_read])?;
                }
                TOKEN_SIGNAL => {
                    let signal = match signalfd.read_signal() {
                        Ok(Some(siginfo)) => Signal::try_from(siginfo.ssi_signo as i32),
                        Ok(None) => bail!("Failed to read signal"),
                        Err(err) => bail!(err),
                    }?;
                    match signal {
                        Signal::SIGCHLD => {
                            let status = waitpid(Pid::from_raw(-1), None)?;
                            container_status = Some(status);
                        }
                        _signal => {
                            // TODO: forward signal to container
                        }
                    }
                }
                _ => {}
            }
        }
        debug!("Done polling");
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

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();
    match start(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("Failed to start shim: {}", err);
            ExitCode::FAILURE
        }
    }
}
