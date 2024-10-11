use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{bail, Context, Result};
use nix::{sys::signal::Signal, unistd::Pid};
use prost_types::Timestamp;
use time::OffsetDateTime;
use tokio::{
    process::Command,
    sync::{mpsc, RwLock},
};

use crate::signal::forward_signal;

const PID_FILE: &str = "container.pid";

pub struct Container {
    /// The container ID.
    pub id: String,

    /// The bundle directory.
    pub bundle: PathBuf,

    /// The container's stdout path.
    pub stdout: PathBuf,

    /// The container's stderr path.
    pub stderr: PathBuf,

    /// The container status.
    status: RwLock<Status>,

    /// The container process ID.
    pid: RwLock<i32>,

    /// The container's exit code.
    exit_code: RwLock<i32>,

    /// The container's exit timestamp.
    exited_at: RwLock<Option<OffsetDateTime>>,

    /// The container's wait channels.
    wait_channels: RwLock<Vec<mpsc::UnboundedSender<()>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    UNKNOWN,
    CREATED,
    RUNNING,
    STOPPED,
}

impl Container {
    pub fn new(id: &str, bundle: &PathBuf, stdout: &PathBuf, stderr: &PathBuf) -> Self {
        Self {
            id: id.to_string(),
            bundle: bundle.to_owned(),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            status: RwLock::new(Status::UNKNOWN),
            pid: RwLock::new(0),
            exit_code: RwLock::new(0),
            exited_at: RwLock::new(None),
            wait_channels: RwLock::new(Vec::new()),
        }
    }

    pub async fn create(&self, runtime: &PathBuf) -> Result<()> {
        let mut cmd = Command::new(runtime);
        cmd.arg("create")
            .arg("--bundle")
            .arg(&self.bundle)
            .arg("--pid-file")
            .arg(self.bundle.join(PID_FILE))
            .arg(&self.id);
        cmd.stdin(Stdio::null())
            .stdout(stdio_file(&self.stdout)?)
            .stderr(stdio_file(&self.stderr)?);
        let mut child = cmd.spawn().context("Failed to spawn OCI runtime")?;
        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => bail!("OCI runtime exited with status {}", status),
            Err(err) => bail!("Failed to wait for OCI runtime: {}", err),
        };
        let pid = read_pid(self.bundle.join(PID_FILE))?;
        let mut pid_guard = self.pid.write().await;
        let mut status_guard = self.status.write().await;
        *pid_guard = pid;
        *status_guard = Status::CREATED;
        Ok(())
    }

    pub async fn start(&self, runtime: &PathBuf) -> Result<()> {
        let mut cmd = Command::new(runtime);
        cmd.arg("start").arg(&self.id);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("Failed to spawn OCI runtime")?;
        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => bail!("OCI runtime exited with status {}", status),
            Err(err) => bail!("Failed to wait for OCI runtime: {}", err),
        };
        *self.status.write().await = Status::RUNNING;
        Ok(())
    }

    pub async fn delete(&self, runtime: &PathBuf) -> Result<()> {
        let mut cmd = Command::new(runtime);
        cmd.arg("delete").arg(&self.id);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("Failed to spawn OCI runtime")?;
        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => bail!("OCI runtime exited with status {}", status),
            Err(err) => bail!("Failed to wait for OCI runtime: {}", err),
        };
        Ok(())
    }

    pub async fn kill(&self, signal: Signal) -> Result<()> {
        let pid = Pid::from_raw(*self.pid.read().await);
        forward_signal(pid, signal)?;
        Ok(())
    }

    pub async fn wait_channel(&self) -> mpsc::UnboundedReceiver<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        // keep this guard so that the status is not changed while adding the channel
        let status_guard = self.status.read().await;
        if *status_guard != Status::STOPPED {
            self.wait_channels.write().await.push(tx);
        }
        rx
    }

    pub async fn set_exited(&self, exit_code: i32) {
        let mut status_guard = self.status.write().await;
        let mut exit_code_guard = self.exit_code.write().await;
        let mut exited_at_guard = self.exited_at.write().await;
        *status_guard = Status::STOPPED;
        *exit_code_guard = exit_code;
        *exited_at_guard = Some(OffsetDateTime::now_utc());
        for tx in self.wait_channels.write().await.drain(..) {
            let _ = tx.send(());
        }
    }

    pub async fn exited_at(&self) -> Option<Timestamp> {
        self.exited_at.read().await.map(|exited_at| Timestamp {
            seconds: exited_at.unix_timestamp(),
            nanos: exited_at.nanosecond() as i32,
        })
    }

    pub async fn pid(&self) -> i32 {
        *self.pid.read().await
    }

    pub async fn status(&self) -> Status {
        *self.status.read().await
    }

    pub async fn exit_code(&self) -> i32 {
        *self.exit_code.read().await
    }
}

fn read_pid<P: AsRef<Path>>(path: P) -> Result<i32> {
    let contents = fs::read_to_string(path)?;
    Ok(contents.parse()?)
}

fn stdio_file<P: AsRef<Path>>(path: P) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    Ok(file)
}
