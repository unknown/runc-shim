use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{bail, Context, Result};
use nix::{sys::signal::Signal, unistd::Pid};
use tokio::{process::Command, sync::mpsc};

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
    pub status: Status,

    /// The container process ID.
    pub pid: i32,

    /// The container's exit code.
    pub exit_code: i32,

    /// The container's wait channels.
    wait_channels: Vec<mpsc::UnboundedSender<()>>,
}

#[derive(Clone, PartialEq, Eq)]
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
            status: Status::UNKNOWN,
            pid: 0,
            exit_code: 0,
            wait_channels: Vec::new(),
        }
    }

    pub async fn create(&mut self, runtime: &PathBuf) -> Result<()> {
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
        self.pid = pid;
        self.status = Status::CREATED;
        Ok(())
    }

    pub async fn start(&mut self, runtime: &PathBuf) -> Result<()> {
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
        self.status = Status::RUNNING;
        Ok(())
    }

    pub async fn delete(&mut self, runtime: &PathBuf) -> Result<()> {
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

    pub async fn kill(&mut self, signal: Signal) -> Result<()> {
        let pid = Pid::from_raw(self.pid);
        forward_signal(pid, signal)?;
        Ok(())
    }

    pub fn wait_channel(&mut self) -> mpsc::UnboundedReceiver<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        if self.status != Status::STOPPED {
            self.wait_channels.push(tx);
        }
        rx
    }

    pub fn set_exited(&mut self, exit_code: i32) {
        self.status = Status::STOPPED;
        self.exit_code = exit_code;
        for tx in self.wait_channels.drain(..) {
            let _ = tx.send(());
        }
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
