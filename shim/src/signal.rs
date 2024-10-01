use std::sync::Arc;

use anyhow::{Context, Result};
use nix::{
    sys::{
        signal::{kill, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::Pid,
};
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::{mpsc, Mutex},
};
use tracing::{debug, error, info, warn};

pub async fn handle_signals(
    container_pid: Pid,
    wait_channels: Arc<Mutex<Vec<mpsc::UnboundedSender<i32>>>>,
) -> Result<()> {
    let mut sigchld = signal(SignalKind::child())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigquit = signal(SignalKind::quit())?;

    loop {
        tokio::select! {
            _ = sigchld.recv() => {
                debug!("Received SIGCHLD");
                let status = waitpid(container_pid, Some(WaitPidFlag::WNOHANG)).context("Failed to get container status")?;
                let exit_code = match status {
                    WaitStatus::Exited(pid, status) => {
                        info!("Container {} exited with status {}", pid, status);
                        Some(status)
                    }
                    WaitStatus::Signaled(pid, signal, _) => {
                        info!("Container {} exited with signal {}", pid, signal);
                        Some(128 + signal as i32)
                    }
                    _ => {
                        info!("Container exited with unknown status");
                        None
                    }
                };
                if let Some(exit_code) = exit_code {
                    for sender in wait_channels.lock().await.iter() {
                        if let Err(err) = sender.send(exit_code) {
                            error!("Failed to send exit signal: {}", err);
                        }
                    }
                }
                break;
            }
            _ = sigint.recv() => {
                debug!("Received SIGINT");
                forward_signal(container_pid, Signal::SIGINT);
            }
            _ = sigterm.recv() => {
                debug!("Received SIGTERM");
                forward_signal(container_pid, Signal::SIGTERM);
            }
            _ = sigquit.recv() => {
                debug!("Received SIGQUIT");
                forward_signal(container_pid, Signal::SIGQUIT);
            }
        }
    }

    Ok(())
}

pub fn forward_signal(container_pid: Pid, signal: Signal) {
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
