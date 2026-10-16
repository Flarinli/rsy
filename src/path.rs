use std::path::{Path, PathBuf};

/// Represents a sync path that can be either local, remote (SSH), or S3
#[derive(Debug, Clone, PartialEq)]
pub enum SyncPath {
    Local {
        path: PathBuf,
        has_trailing_slash: bool,
    },
    Remote {
        /// Host alias or address passed to OpenSSH; IPv6 literals are stored
        /// without brackets.
        host: String,
        user: Option<String>,
        /// Port from an `ssh://host:port/` URL; `None` defers to ssh_config.
        port: Option<u16>,
        path: PathBuf,
        has_trailing_slash: bool,
    },
    S3 {
        bucket: String,
        key: String,
        region: Option<String>,
        endpoint: Option<String>,
        has_trailing_slash: bool,
    },
    Gcs {
        bucket: String,
        key: String,
        has_trailing_slash: bool,
    },
}

/// SSH authority of a remote operand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAuthority {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// A remote operand that looks like SSH syntax but cannot be used safely.
///
/// These are errors rather than a fallback to a local path: silently
/// reinterpreting a mistyped remote spec as a local directory would write (or
/// delete, with `--delete`) in the wrong place.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteSpecError {
    #[error("ssh:// URL needs an absolute path after the host (e.g. ssh://host/srv/data): {0}")]
    UrlMissingPath(String),
    #[error("SSH host is empty in {0:?}")]
    EmptyHost(String),
    #[error("SSH user before '@' is empty in {0:?}")]
    EmptyUser(String),
    #[error("SSH host {0:?} must not start with '-'")]
    OptionLikeHost(String),
    #[error("SSH user {0:?} must not start with '-'")]
    OptionLikeUser(String),
    #[error("SSH host or user contains whitespace or control characters: {0:?}")]
    InvalidCharacter(String),
    #[error("unterminated IPv6 literal (missing ']') in {0:?}")]
    UnterminatedBracket(String),
    #[error(
        "{0:?}: '~user' is not supported in remote paths; use an absolute path or a path relative to your remote home"
    )]
    TildeUser(String),
    #[error("IPv6 addresses in ssh:// URLs must use brackets, e.g. ssh://[::1]:22/path: {0:?}")]
    UnbracketedIpv6(String),
    #[error("invalid SSH port {port:?} in {spec:?}: expected 1-65535")]
    InvalidPort { spec: String, port: String },
    #[error("remote host/user must be UTF-8 (use ./ for a local path)")]
    NonUtf8Authority,
}

/// Split an operand into an SSH authority and the remote path bytes.
///
/// Returns `Ok(None)` for local paths. Accepted remote forms:
/// - `[user@]host:path` (scp style; `host` may be a bracketed IPv6 literal)
/// - `ssh://[user@]host[:port]/path`
///
/// Splits only at ASCII delimiters, so the returned path keeps its exact
/// native bytes (non-UTF-8 Unix names survive).
pub fn split_remote(spec: &[u8]) -> Result<Option<(RemoteAuthority, &[u8])>, RemoteSpecError> {
    if let Some(rest) = spec.strip_prefix(b"ssh://") {
        let display = || String::from_utf8_lossy(spec).into_owned();
        let slash = rest
            .iter()
            .position(|&b| b == b'/')
            .ok_or_else(|| RemoteSpecError::UrlMissingPath(display()))?;
        let (authority, path) = rest.split_at(slash);
        let authority =
            std::str::from_utf8(authority).map_err(|_| RemoteSpecError::NonUtf8Authority)?;
        let (user, host_port) = match authority.rsplit_once('@') {
            Some((user, host_port)) => (Some(user), host_port),
            None => (None, authority),
        };
        let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
            let (host, after) = bracketed
                .split_once(']')
                .ok_or_else(|| RemoteSpecError::UnterminatedBracket(display()))?;
            match after {
                "" => (host, None),
                _ => match after.strip_prefix(':') {
                    Some(port) => (host, Some(port)),
                    None => {
                        return Err(RemoteSpecError::InvalidPort {
                            spec: display(),
                            port: after.to_owned(),
                        })
                    }
                },
            }
        } else {
            match host_port.matches(':').count() {
                0 => (host_port, None),
                1 => {
                    let (host, port) = host_port.split_once(':').unwrap_or((host_port, ""));
                    (host, Some(port))
                }
                _ => return Err(RemoteSpecError::UnbracketedIpv6(display())),
            }
        };
        let port = port
            .map(|port| match port.parse::<u16>() {
                Ok(value) if value != 0 => Ok(value),
                _ => Err(RemoteSpecError::InvalidPort {
                    spec: display(),
                    port: port.to_owned(),
                }),
            })
            .transpose()?;
        let authority = validate_authority(user, host, port, &display())?;
        return Ok(Some((authority, path)));
    }

    // scp style. A user part ends at the first '@' that precedes any path
    // separator, colon, or IPv6 bracket.
    let (user, rest) = match spec
        .iter()
        .position(|&b| matches!(b, b'@' | b'/' | b':' | b'['))
    {
        Some(at) if spec[at] == b'@' => (Some(&spec[..at]), &spec[at + 1..]),
        _ => (None, spec),
    };
    let (host, path) = if let Some(bracketed) = rest.strip_prefix(b"[") {
        let Some(close) = bracketed.iter().position(|&b| b == b']') else {
            if user.is_some() {
                return Err(RemoteSpecError::UnterminatedBracket(
                    String::from_utf8_lossy(spec).into_owned(),
                ));
            }
            return Ok(None);
        };
        match bracketed[close + 1..].strip_prefix(b":") {
            Some(path) => (&bracketed[..close], path),
            None => return Ok(None),
        }
    } else {
        let Some(colon) = rest.iter().position(|&b| b == b':') else {
            return Ok(None);
        };
        let host = &rest[..colon];
        if host.contains(&b'/') {
            return Ok(None);
        }
        // `C:\dir` / `d:/dir` is a drive letter only on Windows. Elsewhere
        // `a:/backup` names host `a`; silently copying into a local `a:`
        // directory instead would put (or with --delete, remove) data in
        // the wrong place.
        if cfg!(windows) && user.is_none() && matches!(host, [b'a'..=b'z' | b'A'..=b'Z']) {
            return Ok(None);
        }
        // `:name` stays a local relative path, as before.
        if user.is_none() && host.is_empty() {
            return Ok(None);
        }
        (host, &rest[colon + 1..])
    };
    let user = user
        .map(|user| std::str::from_utf8(user).map_err(|_| RemoteSpecError::NonUtf8Authority))
        .transpose()?;
    let host = std::str::from_utf8(host).map_err(|_| RemoteSpecError::NonUtf8Authority)?;
    let authority = validate_authority(user, host, None, &String::from_utf8_lossy(spec))?;
    Ok(Some((authority, home_relative(path, spec)?)))
}

/// Interpret scp-style home shorthands without a remote shell.
///
/// Remote paths are protocol data, so nothing expands `~` on the far side.
/// sshd starts the agent in the login user's home directory, so `~`, `~/x`
/// and an empty path become paths relative to it instead of a literal `~`
/// directory. `~user` would need a remote lookup and is refused.
fn home_relative<'a>(path: &'a [u8], spec: &[u8]) -> Result<&'a [u8], RemoteSpecError> {
    match path {
        b"" | b"~" => Ok(b"."),
        b"~/" => Ok(b"./"),
        [b'~', b'/', rest @ ..] => Ok(rest),
        [b'~', ..] => Err(RemoteSpecError::TildeUser(
            String::from_utf8_lossy(spec).into_owned(),
        )),
        _ => Ok(path),
    }
}

fn validate_authority(
    user: Option<&str>,
    host: &str,
    port: Option<u16>,
    spec: &str,
) -> Result<RemoteAuthority, RemoteSpecError> {
    let invalid = |text: &str| text.chars().any(|c| c.is_whitespace() || c.is_control());
    if host.is_empty() {
        return Err(RemoteSpecError::EmptyHost(spec.to_owned()));
    }
    // OpenSSH gets `--` before the host as well; refusing option-shaped names
    // here also keeps them out of ProxyJump/ProxyCommand expansions.
    if host.starts_with('-') {
        return Err(RemoteSpecError::OptionLikeHost(host.to_owned()));
    }
    if invalid(host) {
        return Err(RemoteSpecError::InvalidCharacter(spec.to_owned()));
    }
    if let Some(user) = user {
        if user.is_empty() {
            return Err(RemoteSpecError::EmptyUser(spec.to_owned()));
        }
        if user.starts_with('-') {
            return Err(RemoteSpecError::OptionLikeUser(user.to_owned()));
        }
        if invalid(user) {
            return Err(RemoteSpecError::InvalidCharacter(spec.to_owned()));
        }
    }
    Ok(RemoteAuthority {
        user: user.map(str::to_owned),
        host: host.to_owned(),
        port,
    })
}

impl SyncPath {
    /// Parse a path string into a SyncPath
    ///
    /// Supported formats:
    /// - Local: `/path/to/dir`, `./relative/path`, `relative/path`
    /// - Remote: `user@host:/path`, `host:/path`, `user@[::1]:/path`,
    ///   `ssh://[user@]host[:port]/path`
    /// - S3: `s3://bucket/key/path`, `s3://bucket/key?region=us-west-2`, `s3://bucket/key?endpoint=https://...`
    ///
    /// Trailing slash semantics (rsync-compatible):
    /// - `/path/to/dir` (no slash): Copy directory itself to destination
    /// - `/path/to/dir/` (with slash): Copy directory contents to destination
    pub fn parse(s: &str) -> Result<Self, RemoteSpecError> {
        // Detect trailing slash (before parsing)
        // For S3 paths with query parameters, check the path portion before '?'
        let has_trailing_slash = if s.starts_with("s3://") {
            if let Some(q_pos) = s.find('?') {
                s[..q_pos].ends_with('/')
            } else {
                s.ends_with('/')
            }
        } else {
            s.ends_with('/') || s.ends_with('\\')
        };
        // Check for S3 URL format
        if let Some(remainder) = s.strip_prefix("s3://") {
            // Split on ? to separate path from query params
            let (path_part, query_part) = if let Some(q_pos) = remainder.find('?') {
                (&remainder[..q_pos], Some(&remainder[q_pos + 1..]))
            } else {
                (remainder, None)
            };

            // Split path into bucket and key
            if let Some(slash_pos) = path_part.find('/') {
                let bucket = path_part[..slash_pos].to_string();
                let key = path_part[slash_pos + 1..].to_string();

                // Parse query parameters (region, endpoint)
                let mut region = None;
                let mut endpoint = None;

                if let Some(query) = query_part {
                    for param in query.split('&') {
                        if let Some((k, v)) = param.split_once('=') {
                            match k {
                                "region" => region = Some(v.to_string()),
                                "endpoint" => endpoint = Some(v.to_string()),
                                _ => {} // Ignore unknown params
                            }
                        }
                    }
                }

                return Ok(SyncPath::S3 {
                    bucket,
                    key,
                    region,
                    endpoint,
                    has_trailing_slash,
                });
            } else {
                // Just bucket, no key (treat as root)
                return Ok(SyncPath::S3 {
                    bucket: path_part.to_string(),
                    key: String::new(),
                    region: None,
                    endpoint: None,
                    has_trailing_slash,
                });
            }
        }

        // Check for GCS URL format
        if let Some(remainder) = s.strip_prefix("gs://") {
            // GCS usually doesn't use query params for region/endpoint in the same way,
            // but we'll handle basic gs://bucket/key
            let (path_part, _query_part) = if let Some(q_pos) = remainder.find('?') {
                (&remainder[..q_pos], Some(&remainder[q_pos + 1..]))
            } else {
                (remainder, None)
            };

            // Split path into bucket and key
            if let Some(slash_pos) = path_part.find('/') {
                let bucket = path_part[..slash_pos].to_string();
                let key = path_part[slash_pos + 1..].to_string();

                return Ok(SyncPath::Gcs {
                    bucket,
                    key,
                    has_trailing_slash,
                });
            } else {
                // Just bucket, no key (treat as root)
                return Ok(SyncPath::Gcs {
                    bucket: path_part.to_string(),
                    key: String::new(),
                    has_trailing_slash,
                });
            }
        }

        if let Some((authority, path)) = split_remote(s.as_bytes())? {
            let path = std::str::from_utf8(path).map_err(|_| RemoteSpecError::NonUtf8Authority)?;
            return Ok(SyncPath::Remote {
                host: authority.host,
                user: authority.user,
                port: authority.port,
                path: PathBuf::from(path),
                has_trailing_slash,
            });
        }

        // Otherwise it's a local path
        Ok(SyncPath::Local {
            path: PathBuf::from(s),
            has_trailing_slash,
        })
    }

    /// Get the path component
    pub fn path(&self) -> &Path {
        match self {
            SyncPath::Local { path, .. } => path,
            SyncPath::Remote { path, .. } => path,
            SyncPath::S3 { key, .. } => Path::new(key),
            SyncPath::Gcs { key, .. } => Path::new(key),
        }
    }

    /// Check if the original path string had a trailing slash
    ///
    /// This is used for rsync-compatible directory behavior:
    /// - No trailing slash: copy the directory itself
    /// - Trailing slash: copy only the directory contents
    pub fn has_trailing_slash(&self) -> bool {
        match self {
            SyncPath::Local {
                has_trailing_slash, ..
            } => *has_trailing_slash,
            SyncPath::Remote {
                has_trailing_slash, ..
            } => *has_trailing_slash,
            SyncPath::S3 {
                has_trailing_slash, ..
            } => *has_trailing_slash,
            SyncPath::Gcs {
                has_trailing_slash, ..
            } => *has_trailing_slash,
        }
    }

    /// Check if this is a remote SSH path
    #[allow(dead_code)] // Used in tests
    pub fn is_remote(&self) -> bool {
        matches!(self, SyncPath::Remote { .. })
    }

    /// Check if this is a local path
    pub fn is_local(&self) -> bool {
        matches!(self, SyncPath::Local { .. })
    }

    /// Check if this is an S3 path
    #[allow(dead_code)] // Public API for S3 path detection
    pub fn is_s3(&self) -> bool {
        matches!(self, SyncPath::S3 { .. })
    }

    /// Check if this is a GCS path
    #[allow(dead_code)] // Public API for GCS path detection
    pub fn is_gcs(&self) -> bool {
        matches!(self, SyncPath::Gcs { .. })
    }
}

impl std::fmt::Display for SyncPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncPath::Local { path, .. } => write!(f, "{}", path.display()),
            SyncPath::Remote {
                host,
                user,
                port,
                path,
                ..
            } => {
                let host = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.clone()
                };
                let user = user.as_ref().map(|u| format!("{u}@")).unwrap_or_default();
                match port {
                    Some(port) => write!(f, "ssh://{user}{host}:{port}{}", path.display()),
                    None => write!(f, "{user}{host}:{}", path.display()),
                }
            }
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                write!(f, "s3://{}/{}", bucket, key)?;
                let mut query_params = Vec::new();
                if let Some(r) = region {
                    query_params.push(format!("region={}", r));
                }
                if let Some(e) = endpoint {
                    query_params.push(format!("endpoint={}", e));
                }
                if !query_params.is_empty() {
                    write!(f, "?{}", query_params.join("&"))?;
                }
                Ok(())
            }
            SyncPath::Gcs { bucket, key, .. } => {
                write!(f, "gs://{}/{}", bucket, key)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_absolute() {
        let path = SyncPath::parse("/home/user/docs").unwrap();
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("/home/user/docs"));
    }

    #[test]
    fn test_parse_local_relative() {
        let path = SyncPath::parse("./docs").unwrap();
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("./docs"));
    }

    #[test]
    fn test_parse_local_relative_no_dot() {
        let path = SyncPath::parse("docs/subdir").unwrap();
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("docs/subdir"));
    }

    #[test]
    fn test_parse_remote_with_user() {
        let path = SyncPath::parse("nick@server:/home/nick/docs").unwrap();
        assert!(path.is_remote());
        assert_eq!(path.path(), Path::new("/home/nick/docs"));
        match path {
            SyncPath::Remote { host, user, .. } => {
                assert_eq!(host, "server");
                assert_eq!(user, Some("nick".to_string()));
            }
            _ => panic!("Expected remote path"),
        }
    }

    #[test]
    fn test_parse_remote_without_user() {
        let path = SyncPath::parse("server:/home/nick/docs").unwrap();
        assert!(path.is_remote());
        assert_eq!(path.path(), Path::new("/home/nick/docs"));
        match path {
            SyncPath::Remote { host, user, .. } => {
                assert_eq!(host, "server");
                assert_eq!(user, None);
            }
            _ => panic!("Expected remote path"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_windows_drive_letter() {
        // C:/path should be treated as local, not remote
        let path = SyncPath::parse("C:/Users/nick").unwrap();
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("C:/Users/nick"));
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_windows_drive_letter_backslash() {
        // C:\path with backslashes
        let path = SyncPath::parse("C:\\Users\\nick").unwrap();
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("C:\\Users\\nick"));
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_windows_lowercase_drive() {
        // Lowercase drive letter
        let path = SyncPath::parse("d:/projects").unwrap();
        assert!(path.is_local());
        assert_eq!(path.path(), Path::new("d:/projects"));
    }

    #[test]
    fn test_parse_windows_unc_path() {
        // UNC path \\server\share\file
        let path = SyncPath::parse("\\\\server\\share\\file.txt").unwrap();
        assert!(path.is_local());
        // UNC paths should be treated as local Windows paths
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_reserved_names() {
        // Windows reserved names should still parse as local
        let path = SyncPath::parse("C:/Users/nick/CON").unwrap();
        assert!(path.is_local());

        let path = SyncPath::parse("D:/temp/NUL.txt").unwrap();
        assert!(path.is_local());

        let path = SyncPath::parse("C:/PRN").unwrap();
        assert!(path.is_local());
    }

    #[test]
    fn test_display_local() {
        let path = SyncPath::Local {
            path: PathBuf::from("/home/user/docs"),
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "/home/user/docs");
    }

    #[test]
    fn test_display_remote_with_user() {
        let path = SyncPath::Remote {
            host: "server".to_string(),
            user: Some("nick".to_string()),
            port: None,
            path: PathBuf::from("/home/nick/docs"),
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "nick@server:/home/nick/docs");
    }

    #[test]
    fn test_display_remote_without_user() {
        let path = SyncPath::Remote {
            host: "server".to_string(),
            user: None,
            port: None,
            path: PathBuf::from("/home/nick/docs"),
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "server:/home/nick/docs");
    }

    #[test]
    fn test_parse_s3_basic() {
        let path = SyncPath::parse("s3://my-bucket/path/to/file.txt").unwrap();
        assert!(path.is_s3());
        assert_eq!(path.path(), Path::new("path/to/file.txt"));
        match path {
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "path/to/file.txt");
                assert_eq!(region, None);
                assert_eq!(endpoint, None);
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_parse_s3_with_region() {
        let path = SyncPath::parse("s3://my-bucket/file.txt?region=us-west-2").unwrap();
        assert!(path.is_s3());
        match path {
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "file.txt");
                assert_eq!(region, Some("us-west-2".to_string()));
                assert_eq!(endpoint, None);
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_parse_s3_with_endpoint() {
        let path =
            SyncPath::parse("s3://my-bucket/file.txt?endpoint=https://s3.example.com").unwrap();
        assert!(path.is_s3());
        match path {
            SyncPath::S3 {
                bucket,
                key,
                region,
                endpoint,
                ..
            } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "file.txt");
                assert_eq!(region, None);
                assert_eq!(endpoint, Some("https://s3.example.com".to_string()));
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_parse_s3_bucket_only() {
        let path = SyncPath::parse("s3://my-bucket").unwrap();
        assert!(path.is_s3());
        match path {
            SyncPath::S3 { bucket, key, .. } => {
                assert_eq!(bucket, "my-bucket");
                assert_eq!(key, "");
            }
            _ => panic!("Expected S3 path"),
        }
    }

    #[test]
    fn test_display_s3() {
        let path = SyncPath::S3 {
            bucket: "my-bucket".to_string(),
            key: "path/to/file.txt".to_string(),
            region: None,
            endpoint: None,
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "s3://my-bucket/path/to/file.txt");
    }

    #[test]
    fn test_display_s3_with_region() {
        let path = SyncPath::S3 {
            bucket: "my-bucket".to_string(),
            key: "file.txt".to_string(),
            region: Some("us-west-2".to_string()),
            endpoint: None,
            has_trailing_slash: false,
        };
        assert_eq!(path.to_string(), "s3://my-bucket/file.txt?region=us-west-2");
    }

    #[test]
    fn test_display_s3_with_endpoint() {
        let path = SyncPath::S3 {
            bucket: "my-bucket".to_string(),
            key: "file.txt".to_string(),
            region: None,
            endpoint: Some("https://s3.example.com".to_string()),
            has_trailing_slash: false,
        };
        assert_eq!(
            path.to_string(),
            "s3://my-bucket/file.txt?endpoint=https://s3.example.com"
        );
    }

    fn remote(spec: &str) -> (Option<String>, String, Option<u16>, PathBuf) {
        match SyncPath::parse(spec).unwrap() {
            SyncPath::Remote {
                user,
                host,
                port,
                path,
                ..
            } => (user, host, port, path),
            other => panic!("{spec:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn ssh_url_carries_user_host_port_and_absolute_path() {
        assert_eq!(
            remote("ssh://alice@example.com:2222/srv/data/"),
            (
                Some("alice".into()),
                "example.com".into(),
                Some(2222),
                PathBuf::from("/srv/data/")
            )
        );
        assert_eq!(
            remote("ssh://example.com/srv"),
            (None, "example.com".into(), None, PathBuf::from("/srv"))
        );
        assert!(SyncPath::parse("ssh://example.com/srv/")
            .unwrap()
            .has_trailing_slash());
    }

    #[test]
    fn ipv6_literals_are_bracketed_in_both_forms() {
        assert_eq!(
            remote("ssh://[2001:db8::1]:22/srv"),
            (None, "2001:db8::1".into(), Some(22), PathBuf::from("/srv"))
        );
        assert_eq!(
            remote("root@[::1]:/srv"),
            (
                Some("root".into()),
                "::1".into(),
                None,
                PathBuf::from("/srv")
            )
        );
        assert_eq!(
            remote("[fe80::1%eth0]:backups"),
            (None, "fe80::1%eth0".into(), None, PathBuf::from("backups"))
        );
    }

    #[test]
    fn display_round_trips_remote_forms() {
        for spec in [
            "nick@server:/home/nick/docs",
            "ssh://nick@server:2222/home/nick",
            "ssh://[::1]:22/srv",
            "[::1]:/srv",
        ] {
            assert_eq!(SyncPath::parse(spec).unwrap().to_string(), spec);
        }
    }

    #[test]
    fn malformed_remote_specs_are_errors_not_local_paths() {
        for spec in [
            "ssh://host",
            "ssh://host:0/x",
            "ssh://host:99999/x",
            "ssh://host:ssh/x",
            "ssh://::1/x",
            "ssh://[::1/x",
            "ssh://@host/x",
            "ssh:///x",
            "user@-oProxyCommand=id:/x",
            "-oProxyCommand=id:/x",
            "@host:/x",
            "user@:/x",
            "user@[::1:/x",
            "bad host:/x",
        ] {
            assert!(SyncPath::parse(spec).is_err(), "{spec:?} must be rejected");
        }
    }

    #[test]
    fn colon_spellings_that_are_not_ssh_stay_local() {
        for spec in [
            "./host:path",
            "dir/file:with:colons",
            ":leading-colon",
            #[cfg(windows)]
            "C:/Users",
            "[not-a-host]/file",
            "user@host",
        ] {
            assert!(SyncPath::parse(spec).unwrap().is_local(), "{spec:?}");
        }
    }

    #[test]
    fn scp_home_shorthands_become_home_relative_paths() {
        for (spec, expected, contents) in [
            ("host:", ".", false),
            ("host:~", ".", false),
            ("host:~/", "./", true),
            ("host:~/backups/", "backups/", true),
            ("host:docs", "docs", false),
            ("host:/abs/~/x", "/abs/~/x", false),
        ] {
            let parsed = SyncPath::parse(spec).unwrap();
            assert_eq!(parsed.path(), Path::new(expected), "{spec:?}");
            assert_eq!(parsed.has_trailing_slash(), contents, "{spec:?}");
        }
        assert!(matches!(
            SyncPath::parse("host:~bob/x"),
            Err(RemoteSpecError::TildeUser(_))
        ));
        // URL paths are absolute and taken literally.
        assert_eq!(
            SyncPath::parse("ssh://host/~/x").unwrap().path(),
            Path::new("/~/x")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn single_letter_hosts_are_remote_outside_windows() {
        assert_eq!(
            remote("a:/backup"),
            (None, "a".into(), None, PathBuf::from("/backup"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn split_remote_keeps_non_utf8_path_bytes() {
        let (authority, path) = split_remote(b"host:/srv/\xff\xfe").unwrap().unwrap();
        assert_eq!(authority.host, "host");
        assert_eq!(path, b"/srv/\xff\xfe");
        assert_eq!(
            split_remote(b"h\xffst:/srv"),
            Err(RemoteSpecError::NonUtf8Authority)
        );
    }
}
