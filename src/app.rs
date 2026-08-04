use crate::error::Result;
use crate::{
    config::AppConfig,
    diff::{
        DiffBlock, DiffHunk, DiffRow, DisplayLine, apply_hunk, create_diff_blocks, create_diff_rows,
    },
    merge::MergeManager,
    model::{
        ComparisonPreview, DirectoryEntry, EntryStatus, MergeDirection, PreviewKind, ScanResult,
        Side,
    },
    scanner::{read_comparison, scan_directories},
    zed::open_file_pair,
};
use gpui::{
    AnyElement, App, Application, Bounds, Context, IntoElement, PathPromptOptions, Render,
    ScrollHandle, SharedString, UniformListScrollHandle, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, relative, rgb, size, uniform_list,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

fn lock_merge_manager(manager: &Mutex<MergeManager>) -> MutexGuard<'_, MergeManager> {
    manager
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SIDEBAR_WIDTH: f32 = 318.0;
const GUTTER_WIDTH: f32 = 48.0;
const RAIL_WIDTH: f32 = 64.0;
const DIFF_ROW_HEIGHT: f32 = 24.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryFilter {
    All,
    Modified,
    LeftOnly,
    RightOnly,
}

impl EntryFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Modified => "Changed",
            Self::LeftOnly => "Left",
            Self::RightOnly => "Right",
        }
    }

    fn accepts(self, status: EntryStatus) -> bool {
        match self {
            Self::All => true,
            Self::Modified => matches!(
                status,
                EntryStatus::Modified | EntryStatus::TypeMismatch | EntryStatus::Error
            ),
            Self::LeftOnly => status == EntryStatus::LeftOnly,
            Self::RightOnly => status == EntryStatus::RightOnly,
        }
    }
}

#[derive(Clone)]
struct TreeRow {
    relative_path: PathBuf,
    label: String,
    depth: usize,
    folder: bool,
    status: EntryStatus,
}

pub struct FolderDiffApp {
    config: AppConfig,
    entries: Vec<DirectoryEntry>,
    warnings: Vec<String>,
    selected_path: Option<PathBuf>,
    preview: Option<ComparisonPreview>,
    diff_blocks: Vec<DiffBlock>,
    diff_rows: Vec<DiffRow>,
    filter: EntryFilter,
    collapsed_folders: HashSet<PathBuf>,
    merge_manager: Arc<Mutex<MergeManager>>,
    undo_available: bool,
    loading: bool,
    preview_loading: bool,
    show_settings: bool,
    pending_sync: Option<MergeDirection>,
    toast: Option<String>,
    scan_duration_ms: Option<u128>,
    tree_scroll: ScrollHandle,
    diff_scroll: UniformListScrollHandle,
}

impl FolderDiffApp {
    pub fn new(config: AppConfig, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            config,
            entries: Vec::new(),
            warnings: Vec::new(),
            selected_path: None,
            preview: None,
            diff_blocks: Vec::new(),
            diff_rows: Vec::new(),
            filter: EntryFilter::All,
            collapsed_folders: HashSet::new(),
            merge_manager: Arc::new(Mutex::new(MergeManager::new())),
            undo_available: false,
            loading: false,
            preview_loading: false,
            show_settings: false,
            pending_sync: None,
            toast: None,
            scan_duration_ms: None,
            tree_scroll: ScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
        };
        if this.roots().is_some() {
            this.refresh(cx);
        }
        this
    }

    fn roots(&self) -> Option<(&Path, &Path)> {
        Some((
            self.config.left_root.as_deref()?,
            self.config.right_root.as_deref()?,
        ))
    }

    fn save_config(&mut self) {
        if let Err(error) = self.config.save() {
            self.toast = Some(format!("Could not save settings: {error:#}"));
        }
    }

    fn pick_root(&mut self, side: Side, cx: &mut Context<Self>) {
        let prompt = match side {
            Side::Left => "Choose left directory",
            Side::Right => "Choose right directory",
        };
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn(async move |this, cx| {
            let result = selection.await;
            this.update(cx, |this, cx| {
                let selected = match result {
                    Ok(Ok(Some(paths))) => paths.into_iter().next(),
                    Ok(Ok(None)) => return,
                    Ok(Err(error)) => {
                        this.toast = Some(format!("Could not open the folder picker: {error:#}"));
                        cx.notify();
                        return;
                    }
                    Err(error) => {
                        this.toast = Some(format!("The folder picker was interrupted: {error}"));
                        cx.notify();
                        return;
                    }
                };
                let Some(selected) = selected else {
                    return;
                };
                let selected = selected.canonicalize().unwrap_or(selected);
                match side {
                    Side::Left => this.config.left_root = Some(selected),
                    Side::Right => this.config.right_root = Some(selected),
                }
                this.save_config();
                if this.roots().is_some() {
                    this.refresh(cx);
                } else {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn swap_roots(&mut self, cx: &mut Context<Self>) {
        std::mem::swap(&mut self.config.left_root, &mut self.config.right_root);
        self.save_config();
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some((left, right)) = self.roots() else {
            self.entries.clear();
            self.preview = None;
            self.diff_blocks.clear();
            self.diff_rows.clear();
            cx.notify();
            return;
        };
        let left = left.to_path_buf();
        let right = right.to_path_buf();
        let options = self.config.compare.clone();
        let selected = self.selected_path.clone();
        self.loading = true;
        self.toast = None;
        cx.notify();

        let task = cx.background_spawn(async move { scan_directories(&left, &right, &options) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.finish_scan(result, selected, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_scan(
        &mut self,
        result: Result<ScanResult>,
        previous_selection: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.loading = false;
        match result {
            Ok(scan) => {
                self.scan_duration_ms = Some(scan.duration_ms);
                self.warnings = scan.warnings;
                self.entries = scan.entries;
                let selection_still_exists = previous_selection.as_ref().is_some_and(|path| {
                    self.entries
                        .iter()
                        .any(|entry| &entry.relative_path == path)
                });
                self.selected_path = if selection_still_exists {
                    previous_selection
                } else {
                    self.entries
                        .iter()
                        .find(|entry| entry.status.is_difference())
                        .or_else(|| self.entries.first())
                        .map(|entry| entry.relative_path.clone())
                };
                if self.selected_path.is_some() {
                    self.load_preview(cx);
                } else {
                    self.preview = None;
                    self.diff_blocks.clear();
                    self.diff_rows.clear();
                }
            }
            Err(error) => {
                self.entries.clear();
                self.preview = None;
                self.diff_blocks.clear();
                self.diff_rows.clear();
                self.toast = Some(format!("Comparison failed: {error:#}"));
            }
        }
        cx.notify();
    }

    fn load_preview(&mut self, cx: &mut Context<Self>) {
        let Some((left, right)) = self.roots() else {
            return;
        };
        let Some(relative_path) = self.selected_path.clone() else {
            return;
        };
        let left = left.to_path_buf();
        let right = right.to_path_buf();
        self.preview_loading = true;
        cx.notify();
        let task = cx.background_spawn(async move {
            read_comparison(&left, &right, &relative_path).map(|preview| {
                let blocks = if preview.kind == PreviewKind::Text {
                    create_diff_blocks(
                        preview.left.content.as_deref().unwrap_or_default(),
                        preview.right.content.as_deref().unwrap_or_default(),
                    )
                } else {
                    Vec::new()
                };
                (relative_path, preview, blocks)
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.preview_loading = false;
                match result {
                    Ok((path, preview, blocks)) if this.selected_path.as_ref() == Some(&path) => {
                        this.diff_rows = create_diff_rows(&blocks);
                        this.preview = Some(preview);
                        this.diff_blocks = blocks;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        this.preview = None;
                        this.diff_blocks.clear();
                        this.diff_rows.clear();
                        this.toast = Some(format!("Could not preview file: {error:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_path(&mut self, relative_path: PathBuf, cx: &mut Context<Self>) {
        if self.selected_path.as_ref() == Some(&relative_path) {
            return;
        }
        self.selected_path = Some(relative_path);
        self.diff_scroll = UniformListScrollHandle::new();
        self.preview = None;
        self.diff_blocks.clear();
        self.diff_rows.clear();
        self.load_preview(cx);
    }

    fn select_adjacent(&mut self, offset: isize, cx: &mut Context<Self>) {
        let files: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| self.filter.accepts(entry.status))
            .map(|entry| entry.relative_path.clone())
            .collect();
        if files.is_empty() {
            return;
        }
        let current = self
            .selected_path
            .as_ref()
            .and_then(|path| files.iter().position(|candidate| candidate == path))
            .unwrap_or(0) as isize;
        let next = (current + offset).rem_euclid(files.len() as isize) as usize;
        self.select_path(files[next].clone(), cx);
    }

    fn merge_file(&mut self, direction: MergeDirection, cx: &mut Context<Self>) {
        let Some((left, right)) = self.roots() else {
            return;
        };
        let Some(path) = self.selected_path.clone() else {
            return;
        };
        let expected_source = self
            .preview
            .as_ref()
            .and_then(|preview| preview.side(direction.source()).revision.clone());
        let expected_destination = self
            .preview
            .as_ref()
            .and_then(|preview| preview.side(direction.destination()).revision.clone());
        let left = left.to_path_buf();
        let right = right.to_path_buf();
        let manager = self.merge_manager.clone();
        self.loading = true;
        cx.notify();
        let task = cx.background_spawn(async move {
            lock_merge_manager(&manager)
                .merge_file(
                    &left,
                    &right,
                    &path,
                    direction,
                    expected_source.as_ref(),
                    expected_destination.as_ref(),
                )
                .map(|result| (path, result))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok((path, operation)) => {
                    this.selected_path = Some(path);
                    this.undo_available = operation.undo_available;
                    this.toast = Some(format!(
                        "Merged {} path{} — Undo is available",
                        operation.changed_paths.len(),
                        if operation.changed_paths.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ));
                    this.refresh(cx);
                }
                Err(error) => {
                    this.loading = false;
                    this.toast = Some(format!("Merge stopped: {error:#}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn merge_hunk(&mut self, hunk_id: usize, direction: MergeDirection, cx: &mut Context<Self>) {
        let Some((left_root, right_root)) = self.roots() else {
            return;
        };
        let Some(preview) = self.preview.clone() else {
            return;
        };
        let Some(hunk) = self.diff_blocks.iter().find_map(|block| match block {
            DiffBlock::Hunk(hunk) if hunk.id == hunk_id => Some(hunk.clone()),
            _ => None,
        }) else {
            return;
        };
        let left_text = preview.left.content.as_deref().unwrap_or_default();
        let right_text = preview.right.content.as_deref().unwrap_or_default();
        let merged = apply_hunk(left_text, right_text, &hunk, direction);
        let destination_root = match direction.destination() {
            Side::Left => left_root.to_path_buf(),
            Side::Right => right_root.to_path_buf(),
        };
        let expected = preview.side(direction.destination()).revision.clone();
        let path = preview.relative_path.clone();
        let manager = self.merge_manager.clone();
        self.loading = true;
        cx.notify();
        let task = cx.background_spawn(async move {
            lock_merge_manager(&manager)
                .write_text(&destination_root, &path, &merged, expected.as_ref())
                .map(|result| (path, result))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok((path, operation)) => {
                    this.selected_path = Some(path);
                    this.undo_available = operation.undo_available;
                    this.toast = Some("Hunk merged — Undo is available".into());
                    this.refresh(cx);
                }
                Err(error) => {
                    this.loading = false;
                    this.toast = Some(format!("Hunk merge stopped: {error:#}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn synchronize(&mut self, direction: MergeDirection, cx: &mut Context<Self>) {
        let Some((left, right)) = self.roots() else {
            return;
        };
        let left = left.to_path_buf();
        let right = right.to_path_buf();
        let options = self.config.compare.clone();
        let manager = self.merge_manager.clone();
        self.pending_sync = None;
        self.loading = true;
        cx.notify();
        let task = cx.background_spawn(async move {
            lock_merge_manager(&manager).synchronize(&left, &right, &options, direction)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(operation) => {
                    this.undo_available = operation.undo_available;
                    let mut message = format!(
                        "Synchronized {} path{} — no target-only files were deleted",
                        operation.changed_paths.len(),
                        if operation.changed_paths.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    );
                    if !operation.warnings.is_empty() {
                        message.push_str(&format!("; {} skipped", operation.warnings.len()));
                    }
                    this.toast = Some(message);
                    this.refresh(cx);
                }
                Err(error) => {
                    this.loading = false;
                    this.toast = Some(format!("Synchronization stopped: {error:#}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let manager = self.merge_manager.clone();
        self.loading = true;
        cx.notify();
        let task = cx.background_spawn(async move { lock_merge_manager(&manager).undo() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok((label, operation)) if !operation.changed_paths.is_empty() => {
                    this.undo_available = operation.undo_available;
                    this.toast = Some(format!("Undid {label}"));
                    this.refresh(cx);
                }
                Ok(_) => {
                    this.loading = false;
                    this.undo_available = false;
                    this.toast = Some("Nothing to undo".into());
                    cx.notify();
                }
                Err(error) => {
                    this.loading = false;
                    this.toast = Some(format!("Undo failed: {error:#}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_selected_in_zed(&mut self, cx: &mut Context<Self>) {
        let Some((left, right)) = self.roots() else {
            return;
        };
        let Some(path) = self.selected_path.as_deref() else {
            return;
        };
        self.toast = Some(match open_file_pair(left, right, path) {
            Ok(()) => "Opened the pair in Zed's native diff view".into(),
            Err(error) => format!("Could not open Zed: {error:#}"),
        });
        cx.notify();
    }

    fn set_filter(&mut self, filter: EntryFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    fn toggle_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.collapsed_folders.insert(path.clone()) {
            self.collapsed_folders.remove(&path);
        }
        cx.notify();
    }

    fn toggle_option(&mut self, option: &'static str, cx: &mut Context<Self>) {
        match option {
            "line-endings" => {
                self.config.compare.ignore_line_endings = !self.config.compare.ignore_line_endings
            }
            "whitespace" => {
                self.config.compare.ignore_whitespace = !self.config.compare.ignore_whitespace
            }
            "identical" => self.config.compare.show_identical = !self.config.compare.show_identical,
            _ => return,
        }
        self.save_config();
        self.refresh(cx);
    }

    fn visible_entries(&self) -> impl Iterator<Item = &DirectoryEntry> {
        self.entries
            .iter()
            .filter(|entry| self.filter.accepts(entry.status))
    }

    fn tree_rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        let mut seen_folders = HashSet::new();
        for entry in self.visible_entries() {
            let components: Vec<_> = entry.relative_path.components().collect();
            let mut folder_path = PathBuf::new();
            let mut hidden = false;
            for (depth, component) in components
                .iter()
                .take(components.len().saturating_sub(1))
                .enumerate()
            {
                folder_path.push(component.as_os_str());
                if seen_folders.insert(folder_path.clone()) && !hidden {
                    rows.push(TreeRow {
                        relative_path: folder_path.clone(),
                        label: component.as_os_str().to_string_lossy().into_owned(),
                        depth,
                        folder: true,
                        status: entry.status,
                    });
                }
                if self.collapsed_folders.contains(&folder_path) {
                    hidden = true;
                }
            }
            if !hidden {
                rows.push(TreeRow {
                    relative_path: entry.relative_path.clone(),
                    label: entry
                        .relative_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    depth: components.len().saturating_sub(1),
                    folder: false,
                    status: entry.status,
                });
            }
        }
        rows
    }

    fn status_label(status: EntryStatus) -> (&'static str, u32) {
        match status {
            EntryStatus::Modified => ("M", 0xd6a84b),
            EntryStatus::LeftOnly => ("L", 0x61afef),
            EntryStatus::RightOnly => ("R", 0x56b6c2),
            EntryStatus::Identical => ("=", 0x7f8795),
            EntryStatus::TypeMismatch => ("!", 0xe06c75),
            EntryStatus::Error => ("×", 0xe06c75),
        }
    }

    fn action_button(
        &self,
        id: String,
        label: impl Into<SharedString>,
        enabled: bool,
        primary: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let background = if primary { 0x2f6feb } else { 0x242933 };
        let hover = if primary { 0x3979f6 } else { 0x303744 };
        let button = div()
            .id(SharedString::from(id))
            .flex_none()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(if primary { 0x4a82ee } else { 0x3a414d }))
            .bg(rgb(background))
            .text_size(px(12.0))
            .text_color(rgb(0xe7eaf0))
            .whitespace_nowrap()
            .hover(move |style| style.bg(rgb(hover)))
            .child(label.into());
        if enabled {
            button
                .on_click(cx.listener(move |this, _, window, cx| on_click(this, window, cx)))
                .into_any_element()
        } else {
            button.opacity(0.38).into_any_element()
        }
    }

    fn render_root_selector(&self, side: Side, cx: &mut Context<Self>) -> AnyElement {
        let path = match side {
            Side::Left => self.config.left_root.as_ref(),
            Side::Right => self.config.right_root.as_ref(),
        };
        let side_name = match side {
            Side::Left => "LEFT",
            Side::Right => "RIGHT",
        };
        let path_label = path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Choose a directory…".into());
        div()
            .id(SharedString::from(format!("root-{side_name}")))
            .flex()
            .flex_col()
            .flex_1()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x343b47))
            .bg(rgb(0x191d24))
            .hover(|style| style.bg(rgb(0x20252e)))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(0x7f8795))
                    .child(side_name),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(0xe0e4eb))
                    .truncate()
                    .child(path_label),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.pick_root(side, cx)))
            .into_any_element()
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let can_compare = self.roots().is_some();
        let can_undo = self.undo_available;
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(0x303641))
            .bg(rgb(0x12151a))
            .child(
                div()
                    .w(px(152.0))
                    .text_size(px(15.0))
                    .text_color(rgb(0xf0f2f6))
                    .child("Folder Diff"),
            )
            .child(self.render_root_selector(Side::Left, cx))
            .child(self.action_button(
                "swap-roots".into(),
                "⇄ Swap",
                can_compare,
                false,
                cx,
                |this, _, cx| this.swap_roots(cx),
            ))
            .child(self.render_root_selector(Side::Right, cx))
            .child(self.action_button(
                "refresh".into(),
                if self.loading {
                    "Scanning…"
                } else {
                    "↻ Refresh"
                },
                can_compare && !self.loading,
                false,
                cx,
                |this, _, cx| this.refresh(cx),
            ))
            .child(self.action_button(
                "undo".into(),
                "↶ Undo",
                can_undo && !self.loading,
                false,
                cx,
                |this, _, cx| this.undo(cx),
            ))
            .child(
                self.action_button("settings".into(), "⚙", true, false, cx, |this, _, cx| {
                    this.show_settings = !this.show_settings;
                    this.pending_sync = None;
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn render_filter(&self, filter: EntryFilter, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.filter == filter;
        let count = self
            .entries
            .iter()
            .filter(|entry| filter.accepts(entry.status))
            .count();
        let button = div()
            .id(SharedString::from(format!("filter-{}", filter.label())))
            .px_2()
            .py_1()
            .rounded_md()
            .text_size(px(11.0))
            .text_color(rgb(if selected { 0xf3f5f8 } else { 0x929aa8 }))
            .bg(rgb(if selected { 0x343b49 } else { 0x1a1e25 }))
            .child(format!("{} {count}", filter.label()));
        button
            .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            .into_any_element()
    }

    fn render_tree_row(&self, row: TreeRow, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let selected = !row.folder && self.selected_path.as_ref() == Some(&row.relative_path);
        let (status, color) = Self::status_label(row.status);
        let indent = 12.0 + row.depth as f32 * 14.0;
        let prefix = if row.folder {
            if self.collapsed_folders.contains(&row.relative_path) {
                "▸"
            } else {
                "▾"
            }
        } else {
            ""
        };
        let path = row.relative_path.clone();
        let folder = row.folder;
        div()
            .id(SharedString::from(format!("tree-row-{index}")))
            .flex()
            .items_center()
            .gap_2()
            .pl(px(indent))
            .pr_2()
            .py_1()
            .bg(rgb(if selected { 0x24364d } else { 0x171a20 }))
            .hover(|style| style.bg(rgb(0x222730)))
            .text_size(px(12.0))
            .child(div().w(px(12.0)).text_color(rgb(0x87909f)).child(prefix))
            .child(
                div()
                    .flex_1()
                    .text_color(rgb(if row.folder { 0xb3bbc8 } else { 0xdde1e8 }))
                    .child(row.label),
            )
            .child(
                div()
                    .w(px(16.0))
                    .text_color(rgb(color))
                    .child(if row.folder { "" } else { status }),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if folder {
                    this.toggle_folder(path.clone(), cx);
                } else {
                    this.select_path(path.clone(), cx);
                }
            }))
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.tree_rows();
        let differing = self
            .entries
            .iter()
            .filter(|entry| entry.status.is_difference())
            .count();
        let duration = self
            .scan_duration_ms
            .map(|duration| format!(" · {duration} ms"))
            .unwrap_or_default();
        let mut tree = div()
            .id("tree-scroll")
            .flex_1()
            .overflow_y_scroll()
            .scrollbar_width(px(10.0))
            .track_scroll(&self.tree_scroll)
            .py_1();
        if rows.is_empty() && !self.loading {
            tree = tree.child(
                div()
                    .p_4()
                    .text_size(px(12.0))
                    .text_color(rgb(0x8c95a4))
                    .child(if self.entries.is_empty() {
                        "No differences found."
                    } else {
                        "No files match this filter."
                    }),
            );
        } else {
            tree = tree.children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, row)| self.render_tree_row(row, index, cx)),
            );
        }

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .border_r_1()
            .border_color(rgb(0x303641))
            .bg(rgb(0x171a20))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(0x2d333d))
                    .children(
                        [
                            EntryFilter::All,
                            EntryFilter::Modified,
                            EntryFilter::LeftOnly,
                            EntryFilter::RightOnly,
                        ]
                        .into_iter()
                        .map(|filter| self.render_filter(filter, cx)),
                    ),
            )
            .child(tree)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(rgb(0x2d333d))
                    .text_size(px(11.0))
                    .text_color(rgb(0x858e9d))
                    .child(format!("{differing} differences{duration}"))
                    .child(if self.warnings.is_empty() {
                        String::new()
                    } else {
                        format!("{} warnings", self.warnings.len())
                    }),
            )
            .into_any_element()
    }

    fn render_file_toolbar(
        &self,
        preview: &ComparisonPreview,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let left_exists = preview.left.exists;
        let right_exists = preview.right.exists;
        let both_exist = left_exists && right_exists;
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(0x303641))
            .bg(rgb(0x171b22))
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(13.0))
                    .text_color(rgb(0xe4e7ed))
                    .child(preview.relative_path.display().to_string()),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(self.action_button(
                        "previous-file".into(),
                        "↑ Previous",
                        !self.entries.is_empty(),
                        false,
                        cx,
                        |this, _, cx| this.select_adjacent(-1, cx),
                    ))
                    .child(self.action_button(
                        "next-file".into(),
                        "↓ Next",
                        !self.entries.is_empty(),
                        false,
                        cx,
                        |this, _, cx| this.select_adjacent(1, cx),
                    ))
                    .child(self.action_button(
                        "open-zed".into(),
                        "Open in Zed",
                        both_exist,
                        false,
                        cx,
                        |this, _, cx| this.open_selected_in_zed(cx),
                    ))
                    .child(self.action_button(
                        "copy-left-right".into(),
                        "Copy file →",
                        left_exists && !self.loading,
                        true,
                        cx,
                        |this, _, cx| this.merge_file(MergeDirection::LeftToRight, cx),
                    ))
                    .child(self.action_button(
                        "copy-right-left".into(),
                        "← Copy file",
                        right_exists && !self.loading,
                        true,
                        cx,
                        |this, _, cx| this.merge_file(MergeDirection::RightToLeft, cx),
                    )),
            )
            .into_any_element()
    }

    fn render_column_header(&self, side: Side, preview: &ComparisonPreview) -> AnyElement {
        let side_preview = preview.side(side);
        let title = match side {
            Side::Left => "LEFT DIRECTORY",
            Side::Right => "RIGHT DIRECTORY",
        };
        div()
            .flex_1()
            .flex_basis(relative(0.5))
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .bg(rgb(0x151920))
            .border_b_1()
            .border_color(rgb(0x2d333d))
            .text_size(px(11.0))
            .text_color(rgb(0x929baa))
            .child(title)
            .child(if side_preview.exists {
                format!("{} bytes", side_preview.size)
            } else {
                "Missing".into()
            })
            .into_any_element()
    }

    fn render_code_cell(
        &self,
        line: Option<&DisplayLine>,
        background: u32,
        text_color: u32,
    ) -> AnyElement {
        let line_number = line.map(|line| line.number.to_string()).unwrap_or_default();
        let text = line
            .map(|line| SharedString::new(line.text.clone()))
            .unwrap_or_default();
        div()
            .flex()
            .flex_1()
            .flex_basis(relative(0.5))
            .overflow_hidden()
            .h(px(DIFF_ROW_HEIGHT))
            .bg(rgb(background))
            .text_size(px(12.0))
            .child(
                div()
                    .w(px(GUTTER_WIDTH))
                    .h_full()
                    .px_2()
                    .text_color(rgb(0x66707f))
                    .bg(rgb(if background == 0x12151a {
                        0x15191f
                    } else {
                        background
                    }))
                    .child(line_number),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .px_2()
                    .text_color(rgb(text_color))
                    .child(text),
            )
            .into_any_element()
    }

    fn render_split_line(
        &self,
        left: Option<&DisplayLine>,
        right: Option<&DisplayLine>,
        left_background: u32,
        left_text: u32,
        right_background: u32,
        right_text: u32,
    ) -> AnyElement {
        div()
            .flex()
            .flex_none()
            .w_full()
            .h(px(DIFF_ROW_HEIGHT))
            .child(self.render_code_cell(left, left_background, left_text))
            .child(div().w(px(RAIL_WIDTH)).h_full().bg(rgb(0x1b1f26)))
            .child(self.render_code_cell(right, right_background, right_text))
            .into_any_element()
    }

    fn render_hunk_header(&self, hunk: &DiffHunk, cx: &mut Context<Self>) -> AnyElement {
        let left_range = if hunk.left.is_empty() {
            "empty".into()
        } else {
            format!(
                "lines {}–{}",
                hunk.left_start_line,
                hunk.left_start_line + hunk.left.len().saturating_sub(1)
            )
        };
        let right_range = if hunk.right.is_empty() {
            "empty".into()
        } else {
            format!(
                "lines {}–{}",
                hunk.right_start_line,
                hunk.right_start_line + hunk.right.len().saturating_sub(1)
            )
        };
        let hunk_id = hunk.id;
        div()
            .flex()
            .flex_none()
            .w_full()
            .h(px(DIFF_ROW_HEIGHT))
            .border_t_1()
            .border_b_1()
            .border_color(rgb(0x3b424e))
            .child(
                div()
                    .flex_1()
                    .px_3()
                    .py_1()
                    .bg(rgb(0x2a2023))
                    .text_size(px(10.0))
                    .text_color(rgb(0xd9959b))
                    .child(left_range),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .w(px(RAIL_WIDTH))
                    .bg(rgb(0x20252d))
                    .child(self.action_button(
                        format!("hunk-{hunk_id}-right"),
                        "→",
                        true,
                        true,
                        cx,
                        move |this, _, cx| {
                            this.merge_hunk(hunk_id, MergeDirection::LeftToRight, cx)
                        },
                    ))
                    .child(self.action_button(
                        format!("hunk-{hunk_id}-left"),
                        "←",
                        true,
                        true,
                        cx,
                        move |this, _, cx| {
                            this.merge_hunk(hunk_id, MergeDirection::RightToLeft, cx)
                        },
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .px_3()
                    .py_1()
                    .bg(rgb(0x1d2924))
                    .text_size(px(10.0))
                    .text_color(rgb(0x83c797))
                    .child(right_range),
            )
            .into_any_element()
    }

    fn render_diff_row(&self, row_index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.diff_rows.get(row_index).copied() else {
            return div().h(px(DIFF_ROW_HEIGHT)).into_any_element();
        };
        match row {
            DiffRow::Equal {
                block_index,
                line_index,
            } => {
                let Some(DiffBlock::Equal { left, right }) = self.diff_blocks.get(block_index)
                else {
                    return div().h(px(DIFF_ROW_HEIGHT)).into_any_element();
                };
                self.render_split_line(
                    left.get(line_index),
                    right.get(line_index),
                    0x12151a,
                    0xb9c0cb,
                    0x12151a,
                    0xb9c0cb,
                )
            }
            DiffRow::EqualGap { omitted_lines } => div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w_full()
                .h(px(DIFF_ROW_HEIGHT))
                .bg(rgb(0x1a1e25))
                .border_t_1()
                .border_b_1()
                .border_color(rgb(0x303641))
                .text_size(px(10.0))
                .text_color(rgb(0x747e8e))
                .child(format!("⋯ {omitted_lines} unchanged lines ⋯"))
                .into_any_element(),
            DiffRow::HunkHeader { block_index } => {
                let Some(DiffBlock::Hunk(hunk)) = self.diff_blocks.get(block_index) else {
                    return div().h(px(DIFF_ROW_HEIGHT)).into_any_element();
                };
                self.render_hunk_header(hunk, cx)
            }
            DiffRow::HunkLine {
                block_index,
                line_index,
            } => {
                let Some(DiffBlock::Hunk(hunk)) = self.diff_blocks.get(block_index) else {
                    return div().h(px(DIFF_ROW_HEIGHT)).into_any_element();
                };
                self.render_split_line(
                    hunk.left.get(line_index),
                    hunk.right.get(line_index),
                    0x2a1c20,
                    0xf0c3c8,
                    0x172820,
                    0xbbe7c6,
                )
            }
        }
    }

    fn render_text_diff(&self, preview: &ComparisonPreview, cx: &mut Context<Self>) -> AnyElement {
        let content = if self.diff_rows.is_empty() {
            let empty_side = || {
                div()
                    .flex()
                    .flex_1()
                    .flex_basis(relative(0.5))
                    .items_center()
                    .justify_center()
                    .h(px(72.0))
                    .text_size(px(12.0))
                    .text_color(rgb(0x8f98a7))
                    .child("Empty file")
            };
            div()
                .flex_1()
                .bg(rgb(0x12151a))
                .child(
                    div()
                        .flex()
                        .child(empty_side())
                        .child(div().w(px(RAIL_WIDTH)).h(px(72.0)).bg(rgb(0x171a20)))
                        .child(empty_side()),
                )
                .into_any_element()
        } else {
            uniform_list(
                "diff-scroll",
                self.diff_rows.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|row_index| this.render_diff_row(row_index, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .w_full()
            .bg(rgb(0x12151a))
            .track_scroll(self.diff_scroll.clone())
            .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .child(self.render_column_header(Side::Left, preview))
                    .child(div().w(px(RAIL_WIDTH)).bg(rgb(0x171a20)))
                    .child(self.render_column_header(Side::Right, preview)),
            )
            .child(content)
            .into_any_element()
    }

    fn render_non_text(&self, preview: &ComparisonPreview, cx: &mut Context<Self>) -> AnyElement {
        let title = match preview.kind {
            PreviewKind::Binary => "Binary or symbolic-link comparison",
            PreviewKind::TooLarge => "Large file comparison",
            PreviewKind::TypeMismatch => "Filesystem type mismatch",
            PreviewKind::Text => "Text comparison",
        };
        let side_panel = |side: Side| {
            let side_preview = preview.side(side);
            let state = if side_preview.exists {
                format!("{} bytes · {:?}", side_preview.size, side_preview.kind)
            } else {
                "Missing on this side".into()
            };
            div()
                .flex()
                .flex_col()
                .flex_1()
                .flex_basis(relative(0.5))
                .items_center()
                .justify_center()
                .gap_3()
                .overflow_hidden()
                .px_4()
                .child(
                    div()
                        .text_size(px(18.0))
                        .text_color(rgb(0xe2e6ed))
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(0x9099a8))
                        .child(state),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(0x7f8998))
                        .child(preview.message.clone().unwrap_or_default()),
                )
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .bg(rgb(0x12151a))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .child(self.render_column_header(Side::Left, preview))
                    .child(div().w(px(RAIL_WIDTH)).bg(rgb(0x171a20)))
                    .child(self.render_column_header(Side::Right, preview)),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(side_panel(Side::Left))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .w(px(RAIL_WIDTH))
                            .bg(rgb(0x1b1f26))
                            .child(self.action_button(
                                "binary-copy-right".into(),
                                "→",
                                preview.left.exists,
                                true,
                                cx,
                                |this, _, cx| this.merge_file(MergeDirection::LeftToRight, cx),
                            ))
                            .child(self.action_button(
                                "binary-copy-left".into(),
                                "←",
                                preview.right.exists,
                                true,
                                cx,
                                |this, _, cx| this.merge_file(MergeDirection::RightToLeft, cx),
                            )),
                    )
                    .child(side_panel(Side::Right)),
            )
            .into_any_element()
    }

    fn render_comparison(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(preview) = self.preview.as_ref() else {
            return div()
                .flex()
                .items_center()
                .justify_center()
                .flex_1()
                .bg(rgb(0x12151a))
                .text_color(rgb(0x8d96a5))
                .child(if self.preview_loading {
                    "Loading comparison…"
                } else if self.entries.is_empty() {
                    "The directories match under the current settings."
                } else {
                    "Choose a file from the comparison tree."
                })
                .into_any_element();
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .child(self.render_file_toolbar(preview, cx))
            .child(match preview.kind {
                PreviewKind::Text => self.render_text_diff(preview, cx),
                _ => self.render_non_text(preview, cx),
            })
            .into_any_element()
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .flex_1()
            .bg(rgb(0x12151a))
            .child(
                div()
                    .text_size(px(28.0))
                    .text_color(rgb(0xf0f2f6))
                    .child("Compare two directory trees"),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(0x929baa))
                    .child("Recursive, side-by-side comparison with safe two-way merge and undo."),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.action_button(
                        "welcome-left".into(),
                        "Choose left directory",
                        true,
                        true,
                        cx,
                        |this, _, cx| this.pick_root(Side::Left, cx),
                    ))
                    .child(self.action_button(
                        "welcome-right".into(),
                        "Choose right directory",
                        true,
                        true,
                        cx,
                        |this, _, cx| this.pick_root(Side::Right, cx),
                    )),
            )
            .into_any_element()
    }

    fn render_toggle(
        &self,
        id: &'static str,
        title: &'static str,
        description: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(format!("setting-{id}")))
            .flex()
            .items_center()
            .justify_between()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(rgb(0x343b46))
            .bg(rgb(0x1b1f27))
            .hover(|style| style.bg(rgb(0x222730)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(rgb(0xe5e8ee))
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(rgb(0x8992a1))
                            .child(description),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(if enabled { 0x2f6feb } else { 0x343b46 }))
                    .text_size(px(10.0))
                    .text_color(rgb(0xf1f4f8))
                    .child(if enabled { "ON" } else { "OFF" }),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_option(id, cx)))
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .flex_1()
            .p_6()
            .bg(rgb(0x12151a))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .w(px(680.0))
                    .mb_4()
                    .child(
                        div()
                            .text_size(px(22.0))
                            .text_color(rgb(0xf0f2f6))
                            .child("Comparison settings"),
                    )
                    .child(self.action_button(
                        "close-settings".into(),
                        "Done",
                        true,
                        true,
                        cx,
                        |this, _, cx| {
                            this.show_settings = false;
                            cx.notify();
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .w(px(680.0))
                    .child(self.render_toggle(
                        "line-endings",
                        "Ignore line-ending differences",
                        "Treat CRLF, CR, and LF as equivalent during the directory scan.",
                        self.config.compare.ignore_line_endings,
                        cx,
                    ))
                    .child(self.render_toggle(
                        "whitespace",
                        "Ignore whitespace differences",
                        "Ignore spaces, tabs, and line breaks while determining file status.",
                        self.config.compare.ignore_whitespace,
                        cx,
                    ))
                    .child(self.render_toggle(
                        "identical",
                        "Show identical files",
                        "Include unchanged files in the comparison tree.",
                        self.config.compare.show_identical,
                        cx,
                    ))
                    .child(
                        div()
                            .mt_3()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x343b46))
                            .bg(rgb(0x1b1f27))
                            .child(
                                div()
                                    .mb_2()
                                    .text_size(px(13.0))
                                    .text_color(rgb(0xe5e8ee))
                                    .child("Ignored paths"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(0x929baa))
                                    .child(self.config.compare.ignore_patterns.join("  ·  ")),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_sync_confirmation(
        &self,
        direction: MergeDirection,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copyable = self
            .entries
            .iter()
            .filter(|entry| match direction {
                MergeDirection::LeftToRight => {
                    matches!(entry.status, EntryStatus::Modified | EntryStatus::LeftOnly)
                }
                MergeDirection::RightToLeft => {
                    matches!(entry.status, EntryStatus::Modified | EntryStatus::RightOnly)
                }
            })
            .count();
        let direction_label = match direction {
            MergeDirection::LeftToRight => "left → right",
            MergeDirection::RightToLeft => "right → left",
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .flex_1()
            .bg(rgb(0x12151a))
            .child(
                div()
                    .text_size(px(22.0))
                    .text_color(rgb(0xf0f2f6))
                    .child(format!("Synchronize {direction_label}?")),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(rgb(0xa0a8b5))
                    .child(format!(
                        "{copyable} files will be copied. Target-only files will not be deleted."
                    )),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(rgb(0xd4a451))
                    .child("Type mismatches are skipped. The complete operation can be undone."),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(self.action_button(
                        "cancel-sync".into(),
                        "Cancel",
                        true,
                        false,
                        cx,
                        |this, _, cx| {
                            this.pending_sync = None;
                            cx.notify();
                        },
                    ))
                    .child(self.action_button(
                        "confirm-sync".into(),
                        format!("Synchronize {copyable} files"),
                        copyable > 0,
                        true,
                        cx,
                        move |this, _, cx| this.synchronize(direction, cx),
                    )),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.show_settings {
            return self.render_settings(cx);
        }
        if let Some(direction) = self.pending_sync {
            return self.render_sync_confirmation(direction, cx);
        }
        if self.roots().is_none() {
            return self.render_welcome(cx);
        }
        div()
            .flex()
            .flex_1()
            .overflow_hidden()
            .child(self.render_sidebar(cx))
            .child(self.render_comparison(cx))
            .into_any_element()
    }

    fn render_bottom_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let toast = self.toast.clone().unwrap_or_else(|| {
            if self.loading {
                "Scanning directories…".into()
            } else {
                "Ready".into()
            }
        });
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(rgb(0x303641))
            .bg(rgb(0x15181e))
            .text_size(px(11.0))
            .text_color(rgb(0x929baa))
            .child(div().flex_1().child(toast))
            .child(self.action_button(
                "sync-left-right".into(),
                "Synchronize all →",
                self.roots().is_some() && !self.loading,
                false,
                cx,
                |this, _, cx| {
                    this.pending_sync = Some(MergeDirection::LeftToRight);
                    this.show_settings = false;
                    cx.notify();
                },
            ))
            .child(self.action_button(
                "sync-right-left".into(),
                "← Synchronize all",
                self.roots().is_some() && !self.loading,
                false,
                cx,
                |this, _, cx| {
                    this.pending_sync = Some(MergeDirection::RightToLeft);
                    this.show_settings = false;
                    cx.notify();
                },
            ))
            .into_any_element()
    }
}

impl Render for FolderDiffApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x12151a))
            .text_color(rgb(0xdde1e8))
            .child(self.render_top_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_body(cx)),
            )
            .child(self.render_bottom_bar(cx).into_any_element())
    }
}

pub fn run(config: AppConfig) {
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1480.0), px(920.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(980.0), px(640.0))),
                ..Default::default()
            },
            move |_, cx| cx.new(move |cx| FolderDiffApp::new(config, cx)),
        )
        .expect("could not open Folder Diff window");
        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
