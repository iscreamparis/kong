# CLAUDE.md — kong

Guidance for Claude Code in the **kong** repo (this is the in-house Rust dependency
manager for Python/Node/Rust, `Q:\kong`; consumed by the CRM master which serves the
Linux binary to tenant VMs). See `agents.md` / `AGENTS.md` for the full design.

## Building the Linux binary — use WSL (local)

The distributable Linux binary is a **static musl** build. CI (`.github/workflows/release-linux.yml`,
triggered by a `vX.Y.Z` tag or `workflow_dispatch`) produces it, **but for local
builds/verification use WSL** — this is the established path.

WSL gotchas (all real, hit them once):
- WSL default user is **root**; cargo is `/root/.cargo/bin/cargo` (a rustup shim), **not on the non-login PATH**.
- Use `bash -c`, **not** `bash -lc` — the login shell errors with "Failed to start systemd user session for root" and mangles output.
- WSL inherits the **Windows PATH** via interop, so `export PATH=/root/.cargo/bin:$PATH` breaks on the spaces in "Program Files" (`not a valid identifier`). **Set a clean Linux PATH instead.**

Working invocation (headless, static musl — matches CI):

```bash
wsl bash -c 'export PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin; \
  cd /mnt/q/kong && cargo build --release --no-default-features --target x86_64-unknown-linux-musl'
# -> target/x86_64-unknown-linux-musl/release/kong   (static-pie ELF, runs on any Linux)
```

`--no-default-features` drops the Slint GUI (fontconfig/X11/OpenGL) for a headless CLI build.
`musl-gcc` is at `/usr/bin/musl-gcc`. Building on `/mnt/q` (Windows drive via 9p) is slow but is the established flow.

Run `cargo test` (also in WSL) for the Unix-specific tests — some assertions are `#[cfg(unix)]`
and are compiled out on a Windows `cargo test`, so they only really run on Linux.

## Release flow (see RELEASING.md)

`release-X.Y.Z` branch + `vX.Y.Z` tag. Pushing the tag triggers the Linux CI musl build and
attaches `Kong-<ver>-linux-x64.tar.gz` to the GitHub release. CI **only builds** (no `cargo test`).

## How the CRM master serves kong

The CRM master serves the Linux binary at **`/home/iscream/kong-linux-x64`** (app_settings
`kong_binary_path`); tenant VMs curl it via a signed URL and drop it at `/usr/local/bin/kong`.
To ship a new kong: build the Linux binary (WSL or CI) → replace that path on the master →
verify `GET /api/master/kong/info`. A tenant **deploy** self-syncs its kong to whatever the
master serves, so the master's binary must be current before deploying to remote tenants.
