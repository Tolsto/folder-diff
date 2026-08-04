use crate::{
    bail,
    error::{Context, Result},
    model::{
        CompareOptions, EntryKind, EntryStatus, FileRevision, MergeDirection, OperationResult,
    },
    scanner::{ensure_expected_revision, safe_join, scan_directories},
    temp::{TempDir, create_file_in},
};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

const MAX_UNDO_TRANSACTIONS: usize = 20;

enum SnapshotContent {
    Missing,
    File {
        backup_path: PathBuf,
        permissions: fs::Permissions,
    },
    Symlink {
        target: PathBuf,
    },
}

struct Snapshot {
    root: PathBuf,
    relative_path: PathBuf,
    content: SnapshotContent,
}

struct Transaction {
    label: String,
    _temp_directory: TempDir,
    snapshots: Vec<Snapshot>,
    captured: HashSet<(PathBuf, PathBuf)>,
}

pub struct MergeManager {
    undo_stack: VecDeque<Transaction>,
}

impl Default for MergeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MergeManager {
    pub fn new() -> Self {
        Self {
            undo_stack: VecDeque::new(),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn merge_file(
        &mut self,
        left_root: &Path,
        right_root: &Path,
        relative_path: &Path,
        direction: MergeDirection,
        expected_source: Option<&FileRevision>,
        expected_destination: Option<&FileRevision>,
    ) -> Result<OperationResult> {
        let (source_root, destination_root) = roots_for_direction(left_root, right_root, direction);
        let source_path = safe_join(source_root, relative_path)?;
        let destination_path = safe_join(destination_root, relative_path)?;
        ensure_expected_revision(&source_path, expected_source)
            .context("the source changed after the preview was loaded")?;
        ensure_expected_revision(&destination_path, expected_destination)?;

        self.run_transaction(
            format!("Merge {}", relative_path.display()),
            |transaction| {
                capture(transaction, destination_root, relative_path)?;
                copy_entity(source_root, relative_path, destination_root, &source_path)?;
                Ok(vec![relative_path.to_path_buf()])
            },
        )
    }

    pub fn write_text(
        &mut self,
        destination_root: &Path,
        relative_path: &Path,
        content: &str,
        expected_destination: Option<&FileRevision>,
    ) -> Result<OperationResult> {
        let destination_path = safe_join(destination_root, relative_path)?;
        ensure_expected_revision(&destination_path, expected_destination)?;
        self.run_transaction(
            format!("Merge hunk in {}", relative_path.display()),
            |transaction| {
                capture(transaction, destination_root, relative_path)?;
                atomic_write(destination_root, relative_path, content.as_bytes())?;
                Ok(vec![relative_path.to_path_buf()])
            },
        )
    }

    pub fn synchronize(
        &mut self,
        left_root: &Path,
        right_root: &Path,
        options: &CompareOptions,
        direction: MergeDirection,
    ) -> Result<OperationResult> {
        let mut scan_options = options.clone();
        scan_options.show_identical = false;
        let scan = scan_directories(left_root, right_root, &scan_options)?;
        let (source_root, destination_root) = roots_for_direction(left_root, right_root, direction);
        let copyable = |status| match direction {
            MergeDirection::LeftToRight => {
                matches!(status, EntryStatus::Modified | EntryStatus::LeftOnly)
            }
            MergeDirection::RightToLeft => {
                matches!(status, EntryStatus::Modified | EntryStatus::RightOnly)
            }
        };
        let entries: Vec<_> = scan
            .entries
            .iter()
            .filter(|entry| copyable(entry.status))
            .collect();
        let warnings: Vec<_> = scan
            .entries
            .iter()
            .filter(|entry| entry.status == EntryStatus::TypeMismatch)
            .map(|entry| format!("Skipped type mismatch: {}", entry.relative_path.display()))
            .collect();

        let mut result = self.run_transaction(
            match direction {
                MergeDirection::LeftToRight => "Synchronize left to right".into(),
                MergeDirection::RightToLeft => "Synchronize right to left".into(),
            },
            |transaction| {
                let mut changed_paths = Vec::with_capacity(entries.len());
                for entry in entries {
                    let source = entry
                        .source(direction)
                        .context("copyable entry has no source")?;
                    if !matches!(source.kind, EntryKind::File | EntryKind::Symlink) {
                        continue;
                    }
                    capture(transaction, destination_root, &entry.relative_path)?;
                    copy_entity(
                        source_root,
                        &entry.relative_path,
                        destination_root,
                        &source.absolute_path,
                    )?;
                    changed_paths.push(entry.relative_path.clone());
                }
                Ok(changed_paths)
            },
        )?;
        result.warnings = warnings;
        Ok(result)
    }

    pub fn undo(&mut self) -> Result<(String, OperationResult)> {
        let Some(transaction) = self.undo_stack.pop_back() else {
            return Ok((String::new(), OperationResult::default()));
        };
        let label = transaction.label.clone();
        for snapshot in transaction.snapshots.iter().rev() {
            restore(snapshot)?;
        }
        let changed_paths = transaction
            .snapshots
            .iter()
            .map(|snapshot| snapshot.relative_path.clone())
            .collect();
        Ok((
            label,
            OperationResult {
                changed_paths,
                warnings: Vec::new(),
                undo_available: !self.undo_stack.is_empty(),
            },
        ))
    }

    fn run_transaction(
        &mut self,
        label: String,
        operation: impl FnOnce(&mut Transaction) -> Result<Vec<PathBuf>>,
    ) -> Result<OperationResult> {
        let mut transaction = Transaction {
            label,
            _temp_directory: TempDir::new()?,
            snapshots: Vec::new(),
            captured: HashSet::new(),
        };
        let result = operation(&mut transaction);
        let changed_paths = match result {
            Ok(paths) => paths,
            Err(error) => {
                for snapshot in transaction.snapshots.iter().rev() {
                    restore(snapshot).context("merge failed and rollback also failed")?;
                }
                return Err(error);
            }
        };
        if changed_paths.is_empty() {
            return Ok(OperationResult {
                changed_paths,
                warnings: Vec::new(),
                undo_available: self.can_undo(),
            });
        }
        self.undo_stack.push_back(transaction);
        while self.undo_stack.len() > MAX_UNDO_TRANSACTIONS {
            self.undo_stack.pop_front();
        }
        Ok(OperationResult {
            changed_paths,
            warnings: Vec::new(),
            undo_available: true,
        })
    }
}

fn roots_for_direction<'a>(
    left_root: &'a Path,
    right_root: &'a Path,
    direction: MergeDirection,
) -> (&'a Path, &'a Path) {
    match direction {
        MergeDirection::LeftToRight => (left_root, right_root),
        MergeDirection::RightToLeft => (right_root, left_root),
    }
}

fn ensure_safe_parent(root: &Path, relative_path: &Path) -> Result<PathBuf> {
    let destination = safe_join(root, relative_path)?;
    let mut current = root.to_path_buf();
    if let Some(parent) = relative_path.parent() {
        for component in parent.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!(
                        "refusing to write through symlinked directory: {}",
                        current.display()
                    )
                }
                Ok(metadata) if !metadata.is_dir() => {
                    bail!("parent path is not a directory: {}", current.display())
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(destination)
}

fn capture(transaction: &mut Transaction, root: &Path, relative_path: &Path) -> Result<()> {
    let key = (root.to_path_buf(), relative_path.to_path_buf());
    if !transaction.captured.insert(key) {
        return Ok(());
    }
    let destination = safe_join(root, relative_path)?;
    let content = match fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SnapshotContent::Missing,
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_dir() => {
            bail!("refusing to overwrite directory: {}", destination.display())
        }
        Ok(metadata) if metadata.file_type().is_symlink() => SnapshotContent::Symlink {
            target: fs::read_link(&destination)?,
        },
        Ok(metadata) if metadata.is_file() => {
            let backup_path = transaction
                ._temp_directory
                .path()
                .join(format!("backup-{}", transaction.snapshots.len()));
            fs::copy(&destination, &backup_path)?;
            SnapshotContent::File {
                backup_path,
                permissions: metadata.permissions(),
            }
        }
        Ok(_) => bail!("unsupported destination type: {}", destination.display()),
    };
    transaction.snapshots.push(Snapshot {
        root: root.to_path_buf(),
        relative_path: relative_path.to_path_buf(),
        content,
    });
    Ok(())
}

fn remove_non_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            bail!("refusing to remove directory: {}", path.display())
        }
        Ok(_) => fs::remove_file(path).map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn atomic_write(root: &Path, relative_path: &Path, content: &[u8]) -> Result<()> {
    let destination = ensure_safe_parent(root, relative_path)?;
    let existing_permissions = fs::symlink_metadata(&destination)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.permissions());
    let parent = destination
        .parent()
        .context("destination has no parent directory")?;
    let (temporary_path, mut temporary) = create_file_in(parent, ".folder-diff-write")?;
    use std::io::Write as _;
    temporary.write_all(content)?;
    temporary.sync_all()?;
    if let Some(permissions) = existing_permissions {
        temporary.set_permissions(permissions)?;
    }
    drop(temporary);
    remove_non_directory(&destination)?;
    if let Err(error) = fs::rename(&temporary_path, &destination) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error).with_context(|| format!("could not replace {}", destination.display()));
    }
    Ok(())
}

fn copy_entity(
    source_root: &Path,
    relative_path: &Path,
    destination_root: &Path,
    source: &Path,
) -> Result<()> {
    // Validate both paths against their selected roots before touching the filesystem.
    let expected_source = safe_join(source_root, relative_path)?;
    if expected_source != source {
        bail!("source path changed during synchronization");
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        let destination = ensure_safe_parent(destination_root, relative_path)?;
        remove_non_directory(&destination)?;
        create_symlink(&fs::read_link(source)?, &destination)?;
        return Ok(());
    }
    if !metadata.is_file() {
        bail!("only files and symbolic links can be merged");
    }
    let bytes = fs::read(source)?;
    atomic_write(destination_root, relative_path, &bytes)?;
    let destination = safe_join(destination_root, relative_path)?;
    fs::set_permissions(destination, metadata.permissions())?;
    Ok(())
}

fn restore(snapshot: &Snapshot) -> Result<()> {
    let destination = ensure_safe_parent(&snapshot.root, &snapshot.relative_path)?;
    remove_non_directory(&destination)?;
    match &snapshot.content {
        SnapshotContent::Missing => Ok(()),
        SnapshotContent::File {
            backup_path,
            permissions,
        } => {
            fs::copy(backup_path, &destination)?;
            fs::set_permissions(destination, permissions.clone())?;
            Ok(())
        }
        SnapshotContent::Symlink { target } => create_symlink(target, &destination),
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination).map_err(Into::into)
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, destination).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{read_comparison, revision};
    use crate::temp::TempDir;
    use std::fs;

    #[test]
    fn merge_is_guarded_by_revision_and_can_be_undone() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        fs::write(left.path().join("file.txt"), "left\n").unwrap();
        fs::write(right.path().join("file.txt"), "right\n").unwrap();
        let preview = read_comparison(left.path(), right.path(), Path::new("file.txt")).unwrap();
        let mut manager = MergeManager::new();

        manager
            .merge_file(
                left.path(),
                right.path(),
                Path::new("file.txt"),
                MergeDirection::LeftToRight,
                preview.left.revision.as_ref(),
                preview.right.revision.as_ref(),
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(right.path().join("file.txt")).unwrap(),
            "left\n"
        );

        manager.undo().unwrap();
        assert_eq!(
            fs::read_to_string(right.path().join("file.txt")).unwrap(),
            "right\n"
        );
    }

    #[test]
    fn stale_destination_is_rejected() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        fs::write(left.path().join("file.txt"), "left").unwrap();
        fs::write(right.path().join("file.txt"), "right").unwrap();
        let old_revision = revision(&right.path().join("file.txt")).unwrap();
        fs::write(right.path().join("file.txt"), "changed elsewhere").unwrap();

        let error = MergeManager::new()
            .merge_file(
                left.path(),
                right.path(),
                Path::new("file.txt"),
                MergeDirection::LeftToRight,
                Some(&revision(&left.path().join("file.txt")).unwrap()),
                Some(&old_revision),
            )
            .unwrap_err();
        assert!(error.to_string().contains("changed on disk"));
    }

    #[test]
    fn stale_source_is_rejected() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        fs::write(left.path().join("file.txt"), "left").unwrap();
        fs::write(right.path().join("file.txt"), "right").unwrap();
        let old_source = revision(&left.path().join("file.txt")).unwrap();
        let destination = revision(&right.path().join("file.txt")).unwrap();
        fs::write(left.path().join("file.txt"), "changed elsewhere").unwrap();

        let error = MergeManager::new()
            .merge_file(
                left.path(),
                right.path(),
                Path::new("file.txt"),
                MergeDirection::LeftToRight,
                Some(&old_source),
                Some(&destination),
            )
            .unwrap_err();
        assert!(error.to_string().contains("source changed"));
        assert_eq!(
            fs::read_to_string(right.path().join("file.txt")).unwrap(),
            "right"
        );
    }

    #[test]
    fn synchronization_never_deletes_target_only_files() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        fs::write(left.path().join("source.txt"), "source").unwrap();
        fs::write(right.path().join("target-only.txt"), "keep me").unwrap();

        MergeManager::new()
            .synchronize(
                left.path(),
                right.path(),
                &CompareOptions::default(),
                MergeDirection::LeftToRight,
            )
            .unwrap();
        assert!(right.path().join("target-only.txt").exists());
        assert_eq!(
            fs::read_to_string(right.path().join("source.txt")).unwrap(),
            "source"
        );
    }
}
