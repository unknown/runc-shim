use anyhow::{bail, Result};
use nix::{
    libc::pid_t,
    sys::{
        signal::{kill, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::Pid,
};
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::mpsc,
};
use tracing::{debug, error, info, warn};

pub async fn handle_signals(sender: mpsc::UnboundedSender<(pid_t, i32)>) -> Result<()> {
    let mut sigchld = signal(SignalKind::child())?;

    loop {
        tokio::select! {
            _ = sigchld.recv() => {
                debug!("Received SIGCHLD");
                // Because container PIDs are not known a priori, we call `waitpid` with a PID of -1.
                // However, not all SIGCHLD signals are from container processes (e.g. a
                // `process::Command` to create a container) so we need to handle all signals.
                loop {
                    match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::Exited(pid, status)) => {
                            info!("Process {} exited with status {}", pid, status);
                            if let Err(err) = sender.send((pid.as_raw(), status)) {
                                error!("Failed to send exit status: {}", err);
                            }
                        }
                        Ok(WaitStatus::Signaled(pid, signal, _)) => {
                            info!("Process {} exited with signal {}", pid, signal);
                            if let Err(err) = sender.send((pid.as_raw(), 128 + signal as i32)) {
                                error!("Failed to send exit status: {}", err);
                            }
                        }
                        Ok(WaitStatus::StillAlive) => {
                            // Still some unterminated child process
                            break;
                        }
                        Ok(_) => {
                            // Unknown status
                        }
                        Err(nix::Error::ECHILD) => {
                            // No child processes
                            break;
                        }
                        Err(err) => {
                            warn!("Error occurred in signal handler: {}", err);
                            break;
                        }
                    };
                }
            }
        }
    }
}

pub fn forward_signal(pid: Pid, signal: Signal) -> Result<()> {
    match kill(pid, signal) {
        Ok(()) => Ok(()),
        Err(nix::Error::ESRCH) => {
            warn!("Process {} not found, ignoring signal", pid);
            Ok(())
        }
        Err(err) => {
            bail!("Failed to forward signal to process {}: {}", pid, err);
        }
    }
}
