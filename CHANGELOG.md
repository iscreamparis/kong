# Changelog

All notable changes to KONG are documented here. KONG follows semantic-ish
versioning; the version in `Cargo.toml` is the single source of truth.

## 0.8.4

### Fixed — `kong use` no longer strands a live service during a rebuild

Two defects let `kong use` (especially `--clean`) take down a running service
whose `ExecStart` points inside the project `.venv` (e.g.
`.venv/bin/uvicorn`). Both are now fixed, generally, with no app-specific names
or paths.

- **Console-script launchers are materialized.** pip/`venv` write a launcher in
  `bin/` (`Scripts\` on Windows) for every `[console_scripts]` (and
  `gui_scripts`) entry point an installed distribution declares — `uvicorn`,
  `gunicorn`, `alembic`, `pip`, … KONG previously placed only `bin/python`, so
  those launchers were absent and `ExecStart=.../.venv/bin/uvicorn` failed with
  `status=203/EXEC` until a consumer hand-made a shim. KONG now scans every
  `*.dist-info/entry_points.txt` in site-packages and generates a pip-style
  launcher per entry (Unix: `#!<venv-python>` shebang importing and calling the
  declared callable, `chmod 0755`; Windows: a `<name>-script.py` + `<name>.bat`
  pair). Discovery is driven entirely by installed metadata — no package names
  are hardcoded. Wired into both `kong use` (`build_venv`) and `kong solidify`
  (`solidify_python`).

- **The `.venv` is repointed atomically.** `kong use --clean` used to remove
  `.venv` and then rebuild it in place, leaving a window in which `.venv` was
  missing or half-built; a service that restart-looped during that window
  crashed `203/EXEC` (this took down a production control plane). KONG now
  builds the fresh environment in a temporary sibling directory and swaps it
  over the live `.venv` in a single `rename`. On Unix the `rename` atomically
  replaces the existing directory — observers see either the old or the new
  env, never a gap. On Windows (no atomic directory-replace primitive) the old
  env is moved aside and the new one renamed in, shrinking the absent-window to
  the sub-millisecond gap between two metadata renames (with rollback on
  failure). `--clean` now GCs the OLD env only AFTER the swap, never before.

Tests: console-script materialization after `use` (launcher present,
executable, correct shebang/callable); a rebuild over an existing `.venv` stays
complete throughout and the swap replaces the whole env atomically; the swap
leaves no `.venv.kong-*` temp/old leftovers. `cargo test` green.
