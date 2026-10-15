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

For SSH sync, the same `rsy` binary must also be on the remote host's `PATH`.
Static Linux musl builds (`x86_64`/`aarch64-unknown-linux-musl`) are produced
by CI and work well as drop-in remote agents. They leave out ACL support so
that they stay fully standalone.

## Quick start

```bash
rsy /source /destination                 # copy the directory itself
rsy /source/ /destination                # copy its contents (trailing slash)
rsy /source /destination --dry-run       # preview
rsy /source /destination --delete        # mirror: delete extras in destination
rsy -a /source /destination              # archive: recurse, links, perms, times

rsy /local user@host:/remote             # SSH push
rsy user@host:/remote /local             # SSH pull
```

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

## SSH sync

rsy uses **your own OpenSSH client** as the transport. It runs:

```text
ssh -T [-l user] [-o ConnectTimeout=N] -- <host> rsy __serve
```

- Your `~/.ssh/config` applies unchanged. That covers `Host` aliases, `Include`,
  `Match`, `ProxyJump`, `ControlMaster`, agents and security keys. rsy never
  parses or rewrites it.
- Remote paths are **never** placed on the SSH command line. The remote root is
  sent inside the binary protocol handshake, so shell quoting and injection
  through paths are not possible.
- `-T` keeps the binary stream clean even if your config sets `RequestTTY`.
  `--` ensures a host alias can never be interpreted as an OpenSSH option.
- The remote agent works relative to a held root directory handle. It walks
  every path component with `O_NOFOLLOW` directory descriptors, so pre-existing
  symlinks cannot escape the root.
- Every remote file is **BLAKE3-verified** after reconstruction in staging and
  before it is committed. This is the default, not an opt-in.
- Many files are multiplexed over one SSH connection using protocol v3 frames,
  which are bounded at 1 MiB. Both peers must run the same exact protocol
  version, currently **v3.13**.

Useful flags: `--timeout N` (I/O idle timeout), `--contimeout N` (SSH connect
timeout), `--bwlimit RATE`, `-z/--compress`.

Current limitations for **pull** (remote → local): `--remove-source-files` and
`--copy-links` are refused before any transfer starts.

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
