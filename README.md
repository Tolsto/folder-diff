# Folder Diff for Zed

Folder Diff is a native Rust/GPUI companion for Zed on macOS that compares two directory trees, shows every changed path recursively, renders a side-by-side line diff, and merges files or individual hunks in either direction.

## What is implemented

- Recursive directory scanning with a collapsible folder tree.
- Modified, left-only, right-only, identical, type-mismatch, and error states.
- Filters for all, changed, left-only, and right-only paths.
- Side-by-side Myers line diff with collapsed unchanged regions.
- Per-hunk merge arrows and whole-file copy in both directions.
- Binary, large-file, symlink, and filesystem-type previews.
- Bulk left-to-right or right-to-left synchronization with confirmation.
- Transactional merge rollback and a 20-operation undo stack.
- SHA-256 source and destination revision checks before interactive copies.
- Protection against path traversal and writes through symlinked parent directories.
- Configurable line-ending/whitespace equivalence, identical-file visibility, and default ignores.
- Native folder pickers, background scanning, remembered roots, and Zed's native `--diff` view.
- A permanently split left/right comparison with scrollable content and wrapping toolbars.
- CLI roots for Zed tasks: `folder-diff --left PATH --right PATH`.
- One declared dependency: GPUI. The scan, diff, hashing, CLI, config, and merge layers use only Rust's standard library.

Bulk synchronization intentionally **does not delete target-only files**. A path is copied only when the selected source side has a file or symbolic link; type mismatches are skipped and reported.

## Run from source

Prerequisites:

- Rust 1.88 or newer.
- macOS 12+ and the Xcode Command Line Tools.
- Zed's CLI installed if you want to use **Open in Zed** (`Zed > Install CLI`).

```sh
cargo run --release
```

You can preselect either or both roots:

```sh
cargo run --release -- --left /path/to/old --right /path/to/new
```

To ignore remembered roots and choose both directories again:

```sh
cargo run --release -- --choose
```

The app remembers the most recently chosen directories and comparison options under `~/Library/Application Support`.

## Dependency boundary

`Cargo.toml` declares only `gpui`. All former utility dependencies have been replaced with focused standard-library implementations:

| Former crate | Replacement |
| --- | --- |
| `clap` | Small `std::env` argument parser |
| `anyhow` | Internal boxed error and context helper |
| `directories` | macOS Application Support path |
| `globset` / `walkdir` | Internal `*`, `?`, and `**` matching plus `std::fs` traversal |
| `rayon` / `parking_lot` | Scoped standard threads and `std::sync::Mutex` |
| `serde` / `serde_json` | Versioned line-based configuration |
| `sha2` | Internal SHA-256 implementation with a standard test vector |
| `similar` | Internal Myers line/word diff |
| `tempfile` | Exclusive-create temporary paths with cleanup |

With default features disabled, the core resolves only the Folder Diff package itself. The full macOS application still inherits GPUI's transitive graphics, text, media, and platform graph; keeping GPUI necessarily keeps those crates. See [`docs/DEPENDENCIES.md`](docs/DEPENDENCIES.md) for the exact boundary and tradeoffs.

## Build a macOS application

```sh
cargo install cargo-bundle
cargo bundle --release
open "target/release/bundle/osx/Folder Diff.app"
```

For a command available to Zed tasks:

```sh
cargo install --path .
```

## Launch it from Zed

1. Build or install `folder-diff` so the binary is on your shell `PATH`.
2. In Zed, run **zed: open tasks**.
3. Add the task objects from [`integration/zed-tasks.json`](integration/zed-tasks.json) to that array.
4. Run **task: spawn**, then choose **Folder Diff: Compare current worktree**.

The supplied task file provides three workflows:

- Compare the current Zed worktree against the remembered right directory.
- Compare the active file's directory against the remembered right directory.
- Clear both roots and choose both directories in Folder Diff.

Zed does not currently expose marked Project Panel directories as [task variables](https://zed.dev/docs/tasks#variables), so a task cannot receive two folders selected in the Project Panel. Zed exposes the active worktree and active file directory through `$ZED_WORKTREE_ROOT` and `$ZED_DIRNAME`; select the other root using Folder Diff's left/right directory controls.

You can bind the worktree task in Zed's `keymap.json`:

```json
[
  {
    "context": "Workspace",
    "bindings": {
      "alt-d": [
        "task::Spawn",
        { "task_name": "Folder Diff: Compare current worktree" }
      ]
    }
  }
]
```

When both versions of the selected file exist, **Open in Zed** runs:

```sh
zed --diff /absolute/left/file /absolute/right/file
```

Zed controls whether that external file diff is split or unified. To always use its documented [split diff view](https://zed.dev/docs/git#diff-view-styles), add this to Zed's `settings.json`:

```json
{
  "diff_view_style": "split"
}
```

Set `ZED_CLI_PATH` if `zed` is not on `PATH`.

## Merge safety model

Each write is a transaction:

1. Validate that the relative path remains inside the selected root.
2. Compare the source and destination SHA-256 revisions with the loaded preview.
3. Snapshot the destination in a private temporary directory.
4. Write through a same-directory temporary file and atomically persist it.
5. Roll back all touched paths if any step fails.
6. Keep the completed snapshot for **Undo**.

The merge engine refuses to overwrite directories and refuses to create a path through a symlinked parent. File permissions are preserved. Undo snapshots live only for the current app session.

## Development

```sh
cargo fmt --check
cargo test --lib --no-default-features
cargo check --all-targets
```

The core scan, diff, stale-write protection, synchronization behavior, and undo operations have unit tests. GitHub Actions runs the full check on macOS; see [`.github/workflows/ci.yml`](.github/workflows/ci.yml).

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the internal design and extension boundary.

## License

MIT
