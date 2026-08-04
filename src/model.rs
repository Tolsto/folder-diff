use std::{path::PathBuf, time::SystemTime};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergeDirection {
    LeftToRight,
    RightToLeft,
}

impl MergeDirection {
    pub fn source(self) -> Side {
        match self {
            Self::LeftToRight => Side::Left,
            Self::RightToLeft => Side::Right,
        }
    }

    pub fn destination(self) -> Side {
        self.source().opposite()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryStatus {
    Modified,
    LeftOnly,
    RightOnly,
    Identical,
    TypeMismatch,
    Error,
}

impl EntryStatus {
    pub fn is_difference(self) -> bool {
        self != Self::Identical
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRevision(pub String);

#[derive(Clone, Debug)]
pub struct FileMetadata {
    pub relative_path: PathBuf,
    pub absolute_path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified_at: Option<SystemTime>,
    pub readonly: bool,
    pub symlink_target: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct DirectoryEntry {
    pub relative_path: PathBuf,
    pub status: EntryStatus,
    pub left: Option<FileMetadata>,
    pub right: Option<FileMetadata>,
    pub binary: bool,
    pub message: Option<String>,
}

impl DirectoryEntry {
    pub fn source(&self, direction: MergeDirection) -> Option<&FileMetadata> {
        match direction.source() {
            Side::Left => self.left.as_ref(),
            Side::Right => self.right.as_ref(),
        }
    }

    pub fn destination(&self, direction: MergeDirection) -> Option<&FileMetadata> {
        match direction.destination() {
            Side::Left => self.left.as_ref(),
            Side::Right => self.right.as_ref(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompareOptions {
    pub ignore_patterns: Vec<String>,
    pub ignore_whitespace: bool,
    pub ignore_line_endings: bool,
    pub show_identical: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            ignore_patterns: vec![
                ".git".into(),
                "node_modules".into(),
                ".DS_Store".into(),
                "dist".into(),
                "build".into(),
                "target".into(),
                ".idea".into(),
            ],
            ignore_whitespace: false,
            ignore_line_endings: true,
            show_identical: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    pub entries: Vec<DirectoryEntry>,
    pub warnings: Vec<String>,
    pub duration_ms: u128,
}

#[derive(Clone, Debug, Default)]
pub struct SidePreview {
    pub exists: bool,
    pub content: Option<String>,
    pub revision: Option<FileRevision>,
    pub size: u64,
    pub kind: Option<EntryKind>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreviewKind {
    Text,
    Binary,
    TooLarge,
    TypeMismatch,
}

#[derive(Clone, Debug)]
pub struct ComparisonPreview {
    pub relative_path: PathBuf,
    pub kind: PreviewKind,
    pub left: SidePreview,
    pub right: SidePreview,
    pub message: Option<String>,
}

impl ComparisonPreview {
    pub fn side(&self, side: Side) -> &SidePreview {
        match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OperationResult {
    pub changed_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub undo_available: bool,
}
