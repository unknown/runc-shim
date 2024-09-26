use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{bail, Context, Result};
use nix::unistd::Pid;
use tokio::process::Command;

const PID_FILE: &str = "container.pid";
const STDOUT_FILE: &str = "stdout.log";
const STDERR_FILE: &str = "stderr.log";

pub struct Container {
    /// The container ID.
    id: String,

    /// The bundle directory.
    bundle: PathBuf,

    /// The container process ID.
    pid: Option<Pid>,
}

impl Container {
    pub fn new(id: &str, bundle: &PathBuf) -> Self {
        Self {
            id: id.to_string(),
            bundle: bundle.to_path_buf(),
            pid: None,
        }
    }

    pub async fn start(&mut self, runtime: &PathBuf) -> Result<()> {
        let mut cmd = Command::new(runtime);
        cmd.arg("create")
            .arg("--bundle")
            .arg(&self.bundle)
            .arg("--pid-file")
            .arg(self.bundle.join(PID_FILE))
            .arg(&self.id);
        let stdout_file = stdio_file(self.bundle.join(STDOUT_FILE))?;
        let stderr_file = stdio_file(self.bundle.join(STDERR_FILE))?;
        cmd.stdin(Stdio::null())
            .stdout(stdout_file)
            .stderr(stderr_file);
        let mut child = cmd.spawn().context("Failed to spawn OCI runtime")?;
        match child.wait().await {
            Ok(status) if status.success() => {}
            Ok(status) => bail!("OCI runtime exited with status {}", status),
            Err(err) => bail!("Failed to wait for OCI runtime: {}", err),
        };
        let pid = read_pid(self.bundle.join(PID_FILE))?;
        self.pid = Some(pid);
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

    pub fn pid(&self) -> Option<Pid> {
        self.pid
    }
}

fn read_pid<P: AsRef<Path>>(path: P) -> Result<Pid> {
    let contents = fs::read_to_string(path)?;
    Ok(Pid::from_raw(contents.parse()?))
}

fn stdio_file<P: AsRef<Path>>(path: P) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    Ok(file)
}
