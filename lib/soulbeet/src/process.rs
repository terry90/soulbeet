//! Child processes that run in a process group of their own, so a run ends with every process it
//! started: hook scripts, plugin helpers, a JS runtime.

use std::io;
use tokio::process::{Child, Command};

/// The process group of a child `spawn` started. Dropped armed, it kills the whole group:
/// `kill_on_drop` alone only signals the child's own pid.
pub struct ProcessGroup {
    pgid: libc::pid_t,
    armed: bool,
}

impl ProcessGroup {
    /// Starts `command` as the leader of a new process group, killed with its group when the
    /// returned guard is dropped armed. tokio reaps the child once it is killed.
    pub fn spawn(command: &mut Command) -> io::Result<(Child, ProcessGroup)> {
        let child = command.process_group(0).kill_on_drop(true).spawn()?;
        let group = ProcessGroup {
            pgid: child.id().map_or(0, |id| id as libc::pid_t),
            armed: true,
        };
        Ok((child, group))
    }

    /// Sends SIGKILL to every process of the group.
    pub fn kill(&self) {
        if self.pgid > 1 {
            // SAFETY: killpg has no memory-safety preconditions.
            unsafe {
                libc::killpg(self.pgid, libc::SIGKILL);
            }
        }
    }

    /// The run finished and drained: dropping the guard no longer kills the group.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}
