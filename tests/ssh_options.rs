//! SSH launch options through the real CLI, a fake OpenSSH client, and a real
//! `rsy __serve` peer: `-e/--rsh`, `RSY_RSH`, `--ssh-*`, `--jump`,
//! `--rsy-path`, `ssh://` operands and launch-failure diagnostics.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(fixture.path("src/nested")).unwrap();
        std::fs::write(fixture.path("src/top.txt"), b"top").unwrap();
        std::fs::write(fixture.path("src/nested/deep.txt"), b"deep").unwrap();
        // Records argv one word per line, then runs the real agent. The
        // remote command words are ignored: only the transport is faked.
        fixture.script(
            "fake-ssh",
            "printf '%s\\n' \"$@\" > \"$RSY_TEST_ARGV\"\nexec \"$RSY_TEST_AGENT\" __serve\n",
        );
        fixture
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    async fn rsy(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rsy"));
        command
            .args(args)
            .args(["--quiet", "--no-hooks"])
            // Relative local operands and any misparsed spec land here.
            .current_dir(self.root.path())
            .env("RSY_TEST_AGENT", env!("CARGO_BIN_EXE_rsy"))
            .env("RSY_TEST_ARGV", self.path("argv"))
            .env_remove("RSY_RSH")
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .expect("rsy stalled")
            .unwrap()
    }

    fn argv(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("argv"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn fake_ssh(&self) -> String {
        self.path("fake-ssh").to_str().unwrap().to_owned()
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "rsy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_tree_copied(destination: &Path) {
    assert_eq!(std::fs::read(destination.join("top.txt")).unwrap(), b"top");
    assert_eq!(
        std::fs::read(destination.join("nested/deep.txt")).unwrap(),
        b"deep"
    );
}

#[tokio::test]
async fn push_passes_every_explicit_ssh_setting_in_order() {
    let fixture = Fixture::new();
    let rsh = format!("{} -F '/etc/ssh/my config'", fixture.fake_ssh());
    let source = format!("{}/", fixture.path("src").display());
    let destination = format!("ssh://alice@[::1]:2222{}", fixture.path("dst").display());
    let output = fixture
        .rsy(
            &[
                "-e",
                &rsh,
                "--ssh-key",
                "/keys/backup",
                "--ssh-option",
                "ServerAliveInterval=15",
                "--jump",
                "gw.example.com",
                "--contimeout",
                "7",
                "--rsy-path",
                "/opt/rsy/bin/rsy",
                &source,
                &destination,
            ],
            &[],
        )
        .await;
    assert_success(&output);
    assert_tree_copied(&fixture.path("dst"));
    assert_eq!(
        fixture.argv(),
        [
            "-F",
            "/etc/ssh/my config",
            "-T",
            "-p",
            "2222",
            "-l",
            "alice",
            "-i",
            "/keys/backup",
            "-J",
            "gw.example.com",
            "-o",
            "ConnectTimeout=7",
            "-o",
            "ServerAliveInterval=15",
            "--",
            "::1",
            "/opt/rsy/bin/rsy",
            "__serve",
        ]
    );
}

#[tokio::test]
async fn pull_uses_rsy_rsh_and_url_port() {
    let fixture = Fixture::new();
    let source = format!("ssh://backup-host:2200{}/", fixture.path("src").display());
    let destination = fixture.path("pulled");
    let output = fixture
        .rsy(
            &[&source, destination.to_str().unwrap()],
            &[("RSY_RSH", &fixture.fake_ssh())],
        )
        .await;
    assert_success(&output);
    assert_tree_copied(&destination);
    assert_eq!(
        fixture.argv(),
        ["-T", "-p", "2200", "--", "backup-host", "rsy", "__serve"]
    );
}

#[tokio::test]
async fn explicit_rsh_overrides_the_environment() {
    let fixture = Fixture::new();
    let never = fixture.script("never-ssh", "exit 99\n");
    let destination = format!("backup-host:{}", fixture.path("dst").display());
    let output = fixture
        .rsy(
            &[
                "-e",
                &fixture.fake_ssh(),
                "--ssh-port",
                "2022",
                &format!("{}/", fixture.path("src").display()),
                &destination,
            ],
            &[("RSY_RSH", never.to_str().unwrap())],
        )
        .await;
    assert_success(&output);
    assert_tree_copied(&fixture.path("dst"));
    assert_eq!(
        fixture.argv(),
        ["-T", "-p", "2022", "--", "backup-host", "rsy", "__serve"]
    );
}

#[tokio::test]
async fn launch_failures_explain_the_cause_and_touch_nothing() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.path("dst")).unwrap();
    std::fs::write(fixture.path("dst/keep.txt"), b"keep").unwrap();
    let missing = fixture.script("ssh-127", "echo 'sh: 1: rsy: not found' >&2\nexit 127\n");
    let refused = fixture.script(
        "ssh-255",
        "echo 'Permission denied (publickey).' >&2\nexit 255\n",
    );
    let source = format!("{}/", fixture.path("src").display());
    let remote = format!("backup-host:{}", fixture.path("dst").display());

    for (rsh, args, expected) in [
        (
            missing.to_str().unwrap().to_owned(),
            vec![source.as_str(), remote.as_str()],
            "--rsy-path",
        ),
        (
            refused.to_str().unwrap().to_owned(),
            vec![remote.as_str(), "local-copy"],
            "could not connect or authenticate (exit 255)",
        ),
        (
            "/nonexistent/rsy-test-ssh".to_owned(),
            vec![source.as_str(), remote.as_str()],
            "SSH client \"/nonexistent/rsy-test-ssh\" was not found",
        ),
    ] {
        let mut full = vec!["-e", rsh.as_str()];
        full.extend(args);
        let output = fixture.rsy(&full, &[]).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
        // Each cause is reported once, not repeated by wrapper layers.
        let first_line = stderr
            .lines()
            .find(|line| line.starts_with("Error:"))
            .unwrap();
        assert_eq!(first_line.matches(expected).count(), 1, "{first_line}");
    }
    assert_eq!(
        std::fs::read(fixture.path("dst/keep.txt")).unwrap(),
        b"keep"
    );
    assert!(!fixture.path("local-copy").exists());
}

#[tokio::test]
async fn invalid_specs_and_settings_fail_before_launching_ssh() {
    let fixture = Fixture::new();
    let source = format!("{}/", fixture.path("src").display());
    let ssh = fixture.fake_ssh();
    for args in [
        vec![source.as_str(), "ssh://backup-host"],
        vec![source.as_str(), "ssh://backup-host:0/srv"],
        vec![source.as_str(), "user@-oProxyCommand=touch pwned:/srv"],
        vec![
            "--ssh-port",
            "22",
            source.as_str(),
            "ssh://backup-host:2222/srv",
        ],
        vec![
            "--ssh-option",
            "NoValue",
            source.as_str(),
            "backup-host:/srv",
        ],
        vec![
            "--jump=-oProxyCommand=id",
            source.as_str(),
            "backup-host:/srv",
        ],
        vec!["--rsy-path", "", source.as_str(), "backup-host:/srv"],
        vec![
            "-e",
            "ssh 'unterminated",
            source.as_str(),
            "backup-host:/srv",
        ],
    ] {
        let mut full = Vec::new();
        if !args.contains(&"-e") {
            full.extend(["-e", ssh.as_str()]);
        }
        full.extend(args.iter().copied());
        let output = fixture.rsy(&full, &[]).await;
        assert!(!output.status.success(), "{args:?} must be refused");
        assert!(
            !fixture.path("argv").exists(),
            "{args:?} launched ssh before validation"
        );
    }
    assert!(!fixture.path("pwned").exists());
}

#[tokio::test]
async fn single_letter_hosts_are_remote_not_local_directories() {
    let fixture = Fixture::new();
    let output = fixture
        .rsy(
            &[
                "-e",
                &fixture.fake_ssh(),
                &format!("{}/", fixture.path("src").display()),
                // Relative remote path: the fake agent resolves it against
                // its own working directory, which is the fixture root.
                "a:remote-a",
            ],
            &[],
        )
        .await;
    assert_success(&output);
    assert_eq!(
        fixture.argv(),
        ["-T", "--", "a", "rsy", "__serve"],
        "`a:` must name host a"
    );
    assert!(!fixture.path("a:remote-a").exists());
    assert_tree_copied(&fixture.path("remote-a"));
}

#[tokio::test]
async fn tilde_paths_are_relative_to_the_remote_home() {
    let fixture = Fixture::new();
    // The fake agent's working directory (the fixture root) stands in for the
    // home directory sshd starts commands in.
    let output = fixture
        .rsy(
            &[
                "-e",
                &fixture.fake_ssh(),
                fixture.path("src").to_str().unwrap(),
                "backup-host:~/copies/",
            ],
            &[],
        )
        .await;
    assert_success(&output);
    assert_tree_copied(&fixture.path("copies/src"));
    assert!(!fixture.path("~").exists(), "no literal '~' directory");

    let refused = fixture
        .rsy(
            &["-e", &fixture.fake_ssh(), "src", "backup-host:~bob/x"],
            &[],
        )
        .await;
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("'~user' is not supported"));
}
