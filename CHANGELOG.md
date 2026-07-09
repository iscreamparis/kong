# Changelog

All notable changes to KONG are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and KONG adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.6] — 2026-07-09

### Fixed
- **Never select a version KONG cannot install (artifact-aware resolution).**
  `resolve_best_version` chose the highest version satisfying the PEP 440
  specifier without checking whether that release ships an installable artifact;
  wheel-vs-sdist selection happened only afterwards. A release whose *only*
  artifact is a **compiled sdist** was therefore selected and then aborted in
  `sdist::install_sdist` (KONG ships no C toolchain), even when an older release
  in range published a perfectly good wheel.

  Seen in the wild with `pglast>=6,<8`: PyPI received 7.16's sdist at
  `2026-07-09T05:55:10Z` and its first wheel at `06:21:20Z`. Every environment
  built in that 26-minute window failed, with no manifest change anywhere.

  Resolution now prefers the highest satisfying version that publishes a wheel
  compatible with the target interpreter (reusing the existing `select_best_file_for`
  scorer), falling back to wheel-less releases only when no version in range ships
  one — the normal case for pure-Python sdist-only projects, which install fine.
  A compiled sdist still fails loudly rather than degrading silently.

  `list_versions` became `list_releases` (the PyPI JSON already carried the per-version
  file lists; they were being discarded), and `resolve_best_version` takes the target
  Python tag. New pure helper `installable_versions()` is host-independent and unit-tested.

## [0.8.5] — 2026-07-06

### Fixed
- **Directory mode normalisation on tar.gz extraction.** `extract_targz` now
  creates extracted directories via `std::fs::create_dir_all` (OS-default mode,
  `0o755` on Unix) instead of `Archive::unpack`, which preserved the
  tar-recorded mode verbatim.  Some npm tarballs (e.g. `pngjs@5.0.0`) ship
  directory entries with mode `0o666` — no execute/traverse bit.  With the old
  code those directories were non-traversable by their owner, so a subsequent
  `hard_link` into them failed with `Permission denied (os error 13)` even
  though the store owner was the same user.  The fix matches the behaviour of
  npm, pnpm, and yarn, which all normalise directory modes at extraction time.
  File modes are preserved as-is (`entry.unpack`) so executable bits on
  scripts and native binaries are not disturbed.  The fix applies uniformly to
  all tar.gz sources (Node, Python wheels, Rust crates) — no
  package-specific special-casing.  A regression test constructs an in-memory
  archive with a `0o666` directory entry and asserts the extracted directory
  has the owner execute bit set and its contents are reachable.

## [0.8.4] — 2026-06-23

### Fixed
- **Atomic `.venv` repoint on `kong use`.** The venv builder now builds the
  environment into a temporary sibling directory and swaps it over the live
  `.venv` in a single `rename` (atomic on Unix; move-aside + rename + rollback
  on Windows, sub-millisecond gap). `kong use --clean` keeps the existing
  `.venv` during the rebuild (`clean_environments` gained a `keep_venv`
  parameter) and garbage-collects the old environment only AFTER the swap. A
  live service whose `ExecStart` points inside `.venv` therefore never observes
  a missing or half-built environment during a rebuild — closing the 203/EXEC
  window that could crash-loop a running service. Activation scripts and
  console-script shebangs are baked with the FINAL `.venv` path, never the
  temporary build path. Console-script generation (added in 0.8.1) is unchanged.

## [0.8.3] — 2026-06-19

### Fixed
- **Per-project env keying.** Each project's environment is now keyed by the
  `project` field in its `kong.rules` (a path-unique slug; override with
  `kong rules --name <NAME>`) instead of the bare directory basename. This fixes
  same-basename subdirectories across different checkouts (e.g. multiple
  `crm-backend/` folders) colliding on a single shared environment — each folder
  now gets its own env off the one shared store. `kong use` honors
  `rules.project`, and project resolution is unified across `use` / `run` /
  `delete` / `solidify` / `eject` / `import`.

## [0.8.2] — 2026-06-19

### Added
- **Pure-Python sdist support.** The from-scratch resolve path
  (`kong rules` + `kong use`) now installs sdist-only packages (e.g.
  `sgmllib3k`, pulled in behind `feedparser`) with an importable layout and a
  synthesized `dist-info`. Sdists that require compilation fail loudly rather
  than installing silently broken.

## [0.8.1] — 2026-06-19

### Added
- **PEP 440 resolver.** The from-scratch resolve path (`kong rules` /
  `requirements.txt` → PyPI) now parses PEP 440 versions and specifier sets and
  selects the highest released version that satisfies the applicable
  constraints, propagating each parent's `Requires-Dist` bound to its
  dependencies. Non-exact specifiers (`>=`, ranges, `~=`), previously dropped or
  resolved to "latest", are now honored; an unsatisfiable bound is logged loudly
  and falls back to latest rather than aborting.
- **Console-script launchers.** The venv builder and `kong solidify` now scan
  every `*.dist-info/entry_points.txt` and write pip-style launchers for
  `[console_scripts]` / `[gui_scripts]` entries (e.g. `uvicorn`, `gunicorn`,
  `alembic`), driven entirely by `entry_points.txt` — no package names
  hardcoded. Fixes `ExecStart=.venv/bin/uvicorn` failing `203/EXEC`.

### Fixed
- **Wheel selection by target platform tag.** Wheels are now filtered by the
  target OS family and architecture (not just the arch suffix), so a Linux
  x86_64 target no longer wrongly accepts `macosx_*_x86_64` wheels. The target
  is derived from the KONG runtime's own platform tag.
- **Node platform-native dep filtering (npm semantics).** The node resolver now
  installs a platform-specific optional dependency only when its
  `os` / `cpu` / `libc` match the host — a host-incompatible optional dep is
  skipped silently, a required one errors (`EBADPLATFORM`). Stops fetching every
  platform variant (e.g. all seven `@rspack/binding` builds when only one is
  usable).
- **Standalone `solidify`.** `kong solidify` now copies the relocatable
  python-build-standalone runtime into the project's `.venv/runtime/` and
  repoints the interpreter at the local copy, so a solidified project keeps
  working after the global store is cleared.

## [0.8.0] — 2026-06-19

### Added
- **Linux release.** A static musl `Kong-<version>-linux-x64.tar.gz` binary is
  built and attached by CI (`.github/workflows/release-linux.yml`) on every
  `vX.Y.Z` tag, alongside the Windows installer and macOS DMG. The build is
  CLI-only (the Slint GUI is gated behind an optional Cargo feature), so no
  fontconfig / X11 / OpenGL system libraries are required.

### Fixed
- **Wheel selection by target CPython/ABI tag.** Picks the exact
  `cpXY` / `abi3` / `none` wheel for the managed interpreter and rejects a wrong
  CPython minor.
- **`kong import` copy-adopts installed packages.** Copies an existing `.venv` /
  `node_modules` into the store byte-for-byte (native extensions included), with
  no re-download or re-resolve.
- **Robust import copy.** Handles venv internals such as directory symlinks
  (`lib64 -> lib`) and dangling links.

[0.8.5]: https://github.com/iscreamparis/kong/releases/tag/v0.8.5
[0.8.4]: https://github.com/iscreamparis/kong/releases/tag/v0.8.4
[0.8.3]: https://github.com/iscreamparis/kong/releases/tag/v0.8.3
[0.8.2]: https://github.com/iscreamparis/kong/releases/tag/v0.8.2
[0.8.1]: https://github.com/iscreamparis/kong/releases/tag/v0.8.1
[0.8.0]: https://github.com/iscreamparis/kong/releases/tag/v0.8.0
