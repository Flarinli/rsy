use crate::protocol::Operation;
use crate::remote::router::RouterConfig;
use crate::remote::runtime::ClientRemoteSession;
use crate::ssh::{SshSettings, SshTarget};
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::{Child, Command};

/// Owns the OpenSSH child for one v3 remote session.
///
/// Ordinary paths explicitly finish or abort and reap the child. Cancellation
/// drops this owner: router tasks are aborted and `kill_on_drop` kills OpenSSH
/// as emergency cleanup (Tokio provides best-effort reaping on that path).
pub struct SshRemoteSession {
    child: Child,
    remote: ClientRemoteSession,
}

// Completion is not a transfer timeout: even without --timeout, a peer must
// not keep an otherwise completed sync waiting indefinitely for EOF or exit.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SshCompletionError {
    #[error("OpenSSH exited unsuccessfully after the protocol operation: {0}")]
    ExitStatus(ExitStatus),
    #[error("OpenSSH session completion exceeded its shutdown deadline")]
    Timeout,
    #[error("OpenSSH session cleanup exceeded its shutdown deadline")]
    CleanupTimeout,
}

/// SSH session launch options that the v3 adapter maps from CLI flags.
#[derive(Debug, Clone, Default)]
pub struct SshLaunchOptions {
    /// `--contimeout`: passed to OpenSSH as `-o ConnectTimeout=N`. `None`
    /// leaves OpenSSH's own default (the system TCP connect timeout).
    pub connect_timeout: Option<u64>,
    /// `-e/--rsh`, `--ssh-*`, `--jump` and `--rsy-path`.
    pub settings: SshSettings,
}

/// A launch that failed in a way the operator can act on. Each variant is
/// reported instead of a bare handshake EOF, which hides the real cause.
#[derive(Debug, thiserror::Error)]
pub enum SshLaunchError {
    #[error(
        "SSH client {0:?} was not found: install OpenSSH or point -e/--rsh (or RSY_RSH) at it"
    )]
    ClientNotFound(OsString),
    #[error(
        "the remote host could not find {0:?} (exit 127): install rsy so it is on PATH for \
         non-interactive SSH sessions, or pass --rsy-path /path/to/rsy"
    )]
    RemoteAgentNotFound(String),
    #[error(
        "the remote host found {0:?} but could not run it (exit 126): check that it is executable \
         and built for the remote OS/architecture"
    )]
    RemoteAgentNotExecutable(String),
    #[error("ssh could not connect or authenticate (exit 255); see the ssh message above")]
    ConnectionFailed,
}

impl SshRemoteSession {
    pub async fn connect(
        target: &SshTarget,
        operation: Operation,
        remote_root: &Path,
        router_config: RouterConfig,
    ) -> Result<Self> {
        Self::connect_with_options(
            target,
            operation,
            remote_root,
            router_config,
            SshLaunchOptions::default(),
        )
        .await
    }

    pub async fn connect_with_options(
        target: &SshTarget,
        operation: Operation,
        remote_root: &Path,
        router_config: RouterConfig,
        launch: SshLaunchOptions,
    ) -> Result<Self> {
        let command = ssh_command(target, &launch);
        Self::connect_command_operand(
            command,
            operation,
            remote_root,
            None,
            router_config,
            &launch.settings,
        )
        .await
    }

    pub async fn connect_operand_with_options(
        target: &SshTarget,
        operation: Operation,
        path: &Path,
        operand: &crate::remote::operand::RemoteOperand,
        router_config: RouterConfig,
        launch: SshLaunchOptions,
    ) -> Result<Self> {
        let command = ssh_command(target, &launch);
        Self::connect_command_operand(
            command,
            operation,
            path,
            Some(operand),
            router_config,
            &launch.settings,
        )
        .await
    }

    #[cfg(test)]
    async fn connect_command(
        command: Command,
        operation: Operation,
        path: &Path,
        config: RouterConfig,
    ) -> Result<Self> {
        Self::connect_command_operand(
            command,
            operation,
            path,
            None,
            config,
            &SshSettings::default(),
        )
        .await
    }

    async fn connect_command_operand(
        mut command: Command,
        operation: Operation,
        remote_root: &Path,
        operand: Option<&crate::remote::operand::RemoteOperand>,
        router_config: RouterConfig,
        settings: &SshSettings,
    ) -> Result<Self> {
        command.kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let program = command.as_std().get_program().to_os_string();
                return Err(
                    anyhow::Error::new(error).context(SshLaunchError::ClientNotFound(program))
                );
            }
            Err(error) => {
                return Err(
                    anyhow::Error::new(error).context("failed to spawn OpenSSH for v3 session")
                )
            }
        };
        let connection = async {
            let writer = child.stdin.take().context("failed to open v3 SSH stdin")?;
            let reader = child
                .stdout
                .take()
                .context("failed to open v3 SSH stdout")?;
            match operand {
                Some(operand) => {
                    ClientRemoteSession::connect_operand(
                        reader,
                        writer,
                        operation,
                        remote_root,
                        operand,
                        router_config,
                    )
                    .await
                }
                None => {
                    ClientRemoteSession::connect(
                        reader,
                        writer,
                        operation,
                        remote_root,
                        router_config,
                    )
                    .await
                }
            }
            .context("v3 SSH handshake failed")
        }
        .await;
        match connection {
            Ok(remote) => Ok(Self { child, remote }),
            Err(error) => {
                // A missing agent or a refused login ends the pipe before any
                // handshake byte; the exit status says which. Prefer that
                // diagnosis over the bare EOF, but never wait on a live peer.
                let error = match diagnose_exit(&mut child, settings).await {
                    Some(diagnosis) => error.context(diagnosis),
                    None => error,
                };
                // Preserve the handshake/I/O error, not the exit caused by our
                // own termination. Cancellation still drops the local Child.
                if let Err(cleanup) = terminate_child(&mut child).await {
                    tracing::warn!(%cleanup, "failed to clean up SSH handshake");
                }
                Err(error)
            }
        }
    }

    pub const fn remote(&self) -> &ClientRemoteSession {
        &self.remote
    }

    /// After all requests complete, close stdin cleanly, join transport actors,
    /// and reap OpenSSH. A valid protocol Ack does not waive its exit status.
    /// Failure here does not roll back already published destination entries.
    pub async fn finish(mut self) -> Result<()> {
        let completion = tokio::time::timeout(COMPLETION_TIMEOUT, async {
            self.remote.finish().await?;
            let status = self.child.wait().await.context("failed to reap OpenSSH")?;
            if !status.success() {
                return Err(SshCompletionError::ExitStatus(status).into());
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(SshCompletionError::Timeout.into()));
        if completion.is_err() {
            if let Err(cleanup) = self.shutdown().await {
                tracing::warn!(%cleanup, "failed to clean up SSH completion");
            }
        }
        completion
    }

    /// Terminate and reap after an operation error. The caller must retain that
    /// authoritative error rather than replace it with termination/cleanup status.
    pub async fn abort(mut self) -> Result<()> {
        self.shutdown().await
    }

    async fn shutdown(&mut self) -> Result<()> {
        // Signal the process before waiting for actors; neither a stalled peer
        // nor an actor waiting on its pipe can delay termination indefinitely.
        let kill = self.child.start_kill();
        tokio::time::timeout(CLEANUP_TIMEOUT, async {
            let transport = self.remote.shutdown().await;
            let reaped = self.child.wait().await;
            kill.context("failed to terminate OpenSSH")?;
            reaped.context("failed to reap terminated OpenSSH")?;
            transport.context("failed to join SSH transport actors")?;
            Ok(())
        })
        .await
        .unwrap_or_else(|_| Err(SshCompletionError::CleanupTimeout.into()))
    }
}

// Long enough for OpenSSH to exit after its remote command fails, short
// enough that a peer which is merely speaking garbage is not waited on.
const DIAGNOSE_EXIT_GRACE: Duration = Duration::from_millis(1500);

async fn diagnose_exit(child: &mut Child, settings: &SshSettings) -> Option<SshLaunchError> {
    let status = tokio::time::timeout(DIAGNOSE_EXIT_GRACE, child.wait())
        .await
        .ok()?
        .ok()?;
    let program = settings.remote_program().to_owned();
    match status.code()? {
        127 => Some(SshLaunchError::RemoteAgentNotFound(program)),
        126 => Some(SshLaunchError::RemoteAgentNotExecutable(program)),
        255 => Some(SshLaunchError::ConnectionFailed),
        _ => None,
    }
}

async fn terminate_child(child: &mut Child) -> Result<()> {
    tokio::time::timeout(CLEANUP_TIMEOUT, child.kill())
        .await
        .map_err(|_| SshCompletionError::CleanupTimeout)?
        .context("failed to terminate and reap OpenSSH")
}

fn ssh_command(target: &SshTarget, launch: &SshLaunchOptions) -> Command {
    let mut command = Command::new(&launch.settings.command.program);
    command.args(ssh_arguments(target, launch));
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::inherit());
    command
}

/// OpenSSH argv after the program name.
///
/// Order matters: `-e` words come first, then rsy's explicit overrides, then
/// `--` and the host, then the remote command. For `-o` settings OpenSSH keeps
/// the first value it sees; for `-p`/`-l`/`-J` the last one wins. Nothing here
/// is derived from ssh_config.
fn ssh_arguments(target: &SshTarget, launch: &SshLaunchOptions) -> Vec<OsString> {
    let settings = &launch.settings;
    let mut args = settings.command.args.clone();
    // The protocol is binary: a config-level `RequestTTY force` would allocate a
    // pty that rewrites bytes and enables escape sequences, so always disable it.
    args.push(OsString::from("-T"));

    if let Some(port) = target.port.or(settings.port) {
        args.push(OsString::from("-p"));
        args.push(OsString::from(port.to_string()));
    }
    if let Some(user) = &target.user {
        args.push(OsString::from("-l"));
        args.push(OsString::from(user));
    }
    for identity in &settings.identity_files {
        args.push(OsString::from("-i"));
        args.push(identity.clone().into_os_string());
    }
    if let Some(jump) = &settings.jump {
        args.push(OsString::from("-J"));
        args.push(OsString::from(jump));
    }
    if let Some(connect_timeout) = launch.connect_timeout {
        args.push(OsString::from("-o"));
        args.push(OsString::from(format!("ConnectTimeout={connect_timeout}")));
    }
    for option in &settings.options {
        args.push(OsString::from("-o"));
        args.push(OsString::from(option));
    }

    // The alias is one argv word and goes through untouched: Host matching,
    // Include, Match, HostName, ProxyJump, IdentityFile, ControlMaster and
    // Compression all belong to OpenSSH. No local shell is involved. `--` ends
    // option parsing so an alias such as `-oProxyCommand=...` cannot be read
    // as an OpenSSH option.
    args.push(OsString::from("--"));
    args.push(target.alias.clone());
    // OpenSSH hands the remote command to the remote user's shell. Only the
    // agent command goes there; roots and paths are protocol data.
    args.push(OsString::from(settings.remote_program()));
    args.push(OsString::from("__serve"));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    async fn assert_process_gone(pid: u32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = Command::new("/bin/kill")
                    .args(["-0", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .unwrap();
                if !status.success() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("owned SSH child was not killed/reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handshake_error_terminates_and_reaps_spawned_process() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '%s' \"$$\" > \"$PID_FILE\"; printf 'invalid handshake header................'; exec /bin/sleep 60"])
            .env("PID_FILE", &pid_file)
            .stdin(Stdio::piped()).stdout(Stdio::piped());
        let error = match SshRemoteSession::connect_command(
            command,
            Operation::Pull,
            root.path(),
            RouterConfig::default(),
        )
        .await
        {
            Ok(_) => panic!("malformed handshake must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("v3 SSH handshake failed"));
        let pid = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert_process_gone(pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_and_cancelled_finish_keep_emergency_child_cleanup() {
        use crate::remote::runtime::ServerRemoteSession;
        for cancel_finish in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (client_io, server_io) = tokio::io::duplex(64 * 1024);
            let (reader, writer) = tokio::io::split(client_io);
            let (server_reader, server_writer) = tokio::io::split(server_io);
            let (client, server) = tokio::join!(
                ClientRemoteSession::connect(
                    reader,
                    writer,
                    Operation::Pull,
                    root.path(),
                    RouterConfig::default()
                ),
                ServerRemoteSession::accept(server_reader, server_writer, RouterConfig::default()),
            );
            let mut server = server.unwrap();
            // Here the duplex holds the negotiated transport open while the
            // real sleeping subprocess exercises only emergency ownership.
            // CLI process-pipe tests cover ordinary responses and exit status.
            let child = Command::new("/bin/sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let pid = child.id().unwrap();
            let session = SshRemoteSession {
                child,
                remote: client.unwrap(),
            };
            if cancel_finish {
                assert!(
                    tokio::time::timeout(Duration::from_millis(50), session.finish())
                        .await
                        .is_err()
                );
            } else {
                drop(session);
            }
            assert_process_gone(pid).await;
            server.shutdown().await.unwrap();
        }
    }

    #[test]
    fn launcher_passes_alias_untouched_with_private_agent() {
        // The alias is one argv word: no shell, no parsing, no reconstruction.
        // Even hostile-looking aliases belong to OpenSSH's own resolution.
        let target = SshTarget::new("host alias;$(touch /tmp/pwn)", Some("alice".to_string()));
        let args = ssh_arguments(&target, &SshLaunchOptions::default());

        assert_eq!(
            &args[args.len() - 3..],
            [
                OsString::from("host alias;$(touch /tmp/pwn)"),
                OsString::from("rsy"),
                OsString::from("__serve"),
            ]
        );
        assert_eq!(
            &args[..3],
            [
                OsString::from("-T"),
                OsString::from("-l"),
                OsString::from("alice")
            ]
        );
    }

    #[test]
    fn option_shaped_alias_cannot_inject_openssh_options() {
        let target = SshTarget::new("-oProxyCommand=touch pwned", Some("alice".to_string()));
        let args = ssh_arguments(&target, &SshLaunchOptions::default());
        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("alias must follow an end-of-options separator");
        assert_eq!(
            args[separator + 1],
            OsString::from("-oProxyCommand=touch pwned")
        );
        assert!(!args[..separator]
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("-oProxy")));
    }

    #[test]
    fn launcher_defers_configuration_to_openssh() {
        let target = SshTarget::new("gh", None);
        let args = ssh_arguments(&target, &SshLaunchOptions::default());
        let rendered: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        // Port, identities, jump hosts, control masters, and compression all
        // come from the user's ssh_config; rsy must not reconstruct them.
        for reconstructed in ["-p", "-i", "-J", "-C", "-o", "-l"] {
            assert!(
                !rendered.iter().any(|arg| arg == reconstructed),
                "{reconstructed} must not be synthesized from ssh_config"
            );
        }
        // No remote root on argv: roots are protocol data.
        assert!(!rendered.iter().any(|arg| arg.contains("/remote/root")));
        // No user: OpenSSH applies the alias's own User= (or the local user).
        assert_eq!(rendered, vec!["-T", "--", "gh", "rsy", "__serve"]);
    }

    #[test]
    fn contimeout_maps_to_openssh_connect_timeout() {
        let target = SshTarget::new("host.example", None);
        let launch = SshLaunchOptions {
            connect_timeout: Some(10),
            ..SshLaunchOptions::default()
        };
        let args = ssh_arguments(&target, &launch);
        let options: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            options,
            vec![
                "-T",
                "-o",
                "ConnectTimeout=10",
                "--",
                "host.example",
                "rsy",
                "__serve"
            ]
        );
    }

    #[test]
    fn explicit_settings_precede_the_separator_in_a_fixed_order() {
        let target = SshTarget::new("backup", Some("alice".into())).with_port(Some(2222));
        let launch = SshLaunchOptions {
            connect_timeout: Some(5),
            settings: SshSettings::new(
                Some("/usr/local/bin/ssh -F '/etc/my ssh config'"),
                None,
                vec!["~/.ssh/a".into(), "~/.ssh/b".into()],
                vec!["ServerAliveInterval=15".into()],
                Some("gw".into()),
                Some("sudo -n /opt/rsy/bin/rsy".into()),
            )
            .unwrap(),
        };
        let command = ssh_command(&target, &launch);
        assert_eq!(command.as_std().get_program(), "/usr/local/bin/ssh");
        let args: Vec<String> = ssh_arguments(&target, &launch)
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect();
        assert_eq!(
            args,
            [
                "-F",
                "/etc/my ssh config",
                "-T",
                "-p",
                "2222",
                "-l",
                "alice",
                "-i",
                "~/.ssh/a",
                "-i",
                "~/.ssh/b",
                "-J",
                "gw",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ServerAliveInterval=15",
                "--",
                "backup",
                "sudo -n /opt/rsy/bin/rsy",
                "__serve",
            ]
        );
    }

    #[test]
    fn flag_port_applies_when_operand_has_none() {
        let launch = SshLaunchOptions {
            settings: SshSettings {
                port: Some(2200),
                ..SshSettings::default()
            },
            ..SshLaunchOptions::default()
        };
        let args = ssh_arguments(&SshTarget::new("h", None), &launch);
        assert_eq!(
            args,
            ["-T", "-p", "2200", "--", "h", "rsy", "__serve"].map(OsString::from)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn early_exit_status_explains_handshake_failures() {
        let root = tempfile::tempdir().unwrap();
        for (code, expected) in [
            (127, "could not find \"rsy\""),
            (126, "could not run it"),
            (255, "could not connect or authenticate"),
        ] {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", &format!("exit {code}")])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            let error = match SshRemoteSession::connect_command(
                command,
                Operation::Pull,
                root.path(),
                RouterConfig::default(),
            )
            .await
            {
                Ok(_) => panic!("exit {code} must fail the handshake"),
                Err(error) => format!("{error:#}"),
            };
            assert!(error.contains(expected), "exit {code}: {error}");
            assert!(error.contains("v3 SSH handshake failed"), "{error}");
        }
    }

    #[tokio::test]
    async fn missing_client_names_the_program() {
        let launch = SshLaunchOptions {
            settings: SshSettings::new(
                Some("/nonexistent/rsy-test-ssh"),
                None,
                Vec::new(),
                Vec::new(),
                None,
                None,
            )
            .unwrap(),
            ..SshLaunchOptions::default()
        };
        let root = tempfile::tempdir().unwrap();
        let error = match SshRemoteSession::connect_with_options(
            &SshTarget::new("h", None),
            Operation::Pull,
            root.path(),
            RouterConfig::default(),
            launch,
        )
        .await
        {
            Ok(_) => panic!("a missing client cannot connect"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("/nonexistent/rsy-test-ssh"), "{error}");
        assert!(error.contains("was not found"), "{error}");
    }
}
