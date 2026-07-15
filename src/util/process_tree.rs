//! Owned lifetime for a synchronous child and every helper it launches.

use std::process::{Child, ExitStatus};

use super::process::ProcessProfile;
use super::process_guard::ChildTreeGuard;

const REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const REAP_POLL: std::time::Duration = std::time::Duration::from_millis(5);

/// A synchronous child whose process group / Job Object is terminated on close or drop.
pub struct OwnedProcessTree {
    // Keep the guard before the child so its borrowed Windows process handle remains valid even
    // during automatic field drop. `Drop` also terminates it explicitly before reaping `child`.
    tree: ChildTreeGuard,
    child: Option<Child>,
    guardian_lease: Option<crate::player::guardian::GuardianLease>,
    pid: u32,
    exit_status: Option<ExitStatus>,
}

impl OwnedProcessTree {
    pub fn new(child: Child, profile: ProcessProfile) -> Self {
        let tree = ChildTreeGuard::for_std(&child, profile);
        let pid = child.id();
        Self {
            tree,
            child: Some(child),
            guardian_lease: None,
            pid,
            exit_status: None,
        }
    }

    /// Own a same-binary mpv guardian while exposing the actual mpv pid to overlay reducers.
    pub(crate) fn new_guarded(guarded: crate::player::guardian::GuardedSpawn) -> Self {
        let (tree, child, lease, pid) = guarded.into_parts();
        Self {
            tree,
            child: Some(child),
            guardian_lease: Some(lease),
            pid,
            exit_status: None,
        }
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if self.exit_status.is_some() {
            return Ok(self.exit_status);
        }
        let Some(child) = self.child.as_ref() else {
            // A bounded close may have handed a very slow child to the background reaper.
            return Ok(None);
        };

        let Some(clean) = super::process::child_exit_without_reap(child)? else {
            return Ok(None);
        };
        if let Some(lease) = self.guardian_lease.as_mut() {
            lease.disarm_after_guardian_exit(clean);
        }
        // Disarm the pid-based group guard before wait releases the direct child's pid. This also
        // cleans any detached helper while the process-group generation is still unambiguous.
        self.tree.terminate();
        let status = self
            .child
            .as_mut()
            .expect("observed child remains owned")
            .wait()?;
        self.exit_status = Some(status);
        self.child = None;
        Ok(Some(status))
    }

    /// Terminate the whole tree and reap the direct child. Safe to call repeatedly.
    pub fn terminate_and_wait(&mut self) {
        if let Some(mut lease) = self.guardian_lease.take() {
            self.terminate_guarded(&mut lease);
            return;
        }

        // Do this even after the direct child was reaped: a detached descendant may still own the
        // process group / Job Object and must not escape when the wrapper is later dropped.
        self.tree.terminate();
        let Some(mut child) = self.child.take() else {
            return;
        };

        match child.try_wait() {
            Ok(Some(status)) => {
                self.exit_status = Some(status);
                return;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, pid = self.pid, "failed to poll owned child");
            }
        }
        if let Err(error) = child.kill() {
            tracing::debug!(%error, pid = self.pid, "owned child was already unavailable");
        }
        let deadline = std::time::Instant::now() + REAP_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.exit_status = Some(status);
                    return;
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(REAP_POLL);
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(%error, pid = self.pid, "failed to reap owned child");
                    return;
                }
            }
        }

        // Never make overlay close or owner shutdown wait without a deadline. The process tree
        // has already received a hard termination; retain the OS handle on a reaper thread so a
        // late exit is still collected instead of becoming a zombie.
        tracing::warn!(
            pid = self.pid,
            "owned child exceeded the bounded reap deadline"
        );
        let pid = self.pid;
        let reaper_child = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
        let thread_child = std::sync::Arc::clone(&reaper_child);
        if let Err(error) = std::thread::Builder::new()
            .name("ytt-child-reaper".to_owned())
            .spawn(move || {
                let mut child = thread_child
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(child) = child.as_mut()
                    && let Err(error) = child.wait()
                {
                    tracing::warn!(%error, pid, "background child reap failed");
                }
            })
        {
            tracing::warn!(%error, pid, "failed to start background child reaper");
            let mut child = reaper_child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(child) = child.as_mut()
                && let Err(error) = child.wait()
            {
                tracing::warn!(%error, pid, "synchronous child reap failed");
            }
        }
    }

    fn terminate_guarded(&mut self, lease: &mut crate::player::guardian::GuardianLease) {
        let Some(child) = self.child.take() else {
            return;
        };
        self.exit_status =
            crate::player::guardian::shutdown_and_reap_guardian(child, &mut self.tree, lease);
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        self.terminate_and_wait();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Stdio;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;

    fn fixture_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ytt-owned-tree-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create process-tree fixture");
        root
    }

    fn spawn_fixture(root: &std::path::Path) -> OwnedProcessTree {
        let pid_file = root.join("descendant.pid");
        let script = format!("sleep 10 & echo $! > '{}'; wait", pid_file.display());
        let mut command = super::super::process::std_command("sh", ProcessProfile::Media);
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("spawn media process tree fixture");
        OwnedProcessTree::new(child, ProcessProfile::Media)
    }

    fn read_descendant_pid(root: &std::path::Path) -> libc::pid_t {
        let path = root.join("descendant.pid");
        for _ in 0..100 {
            if let Ok(contents) = std::fs::read_to_string(&path)
                && let Ok(pid) = contents.trim().parse()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("descendant pid was not published at {}", path.display());
    }

    fn assert_exits(pid: libc::pid_t) {
        for _ in 0..100 {
            if !super::super::process::process_exists_for_test(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("owned descendant {pid} survived process-tree termination");
    }

    #[test]
    fn drop_terminates_media_descendants() {
        let root = fixture_root("drop");
        let tree = spawn_fixture(&root);
        let descendant = read_descendant_pid(&root);
        assert!(super::super::process::process_exists_for_test(descendant));

        drop(tree);

        assert_exits(descendant);
        std::fs::remove_dir_all(root).expect("remove process-tree fixture");
    }

    #[test]
    fn explicit_close_is_idempotent() {
        let root = fixture_root("close");
        let mut tree = spawn_fixture(&root);
        let direct = libc::pid_t::try_from(tree.id()).expect("child pid fits pid_t");
        let descendant = read_descendant_pid(&root);

        tree.terminate_and_wait();
        tree.terminate_and_wait();

        assert!(!super::super::process::process_exists_for_test(direct));
        assert_exits(descendant);
        std::fs::remove_dir_all(root).expect("remove process-tree fixture");
    }
}
