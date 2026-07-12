use std::path::{Path, PathBuf};

/// Owns an in-progress download path so dropping the async future cannot strand it.
pub(super) struct DownloadTemp {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    remove_on_drop: bool,
}

impl DownloadTemp {
    pub(super) fn create(path: PathBuf) -> std::io::Result<Self> {
        // File creation is synchronous on purpose: `tokio::fs::File::create` may finish
        // its uncancellable blocking open after the outer future is aborted, which would
        // create the path after a drop guard had already tried to remove it.
        let file = std::fs::File::create(&path)?;
        Ok(Self {
            path,
            file: Some(tokio::fs::File::from_std(file)),
            remove_on_drop: true,
        })
    }

    pub(super) fn file_mut(&mut self) -> &mut tokio::fs::File {
        self.file
            .as_mut()
            .expect("download temp owns its file until finish")
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn finish(mut self) -> PathBuf {
        // Close first so Windows can rename/remove the completed temp later.
        drop(self.file.take());
        self.remove_on_drop = false;
        self.path.clone()
    }
}

impl Drop for DownloadTemp {
    fn drop(&mut self) {
        if !self.remove_on_drop {
            return;
        }
        drop(self.file.take());
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %self.path.display(),
                %error,
                "failed to remove incomplete yt-dlp download"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn aborting_owner_removes_partial_temp() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("ytt-ytdlp-cancel-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(".yt-dlp.tmp-{}", std::process::id()));
        let task_path = path.clone();
        let (created_tx, created_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut temp = DownloadTemp::create(task_path).unwrap();
            temp.file_mut().write_all(b"partial").await.unwrap();
            temp.file_mut().sync_all().await.unwrap();
            created_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        created_rx.await.unwrap();
        assert!(path.is_file());

        task.abort();
        let error = task.await.expect_err("aborted download task must cancel");

        assert!(error.is_cancelled());
        assert!(!path.exists(), "partial download must be removed on cancel");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
