# Dependency boundary

Folder Diff intentionally has one declared crate dependency:

```toml
[dependencies]
gpui = { version = "0.2.2", optional = true }
```

The `app` default feature enables GPUI. Building with `--no-default-features` compiles the comparison and merge library without any third-party crate.

## Why GPUI remains

GPUI supplies the native window, renderer, text system, input handling, scrolling, and macOS folder prompt. Replacing it would mean replacing the UI architecture, which is outside this dependency reduction. GPUI 0.2.2 does not expose feature flags that remove most of its graphics, media, HTTP, or text stack, so its transitive graph cannot be substantially trimmed from this crate.

For the checked-in lockfile and `aarch64-apple-darwin` target, `cargo tree` resolves 443 packages for the default application and one package for the no-default-feature core. These counts include Folder Diff itself and may change after dependency updates.

## Internal replacements

- Recursive traversal uses `std::fs::read_dir`, never follows symbolic links, and scans both roots concurrently.
- File comparisons use scoped standard threads selected from `available_parallelism`.
- Ignore patterns support literal component names and the `*`, `?`, and `**` wildcards.
- Revision checks use a local SHA-256 implementation tested against the standard `abc` vector.
- Text comparison uses a local Myers shortest-edit implementation.
- Atomic writes and undo snapshots use exclusive-create temporary files/directories and same-directory rename.
- Configuration uses a small versioned line format with hex-encoded paths, avoiding a parser dependency and preserving non-UTF-8 Unix paths.
- Command-line parsing supports `--choose`, `--left`, `--right`, `--help`, and `--version` directly through `std::env`.

## Licensing

Folder Diff's source is MIT-licensed. GPUI 0.2.2 is Apache-2.0, which is compatible with distributing an MIT application provided the Apache license and applicable notices are preserved when distributing binaries. GPUI's transitive crates retain their own licenses; use the lockfile as the input to a release-time license inventory.
