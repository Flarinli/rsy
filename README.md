# rsy

> [!WARNING]
> Under active development. `main` tracks the unfinished 0.5 rewrite.

[![CI](https://github.com/Flarinli/rsy/actions/workflows/ci.yml/badge.svg)](https://github.com/Flarinli/rsy/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Fast file synchronization with rsync's mental model, rebuilt in Rust around one
bounded, transactional sync engine for local and SSH transfers.

```bash
rsy /source/ /destination
```

**rsy is not a drop-in rsync replacement.** The command-line model is familiar,
but the wire protocol is its own. Use rsync when you need rsync-to-rsync
compatibility.

> rsy was previously named `sy`. The binary, the remote agent (`rsy __serve`),
> the config directory (`~/.config/rsy/`), ignore files (`.rsyignore`) and
> hook environment variables (`RSY_*`) all use the new name.

## Install

Build from source. You need Rust stable.

```bash
git clone https://github.com/Flarinli/rsy.git
cd rsy
cargo build --release            # binary at target/release/rsy
cargo build --release --features acl   # optional: ACL preservation (needs libacl on Linux)
```

For SSH sync, the same `rsy` binary must also be on the remote host (see
[SSH sync](#ssh-sync)). Static Linux builds have no runtime dependencies, which
makes them easy to copy to servers:

```bash
rustup target add x86_64-unknown-linux-musl     # or aarch64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

CI produces the same static artifacts. They leave out ACL support so that they
stay fully standalone.

## Quick start

### Local

```bash
rsy ~/photos /backup            # creates /backup/photos
rsy ~/photos/ /backup/photos    # trailing slash: copy the contents
rsy -n ~/photos/ /backup/photos # dry run: show what would change
rsy -a --delete ~/photos/ /backup/photos   # exact mirror, keep perms and mtimes
```

### Over SSH

```bash
rsy -a ~/photos/ nas:/srv/photos            # push: local → remote
rsy -a nas:/srv/photos/ ~/photos            # pull: remote → local
rsy -a ~/photos/ backup@nas:photos          # relative to the remote home
rsy -a ~/photos/ ssh://backup@nas:2222/srv/photos   # explicit port
```

## SSH sync

rsy syncs to and from any machine you can `ssh` into. You don't need a daemon
or any open ports beyond SSH. Your own OpenSSH client carries the traffic, and
the same `rsy` binary briefly runs on the other side as an agent.

**Full guide: [docs/ssh.md](docs/ssh.md)** covers setup, every address form,
recipes (keys, ports, bastions, cron, sudo), performance, the security model
and troubleshooting.

### Set up in three steps

```bash
# 1. Put rsy on the remote host (a static musl build has no dependencies)
scp target/x86_64-unknown-linux-musl/release/rsy nas:~/.local/bin/rsy

# 2. Check that a non-interactive SSH session finds it
ssh nas 'command -v rsy && rsy --version'

# 3. Sync
rsy -a ~/photos/ nas:/srv/photos
```

If step 2 prints nothing, pass the location explicitly with
`--rsy-path ~/.local/bin/rsy`.

### Remote addresses

| Form | Example |
|------|---------|
| `[user@]host:/abs/path` | `alice@nas:/srv/data` |
| `[user@]host:rel/path` (relative to the remote home; `~/` works too) | `nas:backups/2026` |
| `ssh://[user@]host[:port]/abs/path` | `ssh://alice@nas:2222/srv/data` |
| IPv6 | `root@[2001:db8::7]:/srv`, `ssh://[::1]:2222/srv` |

`host` can be any alias from `~/.ssh/config`, which keeps working unchanged:
`Include`, `Match`, `ProxyJump`, `ControlMaster`, agents and FIDO keys are all
handled by OpenSSH.

### SSH options

| Flag | Meaning |
|------|---------|
| `-e, --rsh "CMD"` | SSH program and arguments, like rsync (`-e "ssh -p 2222"`); also read from `RSY_RSH` |
| `--ssh-port N` | Port (`ssh -p`) |
| `--ssh-key FILE` | Identity file (`ssh -i`), repeatable |
| `--ssh-option K=V` | Any OpenSSH option (`ssh -o`), repeatable |
| `--jump HOST` | Jump host / bastion (`ssh -J`) |
| `--rsy-path CMD` | Remote command that starts rsy (default `rsy`, e.g. `/opt/rsy/bin/rsy` or `"sudo -n rsy"`) |
| `--contimeout N` | Give up connecting after N seconds |
| `--timeout N` | Abort if no data arrives for N seconds |
| `-z`, `--bwlimit RATE` | zstd compression (auto-sampled), bandwidth cap |

```bash
# Key + bastion + keepalive, failing fast instead of prompting (cron-friendly)
rsy -a --delete \
    --ssh-key ~/.ssh/backup_ed25519 --jump admin@bastion \
    --ssh-option BatchMode=yes --ssh-option ServerAliveInterval=15 \
    ~/data/ app01.internal:/srv/data
```

### What you get

- **Safe by construction.** Remote paths travel inside the binary protocol and
  never reach a shell. `--` and `-T` are always passed to `ssh`. The remote
  agent can't be tricked into following symlinks out of the target directory.
- **Verified and atomic.** Each file is BLAKE3-checked in staging and then
  renamed into place. An interrupted sync leaves the old files intact.
- **Efficient.** Large changed files (≥ 10 MiB) send only changed blocks, in
  both directions. Many files share one SSH connection. Compression is optional.
- **Clear failures.** A missing remote `rsy` (exit 127), a refused login
  (exit 255), a missing `ssh` client or a malformed address each produce a
  specific error message telling you what to fix.

Both machines must run the same `rsy` version (protocol v3.13). Pulls
(remote → local) don't support `--remove-source-files` or `--copy-links` yet.
Remote → remote is not supported.

## How it works

```text
source scan ─┐
             ├─> reconcile ─> plan ─> schedule ─> stage ─> verify ─> atomic commit
  dest scan ─┘
```

- **One engine.** Local→local, local→SSH and SSH→local all share the same
  ordered merge reconciler, planner, scheduler and safety rules.
- **Staged, atomic replacement.** New bytes are written to private staging on
  the destination filesystem. Metadata is applied and the result is verified
  before an atomic rename or exchange. If anything fails before commit, the old
  destination is left intact.
- **Bounded resources.** Every queue is bounded. Scans, deletion journals and
  namespace checks spill to authenticated scratch files instead of holding whole
  trees in RAM.
- **Race-aware.** Source files are checked against their scanned identity
  before, during and after transfer. A file that changes mid-copy fails the
  transfer; it is never committed.

### Transfer strategies

| Situation | Strategy |
|-----------|----------|
| New local file | Native OS copy into staging |
| Changed large file on Btrfs/XFS (Linux) | Reflink old file, patch changed ranges |
| Changed file on non-COW filesystem | Native whole-file copy |
| Sparse file | Extent-aware copy preserving holes |
| SSH, changed file ≥ 10 MiB | Rolling delta, in both push and pull |
| SSH, new or small file | Whole-file stream, optional zstd |

## Common flags

| Flag | Description |
|------|-------------|
| `-n, --dry-run` | Preview changes without applying |
| `-a, --archive` | `-rlpt`: recurse, links, permissions, times (no owner/group/devices) |
| `-t` | Preserve modification times (recommended for incremental runs) |
| `-d, --delete` | Delete destination files not present in the source |
| `--max-delete N\|P%` / `--force-delete` | Deletion safety threshold (default `50%`) / override |
| `-H` | Preserve hard links |
| `-X` / `-A` / `-F` | Preserve xattrs / ACLs (`acl` feature) / BSD flags (macOS) |
| `--links=preserve\|follow\|skip` | Symlink handling |
| `--exclude`, `--include`, `--exclude-from`, `--filter` | rsync-style filters |
| `-c, --checksum` | Compare by BLAKE3 content instead of size+mtime |
| `--verify=after\|only` | Verify staged content locally / audit without changes |
| `--backup [--backup-dir D] [--suffix S]` | Keep replaced and deleted files |
| `--remove-source-files` | Move instead of copy (local and push only) |
| `-j, --parallel N` | Concurrent transfers (default 10) |
| `-z, --compress` | zstd compression, auto-sampled |
| `-i, --itemize-changes` | Per-entry change report |
| `--json` | Machine-readable output |

Run `rsy --help` or `man ./man/rsy.1` for the full list.

Without `-t` or `-a`, rsy does not apply source mtimes. The default quick check
compares size and mtime, so a later run may copy those files again.

## Safety guarantees

rsy mutates your data, and losing data is treated as the most serious kind of
bug.

- **Deletion is a commit phase.** Deletes run only after both scans finish
  successfully and the `--max-delete` threshold has passed. Any source scan
  error disables deletion. Excluded destination entries protect their parent
  directories.
- **Source is read-only.** `--remove-source-files` unlinks a source file only
  after the destination commit, with **verified content parity** and successful
  requested preservation. Unverified or quick-equal copies keep their source.
- **Type transitions are refused when they can't be made atomic.** A file or
  symlink can replace an empty directory using atomic exchange. Nonempty
  directory replacements are refused in preflight, before any mutation.
- **Honest limits.** These checks detect races; they do not take snapshots. rsy
  does not claim atomic compare-and-swap against concurrent writers, power-loss
  durability, or whole-run rollback. A failure after commit is reported as a
  committed failure, never as a rollback.

## Configuration

`~/.config/rsy/config.toml` sets defaults:

```toml
max_concurrent = 8
compress = "auto"
exclude = [".git", "node_modules", "*.pyc"]
```

You can put a `.rsyignore` file in a source root. Reusable exclude templates go
in `~/.config/rsy/templates/<name>.rsyignore`, and there are samples in
[`templates/`](templates/).

## Platform status

| Platform | Status |
|----------|--------|
| Linux | Supported, tested in CI |
| macOS | Supported, tested in CI (whole-file copy instead of reflink patching) |
| Windows | Compiles only; synchronization not supported yet |

Out of scope for 0.5: bidirectional sync, S3/GCS endpoints, automatic remote
agent install.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check
cargo run --bin man-gen        # regenerate man/rsy.1
```

Architecture and invariants: [docs/architecture-0.5.md](docs/architecture-0.5.md).
Contributor guide: [CONTRIBUTING.md](CONTRIBUTING.md).
Release history: [CHANGELOG.md](CHANGELOG.md).

Benchmarks (harness check, not a performance study):

```bash
cargo build --release
python3 scripts/benchmark.py --quick --rsy-binary target/release/rsy
cargo bench
```

## License

MIT
