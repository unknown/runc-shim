use std::{
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result};
use nix::{
    sys::{
        signal::{kill, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::Pid,
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    signal::unix::{signal, SignalKind},
    time::sleep,
};
use tracing::{debug, error, info, warn};

use super::Container;

const STDOUT_FILE: &str = "stdout.log";
const STDERR_FILE: &str = "stderr.log";

impl Container {
    pub async fn monitor(&self) -> Result<()> {
        let container_pid = self.pid.context("Missing container PID")?;
        let stdout_fd = self.stdout.as_ref().context("Missing stdout")?.as_raw_fd();
        let stderr_fd = self.stderr.as_ref().context("Missing stderr")?.as_raw_fd();

        let mut sigchld = signal(SignalKind::child())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigquit = signal(SignalKind::quit())?;

        let mut stdout = unsafe { File::from_raw_fd(stdout_fd) };
        let mut stderr = unsafe { File::from_raw_fd(stderr_fd) };
        let mut stdout_reader = BufReader::new(&mut stdout).lines();
        let mut stderr_reader = BufReader::new(&mut stderr).lines();
        let mut stdout_writer = BufWriter::new(stdio_file(self.bundle.join(STDOUT_FILE)).await?);
        let mut stderr_writer = BufWriter::new(stdio_file(self.bundle.join(STDERR_FILE)).await?);

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
