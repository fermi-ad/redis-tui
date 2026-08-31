# `--hosts` and Environment Configuration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `--host`/`--port`/`--url`/`--hosts-file` with a single repeatable `--hosts` list, add `--username`, and make every option settable from the environment as `REDIS_TUI_*`.

**Architecture:** A new `src/hosts.rs` turns each `--hosts` entry into a `redis::ConnectionInfo` built programmatically, never by formatting a URL string — which is what structurally fixes #42. `RedisClient` and `MultiRedisClient` are changed to carry `ConnectionInfo` plus a display label instead of a URL string, which collapses the single-host and multi-host connection paths into one. `clap`'s `env` feature supplies the environment layer with CLI > env > default precedence for free.

**Tech Stack:** Rust 1.97 (pinned in `rust-toolchain.toml`), edition 2021, clap 4.6 (derive + env), redis 1.6, anyhow 1.

**Spec:** https://github.com/fermi-ad/redis-tui/issues/109 — the design, the decisions and their rationale live on the issue and in its comments. Read it alongside this plan.

## Global Constraints

- **This is a breaking change. Target version `2.0.0`.** Set in Task 4, not before — every PR must bump `Cargo.toml`, and CI compares against the live tip of `main`.
- **Every commit must leave CI green.** CI runs `cargo build --locked --all-targets`, `cargo build --locked --release`, `cargo test --locked`, `cargo test --locked -- --ignored`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo fmt --check`. The live tests need `redis-server` on `PATH`.
- **`clippy -D warnings` includes `dead_code`.** An item added but not yet called fails the build on the binary target even when tests use it. This is why the task order below wires each piece in as it lands rather than building bottom-up.
- **Never include Claude Code attribution or `Co-Authored-By` lines in commits.** (`CLAUDE.md`)
- **Never push directly to `main`.** Work on a feature branch; `main` requires a PR and the checks `build-and-test`, `docker-build`, `check`, `audit`.
- **TLS is not compiled in.** `rediss://` must be rejected at parse time with a clear message, never passed to a connect attempt.
- **Version tests use `env!("CARGO_PKG_VERSION")`** — never hardcode version strings.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `src/hosts.rs` | **New.** Parses one `--hosts` entry into a `HostEntry { label, info }`. Owns the shorthand grammar, the defaults, the rejections and their messages. All parsing tests live here. |
| `src/redis_client.rs` | `RedisClient` holds a `ConnectionInfo` and a label instead of a URL string. `MultiRedisClient::from_entries` replaces `from_urls` and `from_single`. `parse_db_from_url` is deleted — the db comes from `ConnectionInfo`. |
| `src/main.rs` | `Args` gains `--hosts`/`--username` and `env` attributes, loses `--host`/`--port`/`--url`/`--hosts-file`. `parse_hosts_file`, `scheme_hint` and `Args::redis_url` are deleted. Background threads take a `ConnectionInfo`. |
| `README.md`, `MANUAL.md`, `CLAUDE.md` | Documentation of the new surface. |

`src/hosts.rs` is a new fifth module. `CLAUDE.md` currently says "Four source modules in `src/`" and is updated in Task 4. The parser is ~130 lines with ~200 lines of tests; putting it in `main.rs` would push that file past 2,000 lines for logic that has one clear responsibility and no coupling to the event loop.

---

### Task 1: `src/hosts.rs` — the entry parser

Lands the parser and immediately routes the existing `--hosts-file` through it, so nothing is dead and the tree stays green. `--hosts-file` still works after this task; it is removed in Task 3.

**Files:**
- Create: `src/hosts.rs`
- Modify: `src/main.rs` — add `mod hosts;`, rewrite `parse_hosts_file` to delegate, delete `scheme_hint`
- Test: `src/hosts.rs` (`#[cfg(test)] mod tests` at the bottom, matching the codebase convention)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub const DEFAULT_PORT: u16 = 6379`
  - `pub struct HostDefaults { pub username: Option<String>, pub password: Option<String>, pub db: u16 }` — derives `Debug, Clone, Default`
  - `pub struct HostEntry { pub label: String, pub info: redis::ConnectionInfo }` — derives `Debug, Clone`
  - `pub fn parse_host_entry(entry: &str, defaults: &HostDefaults) -> Result<HostEntry, String>`
  - `pub fn parse_host_entries(entries: &[String], defaults: &HostDefaults) -> anyhow::Result<Vec<HostEntry>>`

- [ ] **Step 1: Write the failing tests**

Create `src/hosts.rs` containing only the test module for now:

```rust
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
        let err = parse_host_entries(&entries, &defaults()).unwrap_err().to_string();
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --locked hosts::`
Expected: compile failure — `cannot find type HostDefaults`, `cannot find function parse_host_entry`. The module is not declared yet either.

- [ ] **Step 3: Write the implementation**

Prepend to `src/hosts.rs`, above the test module:

```rust
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
        entry
            .into_connection_info()
            .map_err(|e| e.to_string())?
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
            if problems.len() == 1 { "entry" } else { "entries" },
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

    match entry.split_once(':') {
        None => Ok((entry.to_string(), DEFAULT_PORT)),
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
            Ok((host.to_string(), parsed))
        }
    }
}
```

- [ ] **Step 4: Declare the module and route `parse_hosts_file` through it**

In `src/main.rs`, add alongside the other `mod` declarations:

```rust
mod hosts;
```

Replace the body of `parse_hosts_file` (currently `src/main.rs:90`) so the file reader keeps its per-line reporting but delegates every entry to the new parser, and delete `scheme_hint` (currently `src/main.rs:160`) — its "did you mean redis://…?" advice is wrong now that bare `host:port` is valid input:

```rust
/// Read a hosts file, one entry per line, `#` for comments.
///
/// Temporary: `--hosts-file` is removed in favour of `--hosts` in a later
/// commit. Until then this keeps working, delegating each line to the shared
/// entry parser so there is only one grammar.
fn parse_hosts_file(path: &str, defaults: &hosts::HostDefaults) -> Result<Vec<hosts::HostEntry>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read hosts file: {}", path))?;

    let mut entries = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    for (i, line) in content.lines().enumerate() {
        let lineno = i + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match hosts::parse_host_entry(trimmed, defaults) {
            Ok(entry) => entries.push(entry),
            Err(e) => problems.push(format!("  line {}: {}\n    {}", lineno, trimmed, e)),
        }
    }

    if !problems.is_empty() {
        anyhow::bail!(
            "Hosts file '{}' has {} invalid {}:\n{}",
            path,
            problems.len(),
            if problems.len() == 1 { "entry" } else { "entries" },
            problems.join("\n")
        );
    }

    if entries.is_empty() {
        anyhow::bail!("Hosts file '{}' contains no valid URLs", path);
    }

    Ok(entries)
}
```

Update the single call site (currently `src/main.rs:179`) to build defaults from the existing flags and to convert the entries back to the `(label, url)` pairs `from_urls` still expects:

```rust
    let mut client = if let Some(ref hosts_path) = args.hosts_file {
        let defaults = hosts::HostDefaults {
            username: None,
            password: args.password.clone(),
            db: args.db,
        };
        let entries = parse_hosts_file(hosts_path, &defaults)?;
        eprintln!("Connecting to {} hosts...", entries.len());
        let urls: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.label.clone(), format!("redis://{}", e.label)))
            .collect();
        MultiRedisClient::from_urls(
            &urls,
            args.connect_retries,
            Duration::from_secs(args.connect_timeout),
        )?
    } else {
        let url = args.redis_url();
        MultiRedisClient::from_single(&url)
            .with_context(|| format!("Failed to connect to Redis at {}", url))?
    };
```

Then delete the `parse_hosts_file` tests in `src/main.rs` that assert on messages this parser no longer produces — `parse_hosts_rejects_bare_host_port_with_a_scheme_hint` (bare `host:port` is now valid) and the two label tests `parse_hosts_label_strips_scheme_auth_and_db` and `parse_hosts_label_for_a_url_without_auth` (labels are asserted in `hosts.rs` now). Keep the rest; update `parse_hosts_str` to pass `&hosts::HostDefaults::default()`.

> This step is deliberately transitional and its awkwardness — rebuilding URLs from labels — is temporary. Task 2 removes it by making the client take `HostEntry` directly. It exists so this task can land green and be reviewed on its own.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --locked && cargo test --locked -- --ignored`
Expected: PASS. The `hosts::tests` module adds 15 tests.

- [ ] **Step 6: Verify the whole gate**

Run: `cargo clippy --locked --all-targets -- -D warnings && cargo fmt --check`
Expected: clean, no `dead_code`.

- [ ] **Step 7: Commit**

```bash
git add src/hosts.rs src/main.rs
git commit -m "Add a shared host-entry parser building ConnectionInfo directly

An entry is shorthand (db1, db1:6380) or a full URL. Connection info is built
programmatically rather than by formatting a URL string, so a password
containing / # ? or @ is carried verbatim instead of being interpolated into
something unparseable - the defect in #42.

parse_hosts_file keeps its per-line reporting but delegates each line to the
parser, so there is one grammar rather than two. scheme_hint is deleted: it
advised 'did you mean redis://host:port?' for input that is now valid."
```

---

### Task 2: `ConnectionInfo` through the client, one connection path

**Files:**
- Modify: `src/redis_client.rs` — `RedisClient` fields, `connect_with_info`, `from_entries`, `info_for_key`; delete `parse_db_from_url`, `from_single`, `from_urls`, `url_for_key`, `DEFAULT_CONNECT_TIMEOUT`
- Modify: `src/main.rs` — call `from_entries`; background threads take `ConnectionInfo`
- Test: `src/redis_client.rs`

**Interfaces:**
- Consumes: `hosts::HostEntry`, `hosts::HostDefaults`, `hosts::parse_host_entry` from Task 1.
- Produces:
  - `RedisClient { connection: redis::Connection, pub label: String, pub db: i64 }`
  - `pub fn RedisClient::connect_with_info(info: redis::ConnectionInfo, label: &str, timeout: std::time::Duration) -> Result<Self>`
  - `pub fn RedisClient::connect(url: &str) -> Result<Self>` — retained for tests and background threads; delegates via `into_connection_info()` with a 10s timeout
  - `pub fn MultiRedisClient::from_entries(entries: &[hosts::HostEntry], max_retries: u32, connect_timeout: std::time::Duration) -> Result<Self>`
  - `pub fn MultiRedisClient::info_for_key(&self, key: &str) -> redis::ConnectionInfo`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `src/redis_client.rs`:

```rust
    /// #42: a password containing URL metacharacters must actually authenticate.
    /// Building a URL string mangled these; ConnectionInfo carries them verbatim.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn connects_with_a_password_containing_url_metacharacters() {
        let server = TestRedis::start();
        let password = "p/a:s#s?w@rd";

        let mut admin = RedisClient::connect(&server.url()).unwrap();
        redis::cmd("CONFIG")
            .arg("SET")
            .arg("requirepass")
            .arg(password)
            .exec(&mut admin.connection)
            .unwrap();
        drop(admin);

        let defaults = crate::hosts::HostDefaults {
            username: None,
            password: Some(password.to_string()),
            db: 0,
        };
        let entry =
            crate::hosts::parse_host_entry(&format!("127.0.0.1:{}", server.port), &defaults)
                .unwrap();

        let mut client = RedisClient::connect_with_info(
            entry.info,
            &entry.label,
            std::time::Duration::from_secs(3),
        )
        .expect("a password with / : # ? @ in it must authenticate");

        let pong: String = redis::cmd("PING").query(&mut client.connection).unwrap();
        assert_eq!(pong, "PONG");
    }

    /// The db comes from the parsed connection info, not from re-parsing a URL.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn from_entries_reports_the_db_from_the_entry() {
        let server = TestRedis::start();
        let defaults = crate::hosts::HostDefaults {
            username: None,
            password: None,
            db: 3,
        };
        let entry =
            crate::hosts::parse_host_entry(&format!("127.0.0.1:{}", server.port), &defaults)
                .unwrap();

        let multi = MultiRedisClient::from_entries(
            std::slice::from_ref(&entry),
            0,
            std::time::Duration::from_secs(3),
        )
        .unwrap();

        assert_eq!(multi.db, 3);
        assert_eq!(multi.host_count(), 1);
    }
```

`TestRedis` needs its port readable by the test; if `port` is private, make it `pub(crate)` or add an accessor in this step.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --locked -- --ignored connects_with_a_password`
Expected: compile failure — `no function connect_with_info`, `no function from_entries`.

- [ ] **Step 3: Change `RedisClient` to carry connection info**

In `src/redis_client.rs`, replace the struct's `url` field with `label`, delete `parse_db_from_url` and its eight tests, delete `DEFAULT_CONNECT_TIMEOUT`, and replace `connect`/`connect_with_timeout`:

```rust
pub struct RedisClient {
    connection: redis::Connection,
    /// `host:port`. Display only, and never contains credentials.
    pub label: String,
    pub db: i64,
}

impl RedisClient {
    /// Connect using a URL. Retained for the background threads and the live
    /// tests, both of which have a URL and no defaults to apply.
    pub fn connect(url: &str) -> Result<Self> {
        let info = url
            .into_connection_info()
            .with_context(|| format!("Failed to parse Redis URL {}", url))?;
        let label = match info.addr() {
            redis::ConnectionAddr::Tcp(h, p) => format!("{}:{}", h, p),
            _ => url.to_string(),
        };
        Self::connect_with_info(info, &label, std::time::Duration::from_secs(10))
    }

    /// Connect with an explicit socket timeout, so the startup retry budget is
    /// actually bounded: a host that black-holes packets costs one timeout per
    /// attempt on top of the sleep.
    pub fn connect_with_info(
        info: redis::ConnectionInfo,
        label: &str,
        timeout: std::time::Duration,
    ) -> Result<Self> {
        let db = info.redis_settings().db();
        let client = redis::Client::open(info)
            .with_context(|| format!("Failed to create Redis client for {}", label))?;
        let connection = client
            .get_connection_with_timeout(timeout)
            .with_context(|| format!("Failed to connect to {}", label))?;

        Ok(Self {
            connection,
            label: label.to_string(),
            db,
        })
    }
```

Add `use redis::IntoConnectionInfo;` to the imports at the top of the file.

- [ ] **Step 4: Replace `from_single` and `from_urls` with `from_entries`**

Delete both, and add `from_entries` with the same retry structure `from_urls` had — first pass, bounded retries, then drop whatever never came up:

```rust
    /// Connect to every entry, retrying the ones that are not up yet.
    ///
    /// `max_retries` and `connect_timeout` are caller-supplied so startup
    /// cannot stall indefinitely. Entries are already known to be well-formed:
    /// `hosts::parse_host_entry` rejects unparseable ones before this is
    /// reached, so every retry is against a host that could plausibly come up.
    pub fn from_entries(
        entries: &[crate::hosts::HostEntry],
        max_retries: u32,
        connect_timeout: std::time::Duration,
    ) -> Result<Self> {
        let mut clients: Vec<Option<RedisClient>> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        let retry_delay = std::time::Duration::from_secs(2);

        let mut pending: Vec<usize> = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            labels.push(entry.label.clone());
            match RedisClient::connect_with_info(
                entry.info.clone(),
                &entry.label,
                connect_timeout,
            ) {
                Ok(client) => clients.push(Some(client)),
                Err(_) => {
                    clients.push(None);
                    pending.push(i);
                }
            }
        }

        let mut attempt: u32 = 0;
        while !pending.is_empty() && attempt < max_retries {
            attempt += 1;
            eprintln!(
                "Retrying {} host(s) (attempt {}/{})...",
                pending.len(),
                attempt,
                max_retries
            );
            std::thread::sleep(retry_delay);

            pending.retain(|&i| {
                match RedisClient::connect_with_info(
                    entries[i].info.clone(),
                    &entries[i].label,
                    connect_timeout,
                ) {
                    Ok(client) => {
                        eprintln!("  Connected to {}", entries[i].label);
                        clients[i] = Some(client);
                        false
                    }
                    Err(_) => true,
                }
            });
        }

        let mut final_clients = Vec::new();
        let mut final_labels = Vec::new();
        let mut failed = Vec::new();
        for (i, opt) in clients.into_iter().enumerate() {
            match opt {
                Some(client) => {
                    final_clients.push(client);
                    final_labels.push(labels[i].clone());
                }
                None => failed.push(labels[i].clone()),
            }
        }

        if !failed.is_empty() {
            eprintln!(
                "Warning: {} host(s) never came up and are absent from this session: {}",
                failed.len(),
                failed.join(", ")
            );
            eprintln!(
                "         Their keys will not appear. Raise --connect-retries or --connect-timeout if they were merely slow to start."
            );
        }

        if final_clients.is_empty() {
            anyhow::bail!("Could not connect to any of the {} host(s) given", entries.len());
        }

        let db = final_clients[0].db;
        Ok(Self {
            labels: final_labels,
            clients: final_clients,
            key_owner: HashMap::new(),
            collisions: Vec::new(),
            db,
        })
    }

    /// Connection info for the host owning `key`, for a background thread to
    /// open its own connection with.
    pub fn info_for_key(&self, key: &str) -> redis::ConnectionInfo {
        let idx = self.key_owner.get(key).copied().unwrap_or(0);
        self.clients[idx].connection_info()
    }
```

`RedisClient` must be able to hand back its `ConnectionInfo`. Store it alongside the connection — add a `info: redis::ConnectionInfo` field set in `connect_with_info`, and:

```rust
    pub fn connection_info(&self) -> redis::ConnectionInfo {
        self.info.clone()
    }
```

Replace the two `&self.clients[..].url` reads (currently `src/redis_client.rs:829` and `:835`) with `.label`.

- [ ] **Step 5: Wire `main.rs` to the new API**

Replace the connection block so both branches produce `Vec<HostEntry>` and there is one connect call:

```rust
    let defaults = hosts::HostDefaults {
        username: None,
        password: args.password.clone(),
        db: args.db,
    };
    let entries = if let Some(ref hosts_path) = args.hosts_file {
        parse_hosts_file(hosts_path, &defaults)?
    } else {
        let entry = hosts::parse_host_entry(&format!("{}:{}", args.host, args.port), &defaults)
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        vec![entry]
    };
    eprintln!("Connecting to {} host(s)...", entries.len());
    let mut client = MultiRedisClient::from_entries(
        &entries,
        args.connect_retries,
        Duration::from_secs(args.connect_timeout),
    )?;
```

`Args::redis_url` and the `--url` flag are now unused — leave both in place until Task 3, but mark the flag's help text so nothing is silently misleading:

```rust
    /// Full Redis URL. Deprecated: pass it to --hosts instead.
    #[arg(short, long)]
    url: Option<String>,
```

If `redis_url` is now dead, delete it in this step rather than adding an `allow` — `clippy -D warnings` will fail otherwise.

Change the two background-thread constructors to take a `ConnectionInfo`. In `StreamListener::start` (currently `src/main.rs:256`) and `SignalGenerator::start` (`:325`):

```rust
    fn start(
        info: redis::ConnectionInfo,
        label: &str,
        key: &str,
        last_id: &str,
        db: i64,
    ) -> Option<Self> {
        let mut client =
            RedisClient::connect_with_info(info, label, std::time::Duration::from_secs(10)).ok()?;
```

and update the two call sites (`:534` and `:614`) to pass `client.info_for_key(&k)` and the owning label instead of `client.url_for_key(&k)`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --locked && cargo test --locked -- --ignored`
Expected: PASS, including the two new live tests. Live test count rises from 10 to 12.

- [ ] **Step 7: Verify the whole gate**

Run: `cargo clippy --locked --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add src/redis_client.rs src/main.rs
git commit -m "Carry ConnectionInfo through the client and collapse to one connect path

RedisClient holds parsed connection info and a host:port label instead of a URL
string, so the database number comes from the parsed info rather than from
re-parsing a URL - parse_db_from_url and its eight tests are deleted.

from_single and from_urls become from_entries. There is now one connection path
for one host and for many, which also means --connect-timeout applies to the
single-host case; it previously went through RedisClient::connect and silently
used a hardcoded 10s constant instead.

Background threads take a ConnectionInfo from info_for_key rather than
rebuilding a URL, so credentials never round-trip through a string."
```

---

### Task 3: The new flag surface and the environment

**Files:**
- Modify: `Cargo.toml` — clap `env` feature
- Modify: `src/main.rs` — `Args`, delete `parse_hosts_file` and `Args::redis_url`
- Test: `src/main.rs`

**Interfaces:**
- Consumes: `hosts::parse_host_entries`, `MultiRedisClient::from_entries`.
- Produces: `Args { hosts: Vec<String>, username: Option<String>, password: Option<String>, db: u16, rate_history: u64, rate_avg_window: u64, connect_retries: u32, connect_timeout: u64 }`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` in `src/main.rs`:

```rust
    // Environment variables are process-global and cargo runs tests as threads,
    // so these drive the parser with an explicit argv rather than mutating the
    // real environment, which would make them order-dependent.
    #[test]
    fn hosts_defaults_to_localhost() {
        let a = Args::try_parse_from(["redis-tui"]).unwrap();
        assert_eq!(a.hosts, vec!["127.0.0.1:6379".to_string()]);
        assert_eq!(a.db, 0);
        assert_eq!(a.username, None);
    }

    #[test]
    fn hosts_accepts_several_space_separated_values() {
        let a = Args::try_parse_from(["redis-tui", "--hosts", "db1", "db2:6380"]).unwrap();
        assert_eq!(a.hosts, vec!["db1".to_string(), "db2:6380".to_string()]);
    }

    #[test]
    fn hosts_is_repeatable() {
        let a =
            Args::try_parse_from(["redis-tui", "--hosts", "db1", "--hosts", "db2"]).unwrap();
        assert_eq!(a.hosts, vec!["db1".to_string(), "db2".to_string()]);
    }

    #[test]
    fn a_following_flag_is_not_swallowed_by_the_hosts_list() {
        let a =
            Args::try_parse_from(["redis-tui", "--hosts", "db1", "db2", "--db", "4"]).unwrap();
        assert_eq!(a.hosts, vec!["db1".to_string(), "db2".to_string()]);
        assert_eq!(a.db, 4);
    }

    #[test]
    fn the_removed_flags_are_rejected() {
        for flag in ["--host", "--port", "--url", "--hosts-file"] {
            assert!(
                Args::try_parse_from(["redis-tui", flag, "x"]).is_err(),
                "{} should no longer be accepted",
                flag
            );
        }
    }

    #[test]
    fn username_and_password_are_accepted() {
        let a = Args::try_parse_from([
            "redis-tui", "--hosts", "db1", "--username", "admin", "--password", "s3cret",
        ])
        .unwrap();
        assert_eq!(a.username.as_deref(), Some("admin"));
        assert_eq!(a.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn numeric_ranges_are_still_enforced() {
        assert!(Args::try_parse_from(["redis-tui", "--connect-timeout", "0"]).is_err());
        assert!(Args::try_parse_from(["redis-tui", "--rate-history", "0"]).is_err());
        assert!(Args::try_parse_from(["redis-tui", "--rate-avg-window", "0"]).is_err());
    }

    #[test]
    fn help_advertises_the_environment_variables() {
        let help = Args::command().render_help().to_string();
        for var in [
            "REDIS_TUI_HOSTS",
            "REDIS_TUI_USERNAME",
            "REDIS_TUI_PASSWORD",
            "REDIS_TUI_DB",
            "REDIS_TUI_RATE_HISTORY",
            "REDIS_TUI_RATE_AVG_WINDOW",
            "REDIS_TUI_CONNECT_RETRIES",
            "REDIS_TUI_CONNECT_TIMEOUT",
        ] {
            assert!(help.contains(var), "--help should mention {}", var);
        }
    }
```

Add `use clap::CommandFactory;` to that test module for `Args::command()`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --locked hosts_defaults_to_localhost the_removed_flags_are_rejected`
Expected: FAIL — `--hosts` is not a recognised flag and `no field hosts on Args`.

- [ ] **Step 3: Enable clap's `env` feature**

In `Cargo.toml`:

```toml
clap = { version = "4", features = ["derive", "env"] }
```

- [ ] **Step 4: Replace the `Args` struct**

In `src/main.rs`, replace the whole struct (currently `src/main.rs:32`) and delete `impl Args`'s `redis_url`:

```rust
#[derive(Parser, Debug)]
#[command(
    name = "redis-tui",
    about = "A Redis TUI client inspired by Redis Insight"
)]
struct Args {
    /// Redis hosts: `host`, `host:port`, or a full `redis://` URL.
    /// Repeatable, and several may be given at once.
    #[arg(
        long,
        num_args = 1..,
        action = clap::ArgAction::Append,
        value_delimiter = ' ',
        default_value = "127.0.0.1:6379",
        env = "REDIS_TUI_HOSTS"
    )]
    hosts: Vec<String>,

    /// Username for hosts that do not carry their own
    #[arg(long, env = "REDIS_TUI_USERNAME")]
    username: Option<String>,

    /// Password for hosts that do not carry their own
    #[arg(long, env = "REDIS_TUI_PASSWORD")]
    password: Option<String>,

    /// Database number for hosts that do not carry their own
    #[arg(short, long, default_value_t = 0, env = "REDIS_TUI_DB")]
    db: u16,

    /// Rolling window for ingestion rate chart history (minutes)
    #[arg(long, default_value_t = 20, env = "REDIS_TUI_RATE_HISTORY",
          value_parser = clap::value_parser!(u64).range(1..))]
    rate_history: u64,

    /// Sliding window for the plotted ingestion rate line (seconds)
    #[arg(long, default_value_t = 2, env = "REDIS_TUI_RATE_AVG_WINDOW",
          value_parser = clap::value_parser!(u64).range(1..))]
    rate_avg_window: u64,

    /// Retry attempts for a host that is not up yet
    #[arg(long, default_value_t = 5, env = "REDIS_TUI_CONNECT_RETRIES",
          value_parser = clap::value_parser!(u32).range(0..))]
    connect_retries: u32,

    /// Socket timeout per connection attempt (seconds)
    #[arg(long, default_value_t = 3, env = "REDIS_TUI_CONNECT_TIMEOUT",
          value_parser = clap::value_parser!(u64).range(1..))]
    connect_timeout: u64,
}
```

- [ ] **Step 5: Delete `parse_hosts_file` and wire `main()` to the list**

Delete `parse_hosts_file` and every test of it that remains in `src/main.rs` — `hosts.rs` covers the grammar, and there is no file to read any more. Replace the connection block:

```rust
    let defaults = hosts::HostDefaults {
        username: args.username.clone(),
        password: args.password.clone(),
        db: args.db,
    };
    let entries = hosts::parse_host_entries(&args.hosts, &defaults)?;
    eprintln!("Connecting to {} host(s)...", entries.len());
    let mut client = MultiRedisClient::from_entries(
        &entries,
        args.connect_retries,
        Duration::from_secs(args.connect_timeout),
    )?;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --locked && cargo test --locked -- --ignored`
Expected: PASS.

- [ ] **Step 7: Verify the whole gate and the real binary**

Run:
```bash
cargo clippy --locked --all-targets -- -D warnings && cargo fmt --check
cargo run -- --help
REDIS_TUI_HOSTS="db1 db2:6380" cargo run -- --help
```
Expected: clippy and fmt clean. `--help` shows `--hosts`, `--username`, and `[env: REDIS_TUI_*]` annotations, and no `--host`, `--port`, `--url` or `--hosts-file`.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml src/main.rs
git commit -m "Replace --host/--port/--url/--hosts-file with --hosts, and read the environment

One flag now names where to connect. --hosts takes one or more space-separated
entries and is repeatable; an entry is a bare host, host:port, or a full URL.
--username is new, since Redis 6 ACLs mean a password alone is not enough.

Every option is also settable as REDIS_TUI_*, via clap's env feature, which
gives CLI > environment > default precedence and prints the variable names in
--help so the documentation cannot drift from the code.

This removes two silent behaviours rather than patching them. --hosts-file used
to win over --host/--port/--password/--db/--url with no warning and no mention
in the docs; and with one connection path there is no second source of truth to
override anything."
```

---

### Task 4: Documentation and the 2.0.0 release

**Files:**
- Modify: `README.md`, `MANUAL.md`, `CLAUDE.md`, `Cargo.toml`, `start-dev.sh`

- [ ] **Step 1: Update `MANUAL.md`**

Replace the options table (currently `MANUAL.md:26-37`) with the eight options and their variables, and rewrite the examples at `MANUAL.md:47-56`, the connection paragraph at `:65`, the hosts-file section at `:80`, and the multi-host note at `:437`:

```markdown
| Flag | Environment | Description | Default |
|------|-------------|-------------|---------|
| `--hosts <HOSTS>...` | `REDIS_TUI_HOSTS` | Hosts: `host`, `host:port`, or a full `redis://` URL. Repeatable. | `127.0.0.1:6379` |
| `--username <USERNAME>` | `REDIS_TUI_USERNAME` | Username for hosts without their own | None |
| `--password <PASSWORD>` | `REDIS_TUI_PASSWORD` | Password for hosts without their own | None |
| `-d, --db <DB>` | `REDIS_TUI_DB` | Database for hosts without their own | `0` |
| `--rate-history <MINUTES>` | `REDIS_TUI_RATE_HISTORY` | Rolling window for the rate chart | `20` |
| `--rate-avg-window <SECONDS>` | `REDIS_TUI_RATE_AVG_WINDOW` | Sliding average for the plotted rate line | `2` |
| `--connect-retries <N>` | `REDIS_TUI_CONNECT_RETRIES` | Retries for a host that is not up yet | `5` |
| `--connect-timeout <SECONDS>` | `REDIS_TUI_CONNECT_TIMEOUT` | Socket timeout per attempt | `3` |
```

Add a migration section:

```markdown
### Migrating from 1.x

`--host`, `--port`, `--url` and `--hosts-file` were removed in 2.0.0. One flag
now names where to connect.

| 1.x | 2.0 |
|-----|-----|
| `--host db1 --port 6380` | `--hosts db1:6380` |
| `--url redis://:pw@db1/2` | `--hosts redis://:pw@db1/2` |
| `--hosts-file hosts.txt` | `--hosts $(grep -v '^#' hosts.txt \| tr '\n' ' ')` |

Entries mix freely, and `--username`/`--password`/`--db` apply to any entry that
does not carry its own:

    redis-tui --hosts db1 db2:6380 redis://svc:pw@db3/2 \
              --username admin --password s3cret --db 1
```

- [ ] **Step 2: Update `README.md`**

Rewrite the usage examples at `README.md:76-88` and the Docker example at `:50` — `docker run -it --rm redis-tui --host <redis-host>` becomes `--hosts <redis-host>`. Add a short environment example, since it is the reason the image is easier to use now:

```markdown
Every option can be set from the environment instead:

```bash
docker run -it --rm --network host \
  -e REDIS_TUI_HOSTS="db1 db2:6380" \
  -e REDIS_TUI_DB=1 \
  adregistry.fnal.gov/instrumentation/redis-tui
```
```

- [ ] **Step 3: Update `CLAUDE.md`**

- "Four source modules in `src/`" becomes five, with a line for `hosts.rs`.
- Under Conventions, record: *"One flag names where to connect. Connection info is built as `redis::ConnectionInfo`, never by formatting a URL string — a credential interpolated into a URL breaks on `/ # ? @`."*

- [ ] **Step 4: Update `start-dev.sh`**

Its final line passes `--hosts-file`. Replace with the list form:

```bash
cargo run -- --hosts "127.0.0.1:${REDIS_PORT_1}" "127.0.0.1:${REDIS_PORT_2}"
```

and delete the `HOSTS_FILE` variable and the block that writes it.

- [ ] **Step 5: Bump to 2.0.0**

In `Cargo.toml`, set `version = "2.0.0"`, then run `cargo build` to refresh `Cargo.lock`.

- [ ] **Step 6: Verify everything**

Run:
```bash
cargo build --locked --all-targets && cargo build --locked --release
cargo test --locked && cargo test --locked -- --ignored
cargo clippy --locked --all-targets -- -D warnings && cargo fmt --check
grep -rn -- '--host \|--port \|--url \|--hosts-file' README.md MANUAL.md start-dev.sh
```
Expected: all green, and the final `grep` finds only the migration table in `MANUAL.md`.

- [ ] **Step 7: Commit and open the PR**

```bash
git add README.md MANUAL.md CLAUDE.md start-dev.sh Cargo.toml Cargo.lock
git commit -m "Document the 2.0 connection surface and bump to 2.0.0

--host, --port, --url and --hosts-file are gone; MANUAL.md carries a migration
table for each. Every option is documented alongside its REDIS_TUI_ variable,
and start-dev.sh uses the list form.

Breaking, so 1.3.x -> 2.0.0."
git push -u origin <branch>
gh pr create --title "Replace the connection flags with --hosts and add environment configuration (#109)"
```

---

## Self-Review

**Spec coverage.** Every decision recorded on #109 maps to a task: clean break and removals (Task 3), space-separated repeatable `--hosts` (Task 3), the three entry forms and their defaults (Task 1), `--username` (Task 3), URL entries self-contained (Task 1), the eight `REDIS_TUI_*` variables (Task 3), #42 fixed as part of the work (Tasks 1 and 2), `parse_hosts_file`'s validation and label derivation preserved in the entry parser (Task 1), documentation (Task 4).

**Deliberately out of scope**, both recorded on #109 rather than silently dropped:
- `REDIS_TUI_PASSWORD_FILE` for mounted secrets. Purely additive and can land later without another breaking change.
- The socket *read* timeout, which is the larger half of #44. This work fixes only the connect-timeout half, by removing the path that ignored the flag.

**Known gaps to raise rather than paper over.**
- Background threads still connect with a hardcoded 10s timeout inside `RedisClient::connect`. Threading the configured value through `StreamListener`/`SignalGenerator` is a change to their signatures and their call sites, and belongs with #44 rather than here. Task 2's comment should say so.
- `MultiRedisClient::info_for_key` falls back to host 0 for an unknown key, matching what `url_for_key` did. That is existing behaviour, not a new decision.

**Type consistency.** `HostEntry`/`HostDefaults`/`parse_host_entry`/`parse_host_entries` are used with the same names and signatures in Tasks 1, 2 and 3. `connect_with_info(info, label, timeout)` has the same argument order everywhere. `from_entries(entries, max_retries, connect_timeout)` matches the `from_urls` signature it replaces.

**One risk worth stating.** Task 2 changes `RedisClient`'s public shape while `app.rs` and `ui.rs` also read from it. Step 4 lists the two `.url` reads found by `grep -n '\.url\b' src/`, but the implementer should re-run that grep across all of `src/` before assuming the list is complete.
