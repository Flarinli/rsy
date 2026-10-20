# SSH sync guide

`rsy` syncs between a local machine and any host you can reach with OpenSSH,
in both directions:

```bash
rsy ~/photos/ nas:/srv/photos          # push: local → remote
rsy nas:/srv/photos/ ~/photos          # pull: remote → local
```

It uses your normal `ssh` client and configuration, needs no daemon and opens
no extra ports. On the remote side the same `rsy` binary runs as a short-lived
agent for the duration of the sync.

- [How it works](#how-it-works)
- [Setting up the remote host](#setting-up-the-remote-host)
- [Remote addresses](#remote-addresses)
- [Push and pull examples](#push-and-pull-examples)
- [Connection options](#connection-options)
- [Recipes](#recipes)
- [Performance](#performance)
- [Safety and security model](#safety-and-security-model)
- [Limitations](#limitations)
- [Troubleshooting](#troubleshooting)

---

## How it works

```text
 local machine                                           remote host
┌───────────────────────┐   your OpenSSH client     ┌──────────────────────┐
│ rsy  (client)         │  ssh -T … -- host         │ rsy __serve (agent)  │
│  scan · reconcile ·   │◄════ one encrypted ══════►│  held root directory │
│  plan · stage ·       │      byte stream          │  scan · read · stage │
│  verify · commit      │   (rsy protocol v3.13)    │  verify · commit     │
└───────────────────────┘                           └──────────────────────┘
```

1. `rsy` starts `ssh -T [your options] -- <host> rsy __serve`.
2. The client and agent exchange a binary handshake: protocol version,
   operation (push or pull), the remote root path and capabilities. The remote
   path travels **inside the protocol**, never on the SSH command line.
3. Both sides stream cheap, ordered metadata (names, sizes, mtimes). The
   client merges the two listings with the same reconciler that local syncs use.
4. Changed and new files are transferred over the single connection, many at a
   time with independent stream IDs. Large changed files (≥ 10 MiB) send only
   the changed blocks (rolling delta). This works in both directions.
5. Every file is written to private staging on the receiving side, verified
   against the sender's **BLAKE3** digest, and only then atomically renamed
   into place. A failed or interrupted transfer leaves the old file intact.
6. `--delete` runs only after both scans completed successfully and the
   deletion threshold passed.

---

## Setting up the remote host

The remote host needs:

- an SSH server you can log in to, and
- the **same version** of `rsy` on that user's `PATH` for non-interactive
  sessions (or anywhere, if you use `--rsy-path`).

Both sides must speak exactly the same protocol version (currently **v3.13**).
Upgrade client and remote together.

### Install the agent

Static Linux builds have no runtime dependencies, so you can simply copy one:

```bash
# Build a static binary (or download the CI artifact for your architecture)
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl

# Copy it to the remote host
scp target/x86_64-unknown-linux-musl/release/rsy nas:~/.local/bin/rsy
ssh nas chmod +x ~/.local/bin/rsy
```

Use `aarch64-unknown-linux-musl` for 64-bit ARM hosts (Raspberry Pi 4/5,
Graviton, Ampere). On macOS, build normally with `cargo build --release`.

### Check that the agent is found

`ssh host command` runs a **non-interactive** shell, which often does not read
the files that extend `PATH` in an interactive login (for example
`~/.bash_profile`). Test it exactly the way `rsy` will:

```bash
ssh nas 'command -v rsy && rsy --version'
```

If nothing is printed, either add the directory to `PATH` in a file your
remote shell reads for non-interactive commands, or tell `rsy` where it is:

```bash
rsy --rsy-path ~/.local/bin/rsy ~/photos/ nas:/srv/photos
```

> `rsy` does not install itself on remote hosts. Copying a static binary once
> is all that is needed.

---

## Remote addresses

| Form | Example | Notes |
|------|---------|-------|
| `host:path` | `nas:/srv/data` | `host` can be an `~/.ssh/config` alias |
| `user@host:path` | `alice@nas:/srv/data` | Overrides `User` from ssh_config |
| `host:relative` | `nas:backups/2026` | Relative to the remote **home directory** |
| `host:~/path` | `nas:~/backups` | Same as `nas:backups` |
| `host:` / `host:~` | `nas:` | The remote home directory itself |
| `user@[ipv6]:path` | `root@[2001:db8::7]:/srv` | Brackets are required for IPv6 |
| `ssh://[user@]host[:port]/path` | `ssh://alice@nas:2222/srv/data` | The only form that can carry a port; path is absolute |
| `ssh://[ipv6]:port/path` | `ssh://[::1]:2222/srv` | |

Things to know:

- **Trailing slash works like rsync.** `rsy src nas:/dst` creates `/dst/src`.
  `rsy src/ nas:/dst` copies the *contents* of `src` into `/dst`. The same
  applies to remote sources when pulling.
- **No remote shell expansion.** Paths are protocol data, so `*`, `$VAR` and
  quotes are taken literally. `~` and `~/…` at the start of an scp-style path
  are the only shorthands, and they mean "the home directory sshd starts in".
  `~otheruser/…` is refused.
- **Paths can contain any bytes.** Non-UTF-8 Unix filenames are preserved
  exactly, including in the remote root path. Host and user names must be
  UTF-8.
- **Single-letter hosts are remote.** On Linux and macOS, `a:/backup` means
  host `a`. Only on Windows is `C:/…` treated as a drive letter.
- **Mistakes are errors, not local paths.** A malformed remote spec such as
  `ssh://host` (no path), `ssh://host:0/x`, `user@:/x` or a host starting with
  `-` is rejected before anything runs. It is never reinterpreted as a local
  directory.
- To sync a **local** path that contains a colon before its first slash, write
  it as `./name:with:colons`.

---

## Push and pull examples

```bash
# Preview what would change
rsy -n ~/site/ web:/var/www/site

# Mirror: also delete remote files that no longer exist locally
rsy --delete ~/site/ web:/var/www/site

# Archive mode (recursive, symlinks, permissions, mtimes) plus hard links and xattrs
rsy -a -H -X ~/projects/ nas:/srv/projects

# Pull a remote directory into the current directory
rsy nas:/srv/media/movies .

# Compress over a slow link and cap bandwidth at 2 MB/s
rsy -z --bwlimit 2MB ~/data/ remote:/data

# Itemized changes, machine-readable output
rsy -i ~/docs/ nas:docs
rsy --json ~/docs/ nas:docs

# Move files to the server (sources removed only after verified commit)
rsy --remove-source-files ~/outbox/ nas:/srv/inbox
```

Use `-t` or `-a` for repeated syncs. Without preserved mtimes, the default
size+mtime comparison sees every file as changed on the next run. Use
`-c/--checksum` to compare by content instead.

---

## Connection options

### Prefer `~/.ssh/config`

Everything OpenSSH supports keeps working, because `rsy` hands the host alias to
`ssh` unchanged. That includes `Include`, `Match`, `ProxyJump`,
`ControlMaster`, agents, FIDO keys and certificates. A config entry keeps
commands short and applies to `ssh`, `scp`, `git` and `rsy` alike:

```sshconfig
# ~/.ssh/config
Host nas
    HostName 192.168.1.20
    User backup
    Port 2222
    IdentityFile ~/.ssh/nas_ed25519
    IdentitiesOnly yes
    ServerAliveInterval 15
```

```bash
rsy ~/photos/ nas:/srv/photos
```

### Command-line flags

When you can't or don't want to edit ssh_config, these flags pass explicit
settings to OpenSSH:

| Flag | Passed to ssh as | Example |
|------|------------------|---------|
| `-e, --rsh CMD` | the program + leading arguments | `-e "ssh -F /etc/ssh/work_config"` |
| `--ssh-port N` | `-p N` | `--ssh-port 2222` |
| `--ssh-key FILE` (repeatable) | `-i FILE` | `--ssh-key ~/.ssh/backup_ed25519` |
| `--ssh-option K=V` (repeatable) | `-o K=V` | `--ssh-option ServerAliveInterval=15` |
| `--jump HOSTS` | `-J HOSTS` | `--jump bastion` or `--jump me@gw:2222,inner` |
| `--contimeout N` | `-o ConnectTimeout=N` | `--contimeout 10` |
| `--rsy-path CMD` | remote command instead of `rsy` | `--rsy-path /opt/rsy/bin/rsy` |
| `--timeout N` | *(rsy itself)* abort if no data arrives for N seconds | `--timeout 60` |
| `RSY_RSH` env var | default for `-e` | `export RSY_RSH="ssh -o BatchMode=yes"` |

Details:

- **`-e/--rsh`** is split into words like a shell would split it, but **no
  shell runs**. Single quotes, double quotes and backslashes work. Variables,
  globs and `~` are not expanded by `rsy` (OpenSSH expands `~` in `-i` paths
  itself). Use absolute paths elsewhere. The first word is the program, so you
  can point at a
  different `ssh` binary or wrapper. `-e` overrides `RSY_RSH`.
- **Port.** A port in an `ssh://host:PORT/` operand and `--ssh-port` must agree
  if you give both. A port set inside `-e` is overridden by either of them.
- **Order.** `rsy` builds the command as
  `<-e words> -T [-p] [-l user] [-i …] [-J …] [-o ConnectTimeout] [-o …] -- <host> <rsy-path> __serve`.
  For `-o` settings OpenSSH keeps the *first* value it sees, so a `-o` inside
  `-e` wins over `--ssh-option` and `--contimeout`. Set each setting in one
  place to avoid surprises.
- **`--rsy-path`** is run by the remote user's login shell, like rsync's
  `--rsync-path`. That is what makes `--rsy-path "sudo -n rsy"` possible. It is
  the only text `rsy` ever sends to the remote shell, and it is the value you
  typed. Your sync paths are never included in it.
- These flags are validated before `ssh` starts: unbalanced quotes, empty
  values, option-shaped `--ssh-option`/`--jump` values and conflicting ports
  are rejected.

---

## Recipes

### Non-standard port

```bash
rsy ~/data/ ssh://backup@nas.example.com:2222/srv/data
# or
rsy --ssh-port 2222 ~/data/ backup@nas.example.com:/srv/data
```

### A dedicated key

```bash
rsy --ssh-key ~/.ssh/backup_ed25519 --ssh-option IdentitiesOnly=yes \
    ~/data/ nas:/srv/data
```

`IdentitiesOnly=yes` stops the agent from offering every key it holds first,
which matters on servers with a low `MaxAuthTries`.

### Through a bastion / jump host

```bash
rsy --jump admin@bastion.example.com ~/deploy/ app01.internal:/srv/app
```

### Unattended runs (cron, systemd timers, CI)

A password or host-key prompt would block forever with nobody to answer it.
Fail fast instead:

```bash
rsy --ssh-option BatchMode=yes \
    --ssh-option StrictHostKeyChecking=accept-new \
    --contimeout 15 --timeout 120 \
    -a --delete ~/data/ backup@nas:/srv/data
```

- `BatchMode=yes` turns any prompt into an immediate error (exit 255).
- `StrictHostKeyChecking=accept-new` records the key on first contact but
  still refuses a *changed* key.
- `--timeout` aborts a stalled transfer, and `--contimeout` aborts a stalled
  connection attempt.

### Reusing one connection for many syncs

```sshconfig
Host nas
    ControlMaster auto
    ControlPath ~/.ssh/cm-%C
    ControlPersist 10m
```

Later `rsy`, `ssh` and `scp` runs to `nas` skip the TCP and authentication
handshake.

### Writing files owned by another user

```bash
rsy --rsy-path "sudo -n /usr/local/bin/rsy" ~/site/ web:/var/www/site
```

`-n` makes `sudo` fail instead of prompting, because the protocol stream has no
terminal. Allow exactly that command in sudoers on the remote host, for example
`deploy ALL=(root) NOPASSWD: /usr/local/bin/rsy __serve`.

### IPv6 and link-local addresses

```bash
rsy ~/data/ 'root@[2001:db8::7]:/srv/data'
rsy ~/data/ 'ssh://[fe80::1%eth0]:22/srv/data'
```

Quote these in your shell: `[` … `]` are glob characters.

### Debugging the connection

```bash
rsy -e "ssh -v" ~/data/ nas:/srv/data      # OpenSSH debug output on stderr
```

---

## Performance

- **Delta transfers.** Changed regular files of at least 10 MiB send only the
  changed blocks. The block size adapts to the file size (4 KiB–1 MiB), and both
  push and pull support it. New and small files are sent whole, which is faster
  for them.
- **Compression.** `-z` (`--compress=auto`) samples each file and compresses
  with zstd only when that pays off. `--compress=always` forces it and
  `--compress=never` (the default) disables it. Leave OpenSSH's own
  `Compression` off when using `-z`, otherwise the data is compressed twice.
- **Parallelism.** `-j N` (default 10) controls how many files are in flight.
  They share one SSH connection, so the parallelism doesn't add SSH sessions.
- **Bandwidth.** `--bwlimit 5MB` paces file data. Control traffic is not
  delayed behind it.
- **Unchanged trees.** A sync with nothing to do exchanges only metadata.
- **Cipher choice.** On slow CPUs, `--ssh-option Ciphers=aes128-gcm@openssh.com`
  (hardware AES) or `chacha20-poly1305@openssh.com` (no AES-NI) can make a
  noticeable difference.

---

## Safety and security model

| Guarantee | How |
|-----------|-----|
| Your sync paths never reach a remote shell | The remote root and every path are binary protocol data. The SSH command line contains only your explicit flags, the host, and `rsy __serve` (or `--rsy-path`). |
| A host spec can't inject `ssh` options | `--` precedes the host, and hosts/users starting with `-` are rejected. |
| The protocol stream is never a terminal | `-T` is always passed, even if ssh_config says `RequestTTY force`. |
| Remote paths can't escape the chosen root | The agent opens the root once and walks every component through held directory handles with `O_NOFOLLOW`. A symlink planted inside the tree can't redirect writes outside it. |
| Wrong bytes are never committed | Every received file is BLAKE3-verified in staging before an atomic rename. A mismatch aborts that file and keeps the old copy. |
| Interrupted syncs don't leave half-written files | Writes go to private staging. Until the rename, the old destination is untouched. |
| Incomplete scans can't delete data | Deletes run last, only after both scans succeed and `--max-delete` (default 50%) passes. |
| `--remove-source-files` is cautious | A source file is unlinked only after its destination commit is verified and the source is unchanged since it was scanned. |
| Untrusted peer input is bounded | Frames are capped at 1 MiB, lengths are checked before allocation, and malformed messages end the session with an error. |

The remote agent runs with the permissions of the SSH user (or whatever
`--rsy-path` grants), so it can only touch what that user can.

---

## Limitations

- **Same version on both sides.** The client and agent must speak the same
  protocol version. A mismatch fails during the handshake, before anything is
  read or written.
- **Pull restrictions.** `--remove-source-files` and `--copy-links` /
  `--links=follow` are refused for remote → local syncs.
- **No remote → remote.** One side must be local.
- **No automatic agent installation.** Copy `rsy` to the remote host once.
- **Windows** is not supported as either side.
- **Directory type changes** (e.g. a file replacing a non-empty directory) are
  refused before any change is made.

---

## Troubleshooting

| Message | Cause | Fix |
|---------|-------|-----|
| `the remote host could not find "rsy" (exit 127)` | `rsy` is not on the remote `PATH` for non-interactive shells | Install it, or pass `--rsy-path /full/path/to/rsy`. Check with `ssh host 'command -v rsy'`. |
| `the remote host found "rsy" but could not run it (exit 126)` | Not executable, or built for another OS/CPU | `chmod +x`, and use the musl build matching the remote architecture (`uname -m`). |
| `ssh could not connect or authenticate (exit 255)` | Network, DNS, host key or login failure | Read the `ssh:` line printed just above. Run `ssh -v host true` or `rsy -e "ssh -v" …`. |
| `SSH client "ssh" was not found` | No OpenSSH client locally | Install OpenSSH, or point `-e`/`RSY_RSH` at your client. |
| `no compatible protocol version (client …, server …)` | Different `rsy` versions | Install the same version on both machines. |
| `Host key verification failed.` | Unknown or changed host key | Connect once with `ssh host` to verify it, or use `StrictHostKeyChecking=accept-new` for first contact. |
| The sync seems to hang at the start | ssh is waiting for a password or passphrase with no terminal | Use keys or an agent. Add `--ssh-option BatchMode=yes` to get an error instead of a hang. |
| `conflicting SSH ports: …` | `ssh://host:PORT/` and `--ssh-port` disagree | Give the port once. |
| `'~user' is not supported in remote paths` | `host:~bob/x` | Use an absolute path such as `host:/home/bob/x`. |
| `ssh:// URL needs an absolute path after the host` | `ssh://host` or `ssh://host:22` | Add the path: `ssh://host/srv/data`, or use `host:` for the home directory. |
| Files are copied again on every run | Modification times are not preserved | Add `-t` or `-a`, or compare by content with `-c`. |

For a timeout during a transfer, `--timeout N` turns silence into a clean
error. Already committed files stay committed, and the next run continues
from where the trees differ.
