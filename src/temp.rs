use crate::error::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{nanos}-{counter}", std::process::id())
}

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new() -> Result<Self> {
        let root = std::env::temp_dir();
        for _ in 0..100 {
            let path = root.join(unique_name("folder-diff"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        crate::bail!("could not create a unique temporary directory")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn create_file_in(directory: &Path, prefix: &str) -> Result<(PathBuf, File)> {
    for _ in 0..100 {
        let path = directory.join(unique_name(prefix));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    crate::bail!(
        "could not create a unique temporary file in {}",
        directory.display()
    )
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .context("destination has no parent directory")?;
    let (temporary_path, mut temporary) = create_file_in(parent, ".folder-diff-write")?;
    let result = (|| -> Result<()> {
        temporary.write_all(bytes)?;
        temporary.sync_all()?;
        drop(temporary);
        fs::rename(&temporary_path, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}
