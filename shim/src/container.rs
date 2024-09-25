use std::{
    ffi::CString,
    fs,
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use nix::{
    fcntl::{open, OFlag},
    libc,
    sys::{
        stat::Mode,
        wait::{waitpid, WaitStatus},
    },
    unistd::{close, dup2, execv, fork, pipe2, ForkResult, Pid},
};

mod monitor;

const PID_FILE: &str = "container.pid";

pub struct Container {
    /// The container ID.
    id: String,

    /// The bundle directory.
    bundle: PathBuf,

    /// Read end of the stdout pipe.
    stdout: Option<OwnedFd>,

    /// Read end of the stderr pipe.
    stderr: Option<OwnedFd>,

    /// The container process ID.
    pid: Option<Pid>,
}

impl Container {
    pub fn new(id: &str, bundle: &PathBuf) -> Self {
        Self {
            id: id.to_string(),
            bundle: bundle.to_path_buf(),
            stdout: None,
            stderr: None,
            pid: None,
        }
    }

    pub fn start(&mut self, runtime: &PathBuf) -> Result<()> {
        let stdout = pipe2(OFlag::O_CLOEXEC).context("Failed to create pipe")?;
        let stderr = pipe2(OFlag::O_CLOEXEC).context("Failed to create pipe")?;

        let pid = match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => child,
            Ok(ForkResult::Child) => {
                let dev_null = open("/dev/null", OFlag::O_RDWR, Mode::empty())?;
                dup2(dev_null, libc::STDIN_FILENO).context("Failed to dup stdin")?;
                dup2(stdout.1.as_raw_fd(), libc::STDOUT_FILENO).context("Failed to dup stdout")?;
                dup2(stderr.1.as_raw_fd(), libc::STDERR_FILENO).context("Failed to dup stderr")?;
                close(dev_null).context("Failed to close dev null")?;
                let argv = [
                    CString::new(runtime.to_string_lossy().to_string())?,
                    CString::new("create")?,
                    CString::new("--bundle")?,
                    CString::new(self.bundle.to_string_lossy().to_string())?,
                    CString::new("--pid-file")?,
                    CString::new(self.bundle.join(PID_FILE).to_string_lossy().to_string())?,
                    CString::new(self.id.as_str())?,
                ];
                if let Err(err) = execv(&argv[0], &argv) {
                    bail!("Failed to exec OCI runtime: {}", err);
                }
                unsafe { libc::_exit(127) }
            }
            Err(err) => bail!("Failed to fork: {}", err),
        };

        drop(stdout.1);
        drop(stderr.1);

        match waitpid(pid, None) {
            Ok(WaitStatus::Exited(_, 0)) => {}
            Ok(WaitStatus::Exited(_, status)) => bail!("OCI runtime exited with status {}", status),
            Ok(WaitStatus::Signaled(_, signal, _)) => {
                bail!("OCI runtime exited with signal {}", signal)
            }
            _ => bail!("OCI runtime exited with unknown status"),
        };

        let pid = read_pid(self.bundle.join(PID_FILE))?;
        self.stdout = Some(stdout.0);
        self.stderr = Some(stderr.0);
        self.pid = Some(pid);

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
