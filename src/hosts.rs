//! Parsing of `--hosts` entries into Redis connection info.
//!
//! An entry is either shorthand (`db1`, `db1:6380`) or a full Redis URL
//! (`redis://user:pw@db1:6380/2`). Shorthand takes its port from
//! `DEFAULT_PORT` and its credentials and database from the CLI defaults; a
//! URL is self-contained and the defaults are not applied to it.
//!
//! The connection info is built programmatically rather than by formatting a
//! URL string. That is deliberate: the previous `Args::redis_url` interpolated
//! the password into a URL with `format!`, so a password containing `/`, `#`,
//! `?` or `@` produced something unparseable (#42). Nothing here ever encodes
//! a credential, so nothing can mis-encode one.

use anyhow::{bail, Result};
use redis::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo, RedisConnectionInfo};

/// Port used by an entry that does not name one.
pub const DEFAULT_PORT: u16 = 6379;

/// Applied to shorthand entries. A URL entry supplies its own and ignores these.
#[derive(Debug, Clone, Default)]
pub struct HostDefaults {
    pub username: Option<String>,
    pub password: Option<String>,
    pub db: u16,
}

/// A resolved endpoint: how to display it, and how to connect to it.
#[derive(Debug, Clone)]
pub struct HostEntry {
    /// `host:port`, shown in the host column and in status messages. Never
    /// contains credentials.
    pub label: String,
    // Read by the client construction that consumes `HostEntry` starting in
    // the next commit. The transitional `parse_hosts_file` in main.rs only
    // needs `label` for now, so rustc sees this field as dead in this build.
    #[allow(dead_code)]
    pub info: ConnectionInfo,
}

/// Parse one entry. The error is a bare phrase; `parse_host_entries` supplies
/// the surrounding context so a single message reads well in a list.
pub fn parse_host_entry(entry: &str, defaults: &HostDefaults) -> Result<HostEntry, String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err("empty entry".to_string());
    }

    let info = if entry.contains("://") {
        // Validated by redis-rs's own parser, so what parses here and what
        // connects later cannot disagree.
        entry.into_connection_info().map_err(|e| e.to_string())?
    } else {
        let (host, port) = parse_shorthand(entry)?;
        let mut redis = RedisConnectionInfo::default().set_db(i64::from(defaults.db));
        if let Some(u) = &defaults.username {
            redis = redis.set_username(u);
        }
        if let Some(p) = &defaults.password {
            redis = redis.set_password(p);
        }
        ConnectionAddr::Tcp(host, port)
            .into_connection_info()
            .map_err(|e| e.to_string())?
            .set_redis_settings(redis)
    };

    // Deriving the label from the parsed address rather than from the input
    // string means credentials cannot reach it by construction.
    let label = match info.addr() {
        ConnectionAddr::Tcp(host, port) => format!("{}:{}", host, port),
        ConnectionAddr::TcpTls { .. } => {
            return Err(
                "TLS is not compiled in, so a rediss:// entry can never connect".to_string(),
            )
        }
        _ => return Err("only TCP redis:// entries are supported".to_string()),
    };

    Ok(HostEntry { label, info })
}

/// Parse every entry, reporting all the bad ones at once.
///
/// Unreachable from `main()` in this commit: `--hosts-file` validates line by
/// line through `parse_host_entry` directly, so each error can be tagged with
/// its physical line number, which this batch entry point has no way to know.
/// It is wired up to the `--hosts` flag in a later commit.
#[allow(dead_code)]
pub fn parse_host_entries(entries: &[String], defaults: &HostDefaults) -> Result<Vec<HostEntry>> {
    if entries.is_empty() {
        bail!("No hosts given");
    }

    let mut parsed = Vec::with_capacity(entries.len());
    let mut problems: Vec<String> = Vec::new();

    for entry in entries {
        match parse_host_entry(entry, defaults) {
            Ok(host) => parsed.push(host),
            Err(e) => problems.push(format!("  {}\n    {}", entry.trim(), e)),
        }
    }

    if !problems.is_empty() {
        bail!(
            "{} invalid host {}:\n{}",
            problems.len(),
            if problems.len() == 1 {
                "entry"
            } else {
                "entries"
            },
            problems.join("\n")
        );
    }

    Ok(parsed)
}

/// `host` or `host:port`.
///
/// A bare IPv6 address is refused rather than guessed at: `::1` would split on
/// its last colon into a host of `:` and a port of `1`, which is wrong and
/// would fail confusingly at connect time instead of here.
fn parse_shorthand(entry: &str) -> Result<(String, u16), String> {
    if entry.starts_with('[') || entry.matches(':').count() > 1 {
        return Err(format!(
            "looks like an IPv6 address - write it as a URL, e.g. redis://[{}]:{}",
            entry.trim_start_matches('[').trim_end_matches(']'),
            DEFAULT_PORT
        ));
    }

    let (host, port) = match entry.split_once(':') {
        None => (entry, DEFAULT_PORT),
        Some((host, port)) => {
            if host.is_empty() {
                return Err("no host before ':'".to_string());
            }
            let parsed: u16 = port
                .parse()
                .map_err(|_| format!("invalid port '{}'", port))?;
            if parsed == 0 {
                return Err("port must not be 0".to_string());
            }
            (host, parsed)
        }
    };

    if !is_valid_host(host) {
        return Err(format!("'{}' is not a valid host", host));
    }

    Ok((host.to_string(), port))
}

/// Restricts a shorthand host to the character set a real hostname or IPv4
/// address can use. Without this, a typo like `!!! not a host !!!` would be
/// accepted here and only fail later, confusingly, at connect time.
fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> HostDefaults {
        HostDefaults {
            username: Some("admin".to_string()),
            password: Some("s3cret".to_string()),
            db: 1,
        }
    }

    fn tcp(entry: &HostEntry) -> (String, u16) {
        match entry.info.addr() {
            redis::ConnectionAddr::Tcp(h, p) => (h.clone(), *p),
            other => panic!("expected Tcp, got {:?}", other),
        }
    }

    #[test]
    fn bare_host_takes_the_default_port_and_defaults() {
        let e = parse_host_entry("db1", &defaults()).unwrap();
        assert_eq!(tcp(&e), ("db1".to_string(), 6379));
        assert_eq!(e.info.redis_settings().db(), 1);
        assert_eq!(e.info.redis_settings().username(), Some("admin"));
        assert_eq!(e.info.redis_settings().password(), Some("s3cret"));
        assert_eq!(e.label, "db1:6379");
    }

    #[test]
    fn host_port_overrides_only_the_port() {
        let e = parse_host_entry("db2:6380", &defaults()).unwrap();
        assert_eq!(tcp(&e), ("db2".to_string(), 6380));
        assert_eq!(e.info.redis_settings().db(), 1);
        assert_eq!(e.label, "db2:6380");
    }

    #[test]
    fn a_url_entry_is_self_contained_and_ignores_the_defaults() {
        let e = parse_host_entry("redis://svc:pw@db3:6390/2", &defaults()).unwrap();
        assert_eq!(tcp(&e), ("db3".to_string(), 6390));
        assert_eq!(e.info.redis_settings().db(), 2);
        assert_eq!(e.info.redis_settings().username(), Some("svc"));
        assert_eq!(e.info.redis_settings().password(), Some("pw"));
    }

    // This is #42. Building the URL by string concatenation mangled these
    // characters; building ConnectionInfo directly never encodes them at all.
    #[test]
    fn a_password_with_url_metacharacters_survives_verbatim() {
        let d = HostDefaults {
            username: None,
            password: Some("p/a:s#s?w@rd".to_string()),
            db: 0,
        };
        let e = parse_host_entry("db1", &d).unwrap();
        assert_eq!(e.info.redis_settings().password(), Some("p/a:s#s?w@rd"));
    }

    #[test]
    fn the_label_never_leaks_credentials() {
        let e = parse_host_entry("redis://svc:hunter2@db3:6390/2", &defaults()).unwrap();
        assert_eq!(e.label, "db3:6390");
        assert!(!e.label.contains("hunter2"));
    }

    #[test]
    fn rediss_is_rejected_because_tls_is_not_compiled_in() {
        let err = parse_host_entry("rediss://db1:6379", &defaults()).unwrap_err();
        assert!(err.to_lowercase().contains("tls"), "got: {}", err);
    }

    #[test]
    fn an_invalid_port_is_rejected() {
        let err = parse_host_entry("db1:notaport", &defaults()).unwrap_err();
        assert!(err.contains("notaport"), "got: {}", err);
    }

    #[test]
    fn port_zero_is_rejected() {
        let err = parse_host_entry("db1:0", &defaults()).unwrap_err();
        assert!(err.contains('0'), "got: {}", err);
    }

    #[test]
    fn an_empty_entry_is_rejected() {
        assert!(parse_host_entry("   ", &defaults()).is_err());
    }

    #[test]
    fn a_url_with_no_host_is_rejected() {
        assert!(parse_host_entry("redis://", &defaults()).is_err());
    }

    #[test]
    fn outright_garbage_is_rejected() {
        assert!(parse_host_entry("!!! not a host !!!", &defaults()).is_err());
    }

    #[test]
    fn ipv6_shorthand_points_at_the_url_form() {
        let err = parse_host_entry("::1", &defaults()).unwrap_err();
        assert!(err.contains("redis://"), "got: {}", err);
    }

    #[test]
    fn every_bad_entry_is_reported_not_just_the_first() {
        let entries = vec![
            "good1".to_string(),
            "bad:one".to_string(),
            "good2".to_string(),
            "rediss://bad2".to_string(),
        ];
        let err = parse_host_entries(&entries, &defaults())
            .unwrap_err()
            .to_string();
        assert!(err.contains("bad:one"), "got: {}", err);
        assert!(err.contains("rediss://bad2"), "got: {}", err);
        assert!(err.contains('2'), "should say how many were bad: {}", err);
    }

    #[test]
    fn an_empty_list_is_rejected() {
        assert!(parse_host_entries(&[], &defaults()).is_err());
    }

    #[test]
    fn a_valid_list_keeps_its_order() {
        let entries = vec!["db1".to_string(), "db2:6380".to_string()];
        let parsed = parse_host_entries(&entries, &defaults()).unwrap();
        assert_eq!(
            parsed.iter().map(|e| e.label.clone()).collect::<Vec<_>>(),
            vec!["db1:6379".to_string(), "db2:6380".to_string()]
        );
    }
}
