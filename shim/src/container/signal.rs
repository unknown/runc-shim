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
use tracing::{error, info, warn};

use super::Container;

impl Container {
    pub async fn handle_signals(
        &self,
        wait_channels: Arc<Mutex<Vec<mpsc::UnboundedSender<()>>>>,
    ) -> Result<()> {
        let container_pid = self.pid.context("Missing container PID")?;

        let mut sigchld = signal(SignalKind::child())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigquit = signal(SignalKind::quit())?;

        loop {
            tokio::select! {
                _ = sigchld.recv() => {
                    let status = waitpid(container_pid, Some(WaitPidFlag::WNOHANG)).context("Failed to get container status")?;
                    match status {
                        WaitStatus::Exited(pid, status) => {
                            info!("Container {} exited with status {}", pid, status);
                        }
                        WaitStatus::Signaled(pid, signal, _) => {
                            info!("Container {} exited with signal {}", pid, signal);
                        }
                        _ => {
                            info!("Container exited with unknown status");
                        }
                    }
                    for sender in wait_channels.lock().await.iter() {
                        sender.send(()).context("Failed to send exit signal")?;
                    }
                    break;
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
            }
        }

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
