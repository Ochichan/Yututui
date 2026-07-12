//! Platform child-tree ownership for transient helper processes.

use crate::util::process::ProcessProfile;

/// Owns every process descended from a child that must not outlive its caller.
///
/// Tokio's `kill_on_drop` and `Child::kill` only target the direct child. Media and yt-dlp
/// processes can also launch ffmpeg, a JavaScript runtime, or other helpers, so they are placed in
/// a process group on Unix and a kill-on-close Job Object on Windows. Owners must terminate this
/// guard before dropping the child because its Windows fallback borrows the process handle.
pub(crate) struct ChildTreeGuard {
    #[cfg(unix)]
    pgid: Option<libc::pid_t>,
    #[cfg(windows)]
    job: Option<isize>,
    #[cfg(windows)]
    process: Option<isize>,
    armed: bool,
}

impl ChildTreeGuard {
    pub(crate) fn for_std(child: &std::process::Child, profile: ProcessProfile) -> Self {
        if !matches!(profile, ProcessProfile::Media | ProcessProfile::YtDlp) {
            return Self::unarmed();
        }

        #[cfg(windows)]
        let process = {
            use std::os::windows::io::AsRawHandle;
            Some(child.as_raw_handle() as isize)
        };
        Self::armed(
            #[cfg(unix)]
            Some(child.id()).and_then(|id| libc::pid_t::try_from(id).ok()),
            #[cfg(windows)]
            process,
        )
    }

    pub(crate) fn for_tokio(child: &tokio::process::Child, profile: ProcessProfile) -> Self {
        if !matches!(profile, ProcessProfile::Media | ProcessProfile::YtDlp) {
            return Self::unarmed();
        }

        #[cfg(windows)]
        let process = child.raw_handle().map(|handle| handle as isize);
        Self::armed(
            #[cfg(unix)]
            child.id().and_then(|id| libc::pid_t::try_from(id).ok()),
            #[cfg(windows)]
            process,
        )
    }

    fn armed(
        #[cfg(unix)] pgid: Option<libc::pid_t>,
        #[cfg(windows)] process: Option<isize>,
    ) -> Self {
        #[cfg(windows)]
        let job = match process {
            Some(process) => super::process::create_child_job(process),
            None => {
                tracing::warn!(
                    "child exited before its process handle could be assigned to a Job Object; \
                     relying on the direct-child kill-on-drop fallback"
                );
                None
            }
        };
        Self {
            #[cfg(unix)]
            pgid,
            #[cfg(windows)]
            job,
            #[cfg(windows)]
            process,
            armed: true,
        }
    }

    fn unarmed() -> Self {
        Self {
            #[cfg(unix)]
            pgid: None,
            #[cfg(windows)]
            job: None,
            #[cfg(windows)]
            process: None,
            armed: false,
        }
    }

    /// Synchronously terminates any remaining member of the owned process tree.
    ///
    /// This is also used after the direct child exits successfully: a detached helper that kept
    /// no output pipe open must still not escape the lifetime of the media/helper invocation.
    pub(crate) fn terminate(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            super::process::terminate_process_group(pgid);
        }
        #[cfg(windows)]
        match self.job.take() {
            Some(job) => super::process::close_child_job(job),
            None => {
                // Assignment can fail when the host applies an incompatible parent job. Retain
                // the borrowed process handle until this guard drops so cancellation still has
                // a synchronous direct-child fallback in addition to Tokio's kill-on-drop.
                if let Some(process) = self.process {
                    super::process::terminate_child_process(process);
                } else {
                    tracing::warn!(
                        "child has no Windows process handle; relying on kill-on-drop fallback"
                    );
                }
            }
        }
        #[cfg(windows)]
        {
            self.process = None;
        }
        self.armed = false;
    }

    #[cfg(all(test, windows))]
    pub(crate) fn owns_job(&self) -> bool {
        self.job.is_some()
    }
}

impl Drop for ChildTreeGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}
