//! SSH destination identity and launch command for the v3 adapter.
//!
//! OpenSSH owns configuration resolution: `Include`, `Match`, wildcard
//! precedence, `HostName`, `User`, `ProxyJump`, `IdentityFile`,
//! `ControlMaster`, and `Compression` all come from the user's own `ssh_config`
//! applied to the alias exactly as written. `rsy` never parses or reconstructs
//! that file, so it cannot change the meaning of a configured alias. Only
//! settings the operator states explicitly (operand port/user, `-e`, and the
//! `--ssh-*` flags) are added to the command line.

use std::ffi::OsString;
use std::path::PathBuf;

/// One SSH destination as the user wrote it, plus explicit CLI overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// Host alias exactly as provided; passed to OpenSSH as one argv word.
    pub alias: OsString,
    /// Explicit `user@` from the command line; overrides config `User=` via
    /// `-l`. `None` leaves the user choice entirely to OpenSSH.
    pub user: Option<String>,
    /// Port from an `ssh://host:port/` operand; overrides config `Port=`.
    pub port: Option<u16>,
}

impl SshTarget {
    pub fn new(alias: impl Into<OsString>, user: Option<String>) -> Self {
        Self {
            alias: alias.into(),
            user,
            port: None,
        }
    }

    pub fn with_port(mut self, port: Option<u16>) -> Self {
        self.port = port;
        self
    }
}

/// Invalid explicit SSH launch settings, rejected before any connection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SshSettingsError {
    #[error("-e/--rsh: remote shell command is empty")]
    EmptyCommand,
    #[error("-e/--rsh: unterminated {0} quote in {1:?}")]
    UnterminatedQuote(&'static str, String),
    #[error("-e/--rsh: trailing backslash in {0:?}")]
    TrailingBackslash(String),
    #[error("--ssh-option {0:?}: expected an OpenSSH option such as ServerAliveInterval=15")]
    InvalidOption(String),
    #[error("--jump {0:?}: expected [user@]host[:port][,...]")]
    InvalidJump(String),
    #[error("--rsy-path must be a non-empty single-line command")]
    InvalidRemoteProgram,
    #[error(
        "conflicting SSH ports: the operand says {operand} but --ssh-port says {flag}; give the port once"
    )]
    PortConflict { operand: u16, flag: u16 },
}

/// The local program that carries the protocol, plus leading arguments
/// from `-e/--rsh` (rsync's `-e "ssh -p 2222"` convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl Default for SshCommand {
    fn default() -> Self {
        Self {
            program: OsString::from("ssh"),
            args: Vec::new(),
        }
    }
}

impl SshCommand {
    /// Split an `-e` command line into argv words without invoking a shell.
    ///
    /// Supports POSIX-style single quotes, double quotes (with `\"`, `\\`,
    /// `\$` and `` \` `` escapes) and backslash escapes outside quotes. There
    /// is no variable, glob or tilde expansion: OpenSSH itself expands `~` in
    /// `-i` and config paths.
    pub fn parse(command: &str) -> Result<Self, SshSettingsError> {
        let mut words = split_words(command)?.into_iter().map(OsString::from);
        let program = words.next().ok_or(SshSettingsError::EmptyCommand)?;
        Ok(Self {
            program,
            args: words.collect(),
        })
    }
}

fn split_words(command: &str) -> Result<Vec<String>, SshSettingsError> {
    let unterminated =
        |kind: &'static str| SshSettingsError::UnterminatedQuote(kind, command.to_owned());
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err(unterminated("single")),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => current.push(c),
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => return Err(unterminated("double")),
                        },
                        Some(c) => current.push(c),
                        None => return Err(unterminated("double")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => current.push(c),
                    None => return Err(SshSettingsError::TrailingBackslash(command.to_owned())),
                }
            }
            c => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

/// Explicit SSH settings from the command line, validated and ready to be
/// turned into OpenSSH arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshSettings {
    /// Program and leading arguments; defaults to plain `ssh`.
    pub command: SshCommand,
    /// `--ssh-port` (`-p`); must agree with an operand port when both exist.
    pub port: Option<u16>,
    /// `--ssh-key` (`-i`), in order.
    pub identity_files: Vec<PathBuf>,
    /// `--ssh-option` (`-o`), in order.
    pub options: Vec<String>,
    /// `--jump` (`-J`).
    pub jump: Option<String>,
    /// `--rsy-path`: remote command that starts the agent; default `rsy`.
    pub remote_program: Option<String>,
}

impl SshSettings {
    pub fn new(
        rsh: Option<&str>,
        port: Option<u16>,
        identity_files: Vec<PathBuf>,
        options: Vec<String>,
        jump: Option<String>,
        remote_program: Option<String>,
    ) -> Result<Self, SshSettingsError> {
        let command = rsh.map(SshCommand::parse).transpose()?.unwrap_or_default();
        for option in &options {
            // `-o` takes `Key=Value` or `Key Value`. A leading '-' would be a
            // flag in disguise and an empty option is always a mistake.
            let key = option.split(['=', ' ', '\t']).next().unwrap_or_default();
            if key.is_empty()
                || option.starts_with('-')
                || option.len() == key.len()
                || option.contains(['\n', '\r', '\0'])
            {
                return Err(SshSettingsError::InvalidOption(option.clone()));
            }
        }
        if let Some(jump) = &jump {
            // OpenSSH re-launches itself for each hop; an option-shaped hop
            // would reach that nested command line.
            if jump.is_empty()
                || jump
                    .split(',')
                    .any(|hop| hop.is_empty() || hop.starts_with('-'))
                || jump.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                return Err(SshSettingsError::InvalidJump(jump.clone()));
            }
        }
        if let Some(program) = &remote_program {
            if program.trim().is_empty() || program.contains(['\n', '\r', '\0']) {
                return Err(SshSettingsError::InvalidRemoteProgram);
            }
        }
        Ok(Self {
            command,
            port,
            identity_files,
            options,
            jump,
            remote_program,
        })
    }

    /// Combine the operand port (`ssh://host:PORT/`) with `--ssh-port`.
    pub fn effective_port(&self, operand: Option<u16>) -> Result<Option<u16>, SshSettingsError> {
        match (operand, self.port) {
            (Some(operand), Some(flag)) if operand != flag => {
                Err(SshSettingsError::PortConflict { operand, flag })
            }
            (operand, flag) => Ok(operand.or(flag)),
        }
    }

    /// The remote command that starts the agent.
    pub fn remote_program(&self) -> &str {
        self.remote_program.as_deref().unwrap_or("rsy")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<String> {
        let parsed = SshCommand::parse(command).unwrap();
        std::iter::once(parsed.program)
            .chain(parsed.args)
            .map(|word| word.into_string().unwrap())
            .collect()
    }

    #[test]
    fn rsh_splits_like_a_shell_without_running_one() {
        assert_eq!(words("ssh"), ["ssh"]);
        assert_eq!(
            words("  ssh -p 2222   -i ~/.ssh/id_ed25519 "),
            ["ssh", "-p", "2222", "-i", "~/.ssh/id_ed25519"]
        );
        assert_eq!(
            words(r#"ssh -o "ProxyCommand=nc -X 5 -x proxy:1080 %h %p" -F '/my config'"#),
            [
                "ssh",
                "-o",
                "ProxyCommand=nc -X 5 -x proxy:1080 %h %p",
                "-F",
                "/my config"
            ]
        );
        assert_eq!(words(r"/opt/ssh\ client/ssh"), ["/opt/ssh client/ssh"]);
        assert_eq!(words(r#""say \"hi\" \$HOME \n""#), [r#"say "hi" $HOME \n"#]);
        assert_eq!(words("ssh ''"), ["ssh", ""]);
        // No expansion of any kind: these reach OpenSSH literally.
        assert_eq!(words("ssh $HOME `id` *"), ["ssh", "$HOME", "`id`", "*"]);
    }

    #[test]
    fn rsh_rejects_empty_and_unbalanced_input() {
        for (command, expected) in [
            ("", SshSettingsError::EmptyCommand),
            ("   ", SshSettingsError::EmptyCommand),
            (
                "ssh 'oops",
                SshSettingsError::UnterminatedQuote("single", "ssh 'oops".into()),
            ),
            (
                "ssh \"oops",
                SshSettingsError::UnterminatedQuote("double", "ssh \"oops".into()),
            ),
            (
                "ssh \\",
                SshSettingsError::TrailingBackslash("ssh \\".into()),
            ),
        ] {
            assert_eq!(SshCommand::parse(command), Err(expected), "{command:?}");
        }
    }

    #[test]
    fn settings_validate_options_jump_and_remote_program() {
        let check = |options: &[&str], jump: Option<&str>, program: Option<&str>| {
            SshSettings::new(
                None,
                None,
                Vec::new(),
                options.iter().map(|o| o.to_string()).collect(),
                jump.map(str::to_owned),
                program.map(str::to_owned),
            )
        };
        assert!(check(
            &["ServerAliveInterval=15", "StrictHostKeyChecking accept-new"],
            Some("alice@bastion:2222,inner"),
            Some("sudo -n /opt/rsy/bin/rsy")
        )
        .is_ok());
        for bad in ["", "=x", "-oProxyCommand=id", "NoValue", "A=b\nB=c"] {
            assert_eq!(
                check(&[bad], None, None),
                Err(SshSettingsError::InvalidOption(bad.into())),
                "{bad:?}"
            );
        }
        for bad in ["", "-oProxyCommand=id", "a,,b", "a,-b", "two words"] {
            assert_eq!(
                check(&[], Some(bad), None),
                Err(SshSettingsError::InvalidJump(bad.into())),
                "{bad:?}"
            );
        }
        for bad in ["", "  ", "rsy\nrm -rf ~"] {
            assert_eq!(
                check(&[], None, Some(bad)),
                Err(SshSettingsError::InvalidRemoteProgram)
            );
        }
    }

    #[test]
    fn operand_and_flag_ports_must_agree() {
        let settings = SshSettings {
            port: Some(2222),
            ..SshSettings::default()
        };
        assert_eq!(settings.effective_port(None), Ok(Some(2222)));
        assert_eq!(settings.effective_port(Some(2222)), Ok(Some(2222)));
        assert_eq!(
            settings.effective_port(Some(22)),
            Err(SshSettingsError::PortConflict {
                operand: 22,
                flag: 2222
            })
        );
        assert_eq!(
            SshSettings::default().effective_port(Some(22)),
            Ok(Some(22))
        );
        assert_eq!(SshSettings::default().remote_program(), "rsy");
    }
}
