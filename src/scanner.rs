use crate::model::{
    CompareOptions, ComparisonPreview, DirectoryEntry, EntryKind, EntryStatus, FileMetadata,
    FileRevision, PreviewKind, ScanResult, SidePreview,
};
use crate::{
    anyhow, bail,
    error::{Context, Result},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::{self, File},
    io::{BufReader, Read},
    path::{Component, Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

const BINARY_SAMPLE_BYTES: usize = 8 * 1024;
const MAX_TEXT_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;
const MAX_NORMALIZED_COMPARE_BYTES: u64 = 5 * 1024 * 1024;

struct IgnoreMatcher {
    names: HashSet<String>,
    globs: Vec<String>,
}

impl IgnoreMatcher {
    fn new(patterns: &[String]) -> Result<Self> {
        let mut names = HashSet::new();
        let mut globs = Vec::new();
        for raw in patterns {
            let pattern = raw.trim().trim_start_matches("./");
            if pattern.is_empty() {
                continue;
            }
            if pattern
                .chars()
                .any(|character| matches!(character, '/' | '*' | '?' | '[' | ']' | '{' | '}'))
            {
                globs.push(pattern.to_owned());
            } else {
                names.insert(pattern.to_owned());
            }
        }
        Ok(Self { names, globs })
    }

    fn matches(&self, relative_path: &Path) -> bool {
        relative_path.components().any(|component| {
            matches!(component, Component::Normal(name) if self.names.contains(&name.to_string_lossy().to_string()))
        }) || self.globs.iter().any(|pattern| {
            glob_matches(pattern, &relative_path.to_string_lossy().replace('\\', "/"))
        })
    }
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    fn matches_from(
        pattern: &[u8],
        path: &[u8],
        pattern_index: usize,
        path_index: usize,
        memo: &mut BTreeMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(result) = memo.get(&(pattern_index, path_index)) {
            return *result;
        }
        let result = if pattern_index == pattern.len() {
            path_index == path.len()
        } else if pattern[pattern_index] == b'*' {
            let recursive = pattern.get(pattern_index + 1) == Some(&b'*');
            let next_pattern = pattern_index + if recursive { 2 } else { 1 };
            let zero_match_pattern = if recursive && pattern.get(next_pattern) == Some(&b'/') {
                next_pattern + 1
            } else {
                next_pattern
            };
            matches_from(pattern, path, zero_match_pattern, path_index, memo)
                || (path_index < path.len()
                    && (recursive || path[path_index] != b'/')
                    && matches_from(pattern, path, pattern_index, path_index + 1, memo))
        } else if pattern[pattern_index] == b'?' {
            path_index < path.len()
                && path[path_index] != b'/'
                && matches_from(pattern, path, pattern_index + 1, path_index + 1, memo)
        } else {
            path_index < path.len()
                && pattern[pattern_index] == path[path_index]
                && matches_from(pattern, path, pattern_index + 1, path_index + 1, memo)
        };
        memo.insert((pattern_index, path_index), result);
        result
    }

    matches_from(
        pattern.as_bytes(),
        path.as_bytes(),
        0,
        0,
        &mut BTreeMap::new(),
    )
}

fn entry_kind(file_type: fs::FileType) -> EntryKind {
    if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    }
}

fn metadata_for(root: &Path, absolute_path: PathBuf) -> Result<FileMetadata> {
    let relative_path = absolute_path
        .strip_prefix(root)
        .context("scanned path escaped its root")?
        .to_path_buf();
    let metadata = fs::symlink_metadata(&absolute_path)?;
    let kind = entry_kind(metadata.file_type());
    Ok(FileMetadata {
        relative_path,
        absolute_path: absolute_path.clone(),
        kind,
        size: metadata.len(),
        modified_at: metadata.modified().ok(),
        readonly: metadata.permissions().readonly(),
        symlink_target: (kind == EntryKind::Symlink)
            .then(|| fs::read_link(&absolute_path))
            .transpose()?,
    })
}

fn scan_tree(
    root: &Path,
    matcher: &IgnoreMatcher,
) -> (BTreeMap<PathBuf, FileMetadata>, Vec<String>) {
    let mut nodes = BTreeMap::new();
    let mut warnings = Vec::new();

    fn visit(
        root: &Path,
        directory: &Path,
        matcher: &IgnoreMatcher,
        nodes: &mut BTreeMap<PathBuf, FileMetadata>,
        warnings: &mut Vec<String>,
    ) {
        let read = match fs::read_dir(directory) {
            Ok(read) => read,
            Err(error) => {
                warnings.push(format!("{}: {error}", directory.display()));
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in read {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => warnings.push(format!("{}: {error}", directory.display())),
            }
        }
        paths.sort();
        for path in paths {
            let relative = match path.strip_prefix(root) {
                Ok(relative) => relative,
                Err(error) => {
                    warnings.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            if matcher.matches(relative) {
                continue;
            }
            match metadata_for(root, path.clone()) {
                Ok(metadata) => {
                    let recurse = metadata.kind == EntryKind::Directory;
                    nodes.insert(metadata.relative_path.clone(), metadata);
                    if recurse {
                        visit(root, &path, matcher, nodes, warnings);
                    }
                }
                Err(error) => warnings.push(format!("{}: {error}", path.display())),
            }
        }
    }
    visit(root, root, matcher, &mut nodes, &mut warnings);
    (nodes, warnings)
}

pub fn safe_join(root: &Path, relative_path: &Path) -> Result<PathBuf> {
    if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
        bail!("a non-empty relative path is required");
    }
    if relative_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!(
            "path escapes the selected directory: {}",
            relative_path.display()
        );
    }
    Ok(root.join(relative_path))
}

pub fn revision(path: &Path) -> Result<FileRevision> {
    let metadata = fs::symlink_metadata(path)?;
    let mut hasher = Sha256::new();
    if metadata.file_type().is_symlink() {
        hasher.update(b"symlink\0");
        hasher.update(fs::read_link(path)?.to_string_lossy().as_bytes());
    } else if metadata.is_file() {
        let mut reader = BufReader::new(File::open(path)?);
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
    } else {
        hasher.update(format!("{:?}", metadata.file_type()).as_bytes());
    }
    Ok(FileRevision(hasher.finish_hex()))
}

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffered: usize,
    length_bytes: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffered: 0,
            length_bytes: 0,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.length_bytes = self.length_bytes.wrapping_add(bytes.len() as u64);
        if self.buffered != 0 {
            let count = (64 - self.buffered).min(bytes.len());
            self.buffer[self.buffered..self.buffered + count].copy_from_slice(&bytes[..count]);
            self.buffered += count;
            bytes = &bytes[count..];
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while bytes.len() >= 64 {
            self.compress(&bytes[..64]);
            bytes = &bytes[64..];
        }
        self.buffer[..bytes.len()].copy_from_slice(bytes);
        self.buffered = bytes.len();
    }

    fn finish_hex(mut self) -> String {
        let bit_length = self.length_bytes.wrapping_mul(8);
        self.buffer[self.buffered] = 0x80;
        self.buffered += 1;
        if self.buffered > 56 {
            self.buffer[self.buffered..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }
        self.buffer[self.buffered..56].fill(0);
        self.buffer[56..64].copy_from_slice(&bit_length.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);
        let mut output = String::with_capacity(64);
        for word in self.state {
            use std::fmt::Write as _;
            let _ = write!(output, "{word:08x}");
        }
        output
    }

    fn compress(&mut self, block: &[u8]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0_u32; 64];
        for (index, chunk) in block.chunks_exact(4).take(16).enumerate() {
            words[index] = u32::from_be_bytes(chunk.try_into().expect("four-byte chunk"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temporary1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }
        for (state, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *state = state.wrapping_add(value);
        }
    }
}

pub fn is_binary(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut sample = vec![0_u8; BINARY_SAMPLE_BYTES];
    let count = file.read(&mut sample)?;
    sample.truncate(count);
    Ok(sample.contains(&0) || std::str::from_utf8(&sample).is_err())
}

fn normalized_text(path: &Path, options: &CompareOptions) -> Result<String> {
    let mut text = fs::read_to_string(path)?;
    if options.ignore_line_endings {
        text = text.replace("\r\n", "\n").replace('\r', "\n");
    }
    if options.ignore_whitespace {
        text.retain(|character| !character.is_whitespace());
    }
    Ok(text)
}

fn text_equivalent(
    left: &FileMetadata,
    right: &FileMetadata,
    options: &CompareOptions,
) -> Result<bool> {
    if !options.ignore_line_endings && !options.ignore_whitespace {
        return Ok(false);
    }
    if left.size > MAX_NORMALIZED_COMPARE_BYTES || right.size > MAX_NORMALIZED_COMPARE_BYTES {
        return Ok(false);
    }
    if is_binary(&left.absolute_path)? || is_binary(&right.absolute_path)? {
        return Ok(false);
    }
    Ok(normalized_text(&left.absolute_path, options)?
        == normalized_text(&right.absolute_path, options)?)
}

fn compare_node(
    relative_path: &Path,
    left: Option<&FileMetadata>,
    right: Option<&FileMetadata>,
    options: &CompareOptions,
) -> Result<Option<DirectoryEntry>> {
    if matches!((left, right), (Some(left), Some(right)) if left.kind == EntryKind::Directory && right.kind == EntryKind::Directory)
        || matches!((left, right), (Some(left), None) if left.kind == EntryKind::Directory)
        || matches!((left, right), (None, Some(right)) if right.kind == EntryKind::Directory)
    {
        return Ok(None);
    }

    let mut entry = match (left, right) {
        (Some(left), None) => DirectoryEntry {
            relative_path: relative_path.to_path_buf(),
            status: EntryStatus::LeftOnly,
            left: Some(left.clone()),
            right: None,
            binary: left.kind != EntryKind::File || is_binary(&left.absolute_path)?,
            message: None,
        },
        (None, Some(right)) => DirectoryEntry {
            relative_path: relative_path.to_path_buf(),
            status: EntryStatus::RightOnly,
            left: None,
            right: Some(right.clone()),
            binary: right.kind != EntryKind::File || is_binary(&right.absolute_path)?,
            message: None,
        },
        (Some(left), Some(right)) if left.kind != right.kind => DirectoryEntry {
            relative_path: relative_path.to_path_buf(),
            status: EntryStatus::TypeMismatch,
            left: Some(left.clone()),
            right: Some(right.clone()),
            binary: true,
            message: Some(format!(
                "{:?} on the left, {:?} on the right",
                left.kind, right.kind
            )),
        },
        (Some(left), Some(right)) if left.kind == EntryKind::Symlink => DirectoryEntry {
            relative_path: relative_path.to_path_buf(),
            status: if left.symlink_target == right.symlink_target {
                EntryStatus::Identical
            } else {
                EntryStatus::Modified
            },
            left: Some(left.clone()),
            right: Some(right.clone()),
            binary: true,
            message: None,
        },
        (Some(left), Some(right)) if left.kind == EntryKind::File => {
            let same_revision = revision(&left.absolute_path)? == revision(&right.absolute_path)?;
            let identical = same_revision || text_equivalent(left, right, options)?;
            DirectoryEntry {
                relative_path: relative_path.to_path_buf(),
                status: if identical {
                    EntryStatus::Identical
                } else {
                    EntryStatus::Modified
                },
                left: Some(left.clone()),
                right: Some(right.clone()),
                binary: is_binary(&left.absolute_path)? || is_binary(&right.absolute_path)?,
                message: None,
            }
        }
        (Some(left), Some(right)) => DirectoryEntry {
            relative_path: relative_path.to_path_buf(),
            status: EntryStatus::TypeMismatch,
            left: Some(left.clone()),
            right: Some(right.clone()),
            binary: true,
            message: Some(format!("Unsupported {:?} filesystem entry", left.kind)),
        },
        (None, None) => return Ok(None),
    };

    if entry.status == EntryStatus::Identical && !options.show_identical {
        return Ok(None);
    }
    entry.relative_path = relative_path.to_path_buf();
    Ok(Some(entry))
}

pub fn scan_directories(
    left_root: &Path,
    right_root: &Path,
    options: &CompareOptions,
) -> Result<ScanResult> {
    let started = Instant::now();
    if !left_root.is_dir() {
        bail!("not a directory: {}", left_root.display());
    }
    if !right_root.is_dir() {
        bail!("not a directory: {}", right_root.display());
    }

    let matcher = IgnoreMatcher::new(&options.ignore_patterns)?;
    let (left_result, right_result) = std::thread::scope(|scope| {
        let left = scope.spawn(|| scan_tree(left_root, &matcher));
        let right = scope.spawn(|| scan_tree(right_root, &matcher));
        (left.join(), right.join())
    });
    let (left, mut warnings) = left_result.unwrap_or_else(|_| {
        (
            BTreeMap::new(),
            vec!["the left directory scan stopped unexpectedly".into()],
        )
    });
    let (right, right_warnings) = right_result.unwrap_or_else(|_| {
        (
            BTreeMap::new(),
            vec!["the right directory scan stopped unexpectedly".into()],
        )
    });
    warnings.extend(right_warnings);
    let paths: Vec<PathBuf> = left
        .keys()
        .chain(right.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let next_path = AtomicUsize::new(0);
    let compared = Mutex::new(
        std::iter::repeat_with(|| None)
            .take(paths.len())
            .collect::<Vec<Option<Result<Option<DirectoryEntry>>>>>(),
    );
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(paths.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                loop {
                    let index = next_path.fetch_add(1, Ordering::Relaxed);
                    let Some(relative_path) = paths.get(index) else {
                        break;
                    };
                    let result = compare_node(
                        relative_path,
                        left.get(relative_path),
                        right.get(relative_path),
                        options,
                    );
                    compared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())[index] = Some(result);
                }
            });
        }
    });
    let compared: Vec<Result<Option<DirectoryEntry>>> = compared
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .into_iter()
        .map(|result| result.expect("each path is assigned to exactly one worker"))
        .collect();
    let mut entries = Vec::new();
    for (relative_path, result) in paths.into_iter().zip(compared) {
        match result {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(error) => entries.push(DirectoryEntry {
                relative_path,
                status: EntryStatus::Error,
                left: None,
                right: None,
                binary: true,
                message: Some(error.to_string()),
            }),
        }
    }

    Ok(ScanResult {
        entries,
        warnings,
        duration_ms: started.elapsed().as_millis(),
    })
}

fn side_preview(path: &Path) -> Result<SidePreview> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SidePreview::default());
        }
        Err(error) => return Err(error.into()),
    };
    let kind = entry_kind(metadata.file_type());
    Ok(SidePreview {
        exists: true,
        content: None,
        revision: Some(revision(path)?),
        size: metadata.len(),
        kind: Some(kind),
    })
}

pub fn read_comparison(
    left_root: &Path,
    right_root: &Path,
    relative_path: &Path,
) -> Result<ComparisonPreview> {
    let left_path = safe_join(left_root, relative_path)?;
    let right_path = safe_join(right_root, relative_path)?;
    let mut left = side_preview(&left_path)?;
    let mut right = side_preview(&right_path)?;

    if left.kind.is_some_and(|kind| kind != EntryKind::File)
        || right.kind.is_some_and(|kind| kind != EntryKind::File)
    {
        let both_symlinks =
            left.kind == Some(EntryKind::Symlink) && right.kind == Some(EntryKind::Symlink);
        return Ok(ComparisonPreview {
            relative_path: relative_path.to_path_buf(),
            kind: if both_symlinks {
                PreviewKind::Binary
            } else {
                PreviewKind::TypeMismatch
            },
            left,
            right,
            message: Some(if both_symlinks {
                "Symbolic links can be copied as a whole.".into()
            } else {
                "This path cannot be merged because the filesystem types differ.".into()
            }),
        });
    }
    if left.size > MAX_TEXT_PREVIEW_BYTES || right.size > MAX_TEXT_PREVIEW_BYTES {
        return Ok(ComparisonPreview {
            relative_path: relative_path.to_path_buf(),
            kind: PreviewKind::TooLarge,
            left,
            right,
            message: Some("The file exceeds the 2 MiB interactive preview limit.".into()),
        });
    }
    if (left.exists && is_binary(&left_path)?) || (right.exists && is_binary(&right_path)?) {
        return Ok(ComparisonPreview {
            relative_path: relative_path.to_path_buf(),
            kind: PreviewKind::Binary,
            left,
            right,
            message: Some("Binary files can be copied as a whole but not merged by hunk.".into()),
        });
    }

    left.content = Some(if left.exists {
        fs::read_to_string(&left_path)
            .with_context(|| format!("could not decode {} as UTF-8", left_path.display()))?
    } else {
        String::new()
    });
    right.content = Some(if right.exists {
        fs::read_to_string(&right_path)
            .with_context(|| format!("could not decode {} as UTF-8", right_path.display()))?
    } else {
        String::new()
    });
    Ok(ComparisonPreview {
        relative_path: relative_path.to_path_buf(),
        kind: PreviewKind::Text,
        left,
        right,
        message: None,
    })
}

pub fn ensure_expected_revision(path: &Path, expected: Option<&FileRevision>) -> Result<()> {
    match (fs::symlink_metadata(path), expected) {
        (Err(error), None) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        (Ok(_), None) => {
            bail!("the destination appeared after the preview was loaded; refresh first")
        }
        (Err(error), Some(_)) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("the destination disappeared after the preview was loaded; refresh first")
        }
        (Ok(_), Some(expected)) if &revision(path)? == expected => Ok(()),
        (Ok(_), Some(_)) => bail!("the destination changed on disk; refresh before merging"),
        (Err(error), _) => Err(anyhow!(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp::TempDir;
    use std::fs;

    #[test]
    fn finds_recursive_changes_and_honors_ignored_names() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        fs::create_dir_all(left.path().join("src")).unwrap();
        fs::create_dir_all(right.path().join("src")).unwrap();
        fs::write(left.path().join("src/shared.rs"), "left\n").unwrap();
        fs::write(right.path().join("src/shared.rs"), "right\n").unwrap();
        fs::write(left.path().join("left.txt"), "only left\n").unwrap();
        fs::create_dir_all(left.path().join("node_modules/pkg")).unwrap();
        fs::write(left.path().join("node_modules/pkg/index.js"), "ignored").unwrap();

        let result =
            scan_directories(left.path(), right.path(), &CompareOptions::default()).unwrap();
        assert_eq!(result.entries.len(), 2);
        assert_eq!(result.entries[0].relative_path, PathBuf::from("left.txt"));
        assert_eq!(result.entries[0].status, EntryStatus::LeftOnly);
        assert_eq!(result.entries[1].status, EntryStatus::Modified);
    }

    #[test]
    fn can_treat_line_endings_as_equal() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        fs::write(left.path().join("text.txt"), "one\ntwo\n").unwrap();
        fs::write(right.path().join("text.txt"), "one\r\ntwo\r\n").unwrap();

        let options = CompareOptions {
            show_identical: true,
            ..CompareOptions::default()
        };
        let result = scan_directories(left.path(), right.path(), &options).unwrap();
        assert_eq!(result.entries[0].status, EntryStatus::Identical);
    }

    #[test]
    fn rejects_parent_path_components() {
        assert!(safe_join(Path::new("/tmp/root"), Path::new("../escape")).is_err());
    }

    #[test]
    fn sha256_matches_the_standard_vector() {
        let mut hash = Sha256::new();
        hash.update(b"abc");
        assert_eq!(
            hash.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn minimal_globs_match_paths() {
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/bin/main.rs"));
        assert!(glob_matches("**/*.tmp", "a/b/cache.tmp"));
        assert!(glob_matches("**/*.tmp", "cache.tmp"));
    }
}
