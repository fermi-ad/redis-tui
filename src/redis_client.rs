use anyhow::{Context, Result};
use redis::{Commands, ConnectionLike};
use std::collections::HashMap;

/// Information about a Redis key
#[derive(Debug, Clone)]
pub struct KeyInfo {
    pub name: String,
    pub key_type: String,
    pub ttl: i64,
    pub size: i64,
    pub encoding: String,
}

/// A single stream entry
#[derive(Debug, Clone)]
pub struct StreamEntry {
    pub id: String,
    pub fields: Vec<(String, Vec<u8>)>,
}

/// The value of a Redis key, typed by its Redis data type
#[derive(Debug, Clone)]
pub enum RedisValue {
    String(Vec<u8>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    ZSet(Vec<(Vec<u8>, f64)>),
    Hash(Vec<(String, Vec<u8>)>),
    Stream(Vec<StreamEntry>),
    Unknown(String),
}

#[allow(dead_code)]
pub struct RedisClient {
    connection: redis::Connection,
    pub url: String,
    pub db: i64,
}

/// Extract the database number from a Redis URL, e.g. `redis://host:6379/3` -> 3.
///
/// Returns 0 when the URL carries no usable database component. Query strings and
/// fragments are stripped first, so `redis://host/3?timeout=5` yields 3 rather than
/// silently falling back to 0, and a trailing slash (`redis://host/3/`) is tolerated.
/// Socket timeout used by `RedisClient::connect`. Multi-host startup overrides
/// this via `connect_with_timeout` so its retry budget stays bounded.
const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn parse_db_from_url(url: &str) -> i64 {
    // Strip `?query` and `#fragment` before looking at the path.
    let trimmed = url.split(['?', '#']).next().unwrap_or(url);

    // Skip past `scheme://` so the authority's own separators aren't read as a path.
    let after_scheme = match trimmed.find("://") {
        Some(i) => &trimmed[i + 3..],
        None => trimmed,
    };

    // The path begins at the first '/' after the authority. No '/' means no db given.
    let Some((_, path)) = after_scheme.split_once('/') else {
        return 0;
    };

    // Take the last *non-empty* segment: a trailing slash ("/3/") would otherwise
    // yield an empty final segment and look like no db was given at all.
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|db| *db >= 0)
        .unwrap_or(0)
}

/// Validate a TTL before any command is sent to Redis.
///
/// Redis treats `EXPIRE key 0` as "expire immediately", which deletes the key. That is
/// almost never what someone typing 0 into a TTL field means, so it is refused here
/// rather than reinterpreted - no command reaches Redis.
///
/// Any negative TTL means persist (see `set_ttl`), which is what the edit field's
/// "empty=persist" path sends as -1. The message says "negative" rather than naming -1
/// so it stays true to the behaviour callers actually get.
fn validate_ttl(ttl: i64, key: &str) -> Result<()> {
    if ttl == 0 {
        anyhow::bail!(
            "TTL 0 would delete '{}'. Leave the field empty (or use a negative TTL) to persist, \
             or delete the key explicitly.",
            key
        );
    }
    Ok(())
}

impl RedisClient {
    pub fn connect(url: &str) -> Result<Self> {
        Self::connect_with_timeout(url, DEFAULT_CONNECT_TIMEOUT)
    }

    /// Connect with an explicit socket timeout. `from_urls` uses this so the
    /// startup retry budget is actually bounded: with the fixed 10s timeout, a
    /// host that black-holes packets cost 10s per attempt on top of the sleep.
    pub fn connect_with_timeout(url: &str, timeout: std::time::Duration) -> Result<Self> {
        let client = redis::Client::open(url)
            .with_context(|| format!("Failed to create Redis client for {}", url))?;
        let connection = client
            .get_connection_with_timeout(timeout)
            .with_context(|| format!("Failed to connect to {}", url))?;

        // Parse db number from URL (e.g., redis://host:port/3)
        let db = parse_db_from_url(url);

        Ok(Self {
            connection,
            url: url.to_string(),
            db,
        })
    }

    pub fn select_db(&mut self, db: i64) -> Result<()> {
        redis::cmd("SELECT")
            .arg(db)
            .exec(&mut self.connection)
            .with_context(|| format!("Failed to SELECT db {}", db))?;
        self.db = db;
        Ok(())
    }

    /// Scan keys matching `pattern`.
    ///
    /// Returns the decodable keys plus a count of keys that could not be read back as
    /// UTF-8 strings. Those are unavoidably invisible in a text UI, but the count lets
    /// the caller say so instead of silently showing fewer keys than Redis holds.
    pub fn scan_keys(&mut self, pattern: &str) -> Result<(Vec<String>, usize)> {
        let iter: redis::Iter<String> = redis::cmd("SCAN")
            .cursor_arg(0)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(1000)
            .clone()
            .iter(&mut self.connection)
            .context("Failed to SCAN keys")?;

        let mut keys: Vec<String> = Vec::new();
        let mut skipped = 0usize;
        for result in iter {
            match result {
                Ok(key) => keys.push(key),
                // Almost always a non-UTF-8 key name, which a text UI cannot display.
                Err(_) => skipped += 1,
            }
        }
        keys.sort();
        Ok((keys, skipped))
    }

    pub fn get_key_info(&mut self, key: &str) -> Result<KeyInfo> {
        let key_type: String = redis::cmd("TYPE")
            .arg(key)
            .query(&mut self.connection)
            .unwrap_or_else(|_| "unknown".to_string());

        let ttl: i64 = self.connection.ttl(key).unwrap_or(-2);

        let size: i64 = redis::cmd("MEMORY")
            .arg("USAGE")
            .arg(key)
            .query(&mut self.connection)
            .unwrap_or(-1);

        let encoding: String = redis::cmd("OBJECT")
            .arg("ENCODING")
            .arg(key)
            .query(&mut self.connection)
            .unwrap_or_else(|_| "unknown".to_string());

        Ok(KeyInfo {
            name: key.to_string(),
            key_type,
            ttl,
            size,
            encoding,
        })
    }

    pub fn get_value(&mut self, key: &str) -> Result<RedisValue> {
        let key_type: String = redis::cmd("TYPE")
            .arg(key)
            .query(&mut self.connection)
            .unwrap_or_else(|_| "unknown".to_string());

        match key_type.as_str() {
            "string" => {
                let val: Vec<u8> = self.connection.get(key).context("Failed to GET")?;
                Ok(RedisValue::String(val))
            }
            "list" => {
                let vals: Vec<Vec<u8>> = self
                    .connection
                    .lrange(key, 0, -1)
                    .context("Failed to LRANGE")?;
                Ok(RedisValue::List(vals))
            }
            "set" => {
                let vals: Vec<Vec<u8>> = self
                    .connection
                    .smembers(key)
                    .context("Failed to SMEMBERS")?;
                Ok(RedisValue::Set(vals))
            }
            "zset" => {
                let vals: Vec<(Vec<u8>, f64)> = self
                    .connection
                    .zrange_withscores(key, 0, -1)
                    .context("Failed to ZRANGEBYSCORE")?;
                Ok(RedisValue::ZSet(vals))
            }
            "hash" => {
                let map: HashMap<String, Vec<u8>> =
                    self.connection.hgetall(key).context("Failed to HGETALL")?;
                let mut pairs: Vec<(String, Vec<u8>)> = map.into_iter().collect();
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(RedisValue::Hash(pairs))
            }
            "stream" => {
                let entries = self.get_stream_entries(key)?;
                Ok(RedisValue::Stream(entries))
            }
            other => Ok(RedisValue::Unknown(format!("Unsupported type: {}", other))),
        }
    }

    pub fn get_stream_entries(&mut self, key: &str) -> Result<Vec<StreamEntry>> {
        // XRANGE key - + COUNT 500
        let raw: Vec<redis::Value> = redis::cmd("XRANGE")
            .arg(key)
            .arg("-")
            .arg("+")
            .arg("COUNT")
            .arg(500)
            .query(&mut self.connection)
            .context("Failed to XRANGE")?;

        let mut entries = Vec::new();
        for entry_val in raw {
            if let redis::Value::Array(parts) = entry_val {
                if parts.len() >= 2 {
                    let id = match &parts[0] {
                        redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                        _ => continue,
                    };

                    let mut fields = Vec::new();
                    if let redis::Value::Array(field_vals) = &parts[1] {
                        let mut i = 0;
                        while i + 1 < field_vals.len() {
                            let fname = match &field_vals[i] {
                                redis::Value::BulkString(b) => {
                                    String::from_utf8_lossy(b).to_string()
                                }
                                _ => {
                                    i += 2;
                                    continue;
                                }
                            };
                            let fval = match &field_vals[i + 1] {
                                redis::Value::BulkString(b) => b.clone(),
                                _ => Vec::new(),
                            };
                            fields.push((fname, fval));
                            i += 2;
                        }
                    }

                    entries.push(StreamEntry { id, fields });
                }
            }
        }

        Ok(entries)
    }

    /// Blocking XREAD for new entries after `last_id`.
    /// Blocks up to `timeout_ms` milliseconds (0 = forever).
    /// Returns new entries (empty vec if timeout).
    pub fn xread_blocking(
        &mut self,
        key: &str,
        last_id: &str,
        timeout_ms: u64,
    ) -> Result<Vec<StreamEntry>> {
        let raw: redis::Value = redis::cmd("XREAD")
            .arg("BLOCK")
            .arg(timeout_ms)
            .arg("COUNT")
            .arg(100)
            .arg("STREAMS")
            .arg(key)
            .arg(last_id)
            .query(&mut self.connection)
            .context("Failed to XREAD")?;

        // XREAD returns: nil if no data, or array of [key, [[id, [field, val, ...]], ...]]
        let streams = match raw {
            redis::Value::Array(s) => s,
            redis::Value::Nil => return Ok(Vec::new()),
            _ => return Ok(Vec::new()),
        };

        let mut entries = Vec::new();
        for stream_val in streams {
            if let redis::Value::Array(parts) = stream_val {
                if parts.len() >= 2 {
                    // parts[0] = stream key, parts[1] = array of entries
                    if let redis::Value::Array(entry_list) = &parts[1] {
                        for entry_val in entry_list {
                            if let redis::Value::Array(ep) = entry_val {
                                if ep.len() >= 2 {
                                    let id = match &ep[0] {
                                        redis::Value::BulkString(b) => {
                                            String::from_utf8_lossy(b).to_string()
                                        }
                                        _ => continue,
                                    };
                                    let mut fields = Vec::new();
                                    if let redis::Value::Array(fv) = &ep[1] {
                                        let mut i = 0;
                                        while i + 1 < fv.len() {
                                            let fname = match &fv[i] {
                                                redis::Value::BulkString(b) => {
                                                    String::from_utf8_lossy(b).to_string()
                                                }
                                                _ => {
                                                    i += 2;
                                                    continue;
                                                }
                                            };
                                            let fval = match &fv[i + 1] {
                                                redis::Value::BulkString(b) => b.clone(),
                                                _ => Vec::new(),
                                            };
                                            fields.push((fname, fval));
                                            i += 2;
                                        }
                                    }
                                    entries.push(StreamEntry { id, fields });
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(entries)
    }

    pub fn delete_key(&mut self, key: &str) -> Result<()> {
        let _: () = self.connection.del(key).context("Failed to DEL key")?;
        Ok(())
    }

    pub fn get_db_size(&mut self) -> Result<i64> {
        let size: i64 = redis::cmd("DBSIZE")
            .query(&mut self.connection)
            .unwrap_or(0);
        Ok(size)
    }

    #[allow(dead_code)]
    pub fn get_info_section(&mut self, section: &str) -> Result<String> {
        let info: String = redis::cmd("INFO")
            .arg(section)
            .query(&mut self.connection)
            .unwrap_or_default();
        Ok(info)
    }

    pub fn is_connected(&mut self) -> bool {
        self.connection.is_open()
    }

    // ─── Write operations ────────────────────────────────────

    pub fn set_string(&mut self, key: &str, value: &str) -> Result<()> {
        let _: () = self.connection.set(key, value).context("Failed to SET")?;
        Ok(())
    }

    pub fn set_bytes(&mut self, key: &str, value: &[u8]) -> Result<()> {
        let _: () = self
            .connection
            .set(key, value)
            .context("Failed to SET bytes")?;
        Ok(())
    }

    pub fn hset(&mut self, key: &str, field: &str, value: &str) -> Result<()> {
        let _: () = self
            .connection
            .hset(key, field, value)
            .context("Failed to HSET")?;
        Ok(())
    }

    pub fn hset_bytes(&mut self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let _: () = self
            .connection
            .hset(key, field, value)
            .context("Failed to HSET bytes")?;
        Ok(())
    }

    pub fn rpush(&mut self, key: &str, value: &str) -> Result<()> {
        let _: i64 = self
            .connection
            .rpush(key, value)
            .context("Failed to RPUSH")?;
        Ok(())
    }

    pub fn rpush_bytes(&mut self, key: &str, value: &[u8]) -> Result<()> {
        let _: i64 = self
            .connection
            .rpush(key, value)
            .context("Failed to RPUSH bytes")?;
        Ok(())
    }

    pub fn lset(&mut self, key: &str, index: i64, value: &str) -> Result<()> {
        let _: () = redis::cmd("LSET")
            .arg(key)
            .arg(index)
            .arg(value)
            .query(&mut self.connection)
            .context("Failed to LSET")?;
        Ok(())
    }

    pub fn lset_bytes(&mut self, key: &str, index: i64, value: &[u8]) -> Result<()> {
        let _: () = redis::cmd("LSET")
            .arg(key)
            .arg(index)
            .arg(value)
            .query(&mut self.connection)
            .context("Failed to LSET bytes")?;
        Ok(())
    }

    pub fn sadd(&mut self, key: &str, member: &str) -> Result<()> {
        let _: i64 = self
            .connection
            .sadd(key, member)
            .context("Failed to SADD")?;
        Ok(())
    }

    pub fn sadd_bytes(&mut self, key: &str, member: &[u8]) -> Result<()> {
        let _: i64 = self
            .connection
            .sadd(key, member)
            .context("Failed to SADD bytes")?;
        Ok(())
    }

    pub fn zadd(&mut self, key: &str, score: f64, member: &str) -> Result<()> {
        let _: i64 = self
            .connection
            .zadd(key, member, score)
            .context("Failed to ZADD")?;
        Ok(())
    }

    pub fn zadd_bytes(&mut self, key: &str, score: f64, member: &[u8]) -> Result<()> {
        let _: i64 = self
            .connection
            .zadd(key, member, score)
            .context("Failed to ZADD bytes")?;
        Ok(())
    }

    pub fn xadd(&mut self, key: &str, field: &str, value: &str) -> Result<()> {
        let _: String = redis::cmd("XADD")
            .arg(key)
            .arg("*")
            .arg(field)
            .arg(value)
            .query(&mut self.connection)
            .context("Failed to XADD")?;
        Ok(())
    }

    pub fn xadd_binary(&mut self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        let _: String = redis::cmd("XADD")
            .arg(key)
            .arg("*")
            .arg(field)
            .arg(value)
            .query(&mut self.connection)
            .context("Failed to XADD binary")?;
        Ok(())
    }

    pub fn xtrim(&mut self, key: &str, maxlen: usize) -> Result<()> {
        let _: i64 = redis::cmd("XTRIM")
            .arg(key)
            .arg("MAXLEN")
            .arg("~")
            .arg(maxlen)
            .query(&mut self.connection)
            .context("Failed to XTRIM")?;
        Ok(())
    }

    pub fn set_ttl(&mut self, key: &str, ttl: i64) -> Result<()> {
        validate_ttl(ttl, key)?;
        if ttl < 0 {
            let _: () = redis::cmd("PERSIST")
                .arg(key)
                .query(&mut self.connection)
                .context("Failed to PERSIST")?;
        } else {
            let _: () = self
                .connection
                .expire(key, ttl)
                .context("Failed to EXPIRE")?;
        }
        Ok(())
    }

    pub fn rename_key(&mut self, old_key: &str, new_key: &str) -> Result<()> {
        let _: () = self
            .connection
            .rename(old_key, new_key)
            .context("Failed to RENAME")?;
        Ok(())
    }
}

/// Wraps one or more RedisClient connections, aggregating keys from all hosts.
/// Tracks which host owns each key so operations route to the correct connection.
pub struct MultiRedisClient {
    pub clients: Vec<RedisClient>,
    pub labels: Vec<String>,
    /// Maps key name -> index into `clients`. For collisions, first host wins.
    pub key_owner: HashMap<String, usize>,
    /// Keys that exist on multiple hosts (collision warnings).
    pub collisions: Vec<(String, Vec<String>)>,
    pub db: i64,
}

impl MultiRedisClient {
    /// Create from a single URL (backwards-compatible).
    pub fn from_single(url: &str) -> Result<Self> {
        let client = RedisClient::connect(url)?;
        let db = client.db;
        Ok(Self {
            labels: vec![url.to_string()],
            clients: vec![client],
            key_owner: HashMap::new(),
            collisions: Vec::new(),
            db,
        })
    }

    /// Create from multiple URLs, retrying hosts that aren't up yet.
    ///
    /// `max_retries` and `connect_timeout` are caller-supplied so startup cannot
    /// stall indefinitely. Entries here are already known to be well-formed -
    /// `parse_hosts_file` rejects unparseable URLs before this is reached - so
    /// every retry is against a host that could plausibly come up.
    pub fn from_urls(
        urls: &[(String, String)],
        max_retries: u32,
        connect_timeout: std::time::Duration,
    ) -> Result<Self> {
        let mut clients = Vec::new();
        let mut labels = Vec::new();
        let retry_delay = std::time::Duration::from_secs(2);

        // First pass: try to connect to all hosts, track failures
        let mut pending: Vec<(usize, String, String)> = Vec::new();
        for (i, (label, url)) in urls.iter().enumerate() {
            match RedisClient::connect_with_timeout(url, connect_timeout) {
                Ok(client) => {
                    clients.push(Some(client));
                    labels.push(label.clone());
                }
                Err(_) => {
                    clients.push(None);
                    labels.push(label.clone());
                    pending.push((i, label.clone(), url.clone()));
                }
            }
        }

        // Retry failed connections
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

            pending.retain(|(i, label, url)| {
                match RedisClient::connect_with_timeout(url, connect_timeout) {
                    Ok(client) => {
                        eprintln!("  Connected to {}", label);
                        clients[*i] = Some(client);
                        false // remove from pending
                    }
                    Err(_) => true, // keep retrying
                }
            });
        }

        // Collect results, skip hosts that never connected
        let mut final_clients = Vec::new();
        let mut final_labels = Vec::new();
        let mut failed = Vec::new();
        for (i, opt) in clients.into_iter().enumerate() {
            if let Some(client) = opt {
                final_clients.push(client);
                final_labels.push(labels[i].clone());
            } else {
                failed.push(labels[i].clone());
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
            anyhow::bail!("Failed to connect to any Redis host");
        }

        Ok(Self {
            clients: final_clients,
            labels: final_labels,
            key_owner: HashMap::new(),
            collisions: Vec::new(),
            db: 0,
        })
    }

    pub fn select_db(&mut self, db: i64) -> Result<()> {
        for client in &mut self.clients {
            client.select_db(db)?;
        }
        self.db = db;
        Ok(())
    }

    /// Scan all hosts, returning the aggregated keys plus the total number of keys
    /// skipped across hosts because they were not valid UTF-8.
    pub fn scan_keys(&mut self, pattern: &str) -> Result<(Vec<String>, usize)> {
        let mut all_keys: Vec<String> = Vec::new();
        let mut seen: HashMap<String, Vec<usize>> = HashMap::new();
        let mut total_skipped = 0usize;
        self.key_owner.clear();
        self.collisions.clear();

        for (idx, client) in self.clients.iter_mut().enumerate() {
            match client.scan_keys(pattern) {
                Ok((keys, skipped)) => {
                    total_skipped += skipped;
                    for key in keys {
                        seen.entry(key.clone()).or_default().push(idx);
                        if !self.key_owner.contains_key(&key) {
                            self.key_owner.insert(key.clone(), idx);
                            all_keys.push(key);
                        }
                    }
                }
                Err(e) => {
                    // Log error but continue with other hosts
                    eprintln!("Error scanning keys on {}: {}", self.labels[idx], e);
                }
            }
        }

        // Record collisions
        for (key, hosts) in &seen {
            if hosts.len() > 1 {
                let host_names: Vec<String> =
                    hosts.iter().map(|&i| self.labels[i].clone()).collect();
                self.collisions.push((key.clone(), host_names));
            }
        }
        self.collisions.sort_by(|a, b| a.0.cmp(&b.0));

        all_keys.sort();
        Ok((all_keys, total_skipped))
    }

    /// Get the client that owns a key. Falls back to first client.
    fn client_for_key(&mut self, key: &str) -> &mut RedisClient {
        let idx = self.key_owner.get(key).copied().unwrap_or(0);
        &mut self.clients[idx]
    }

    /// Get the host index for a key.
    pub fn host_index_for_key(&self, key: &str) -> usize {
        self.key_owner.get(key).copied().unwrap_or(0)
    }

    /// Get the host label for a key.
    pub fn host_label_for_key(&self, key: &str) -> &str {
        let idx = self.host_index_for_key(key);
        &self.labels[idx]
    }

    /// Check if a key is a collision (exists on multiple hosts).
    pub fn is_collision(&self, key: &str) -> bool {
        self.collisions.iter().any(|(k, _)| k == key)
    }

    pub fn get_key_info(&mut self, key: &str) -> Result<KeyInfo> {
        self.client_for_key(key).get_key_info(key)
    }

    pub fn get_value(&mut self, key: &str) -> Result<RedisValue> {
        self.client_for_key(key).get_value(key)
    }

    pub fn delete_key(&mut self, key: &str) -> Result<()> {
        self.client_for_key(key).delete_key(key)
    }

    pub fn get_db_size(&mut self) -> Result<i64> {
        let mut total: i64 = 0;
        for client in &mut self.clients {
            total += client.get_db_size().unwrap_or(0);
        }
        Ok(total)
    }

    pub fn is_connected(&mut self) -> bool {
        self.clients.iter_mut().any(|c| c.is_connected())
    }

    pub fn num_connected(&mut self) -> usize {
        self.clients
            .iter_mut()
            .filter(|c| c.connection.is_open())
            .count()
    }

    // ─── Write operations (route to key owner) ────────────────

    pub fn set_string(&mut self, key: &str, value: &str) -> Result<()> {
        self.client_for_key(key).set_string(key, value)
    }

    pub fn set_bytes(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.client_for_key(key).set_bytes(key, value)
    }

    pub fn hset(&mut self, key: &str, field: &str, value: &str) -> Result<()> {
        self.client_for_key(key).hset(key, field, value)
    }

    pub fn hset_bytes(&mut self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        self.client_for_key(key).hset_bytes(key, field, value)
    }

    pub fn rpush(&mut self, key: &str, value: &str) -> Result<()> {
        self.client_for_key(key).rpush(key, value)
    }

    pub fn rpush_bytes(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.client_for_key(key).rpush_bytes(key, value)
    }

    pub fn lset(&mut self, key: &str, index: i64, value: &str) -> Result<()> {
        self.client_for_key(key).lset(key, index, value)
    }

    pub fn lset_bytes(&mut self, key: &str, index: i64, value: &[u8]) -> Result<()> {
        self.client_for_key(key).lset_bytes(key, index, value)
    }

    pub fn sadd(&mut self, key: &str, member: &str) -> Result<()> {
        self.client_for_key(key).sadd(key, member)
    }

    pub fn sadd_bytes(&mut self, key: &str, member: &[u8]) -> Result<()> {
        self.client_for_key(key).sadd_bytes(key, member)
    }

    pub fn zadd(&mut self, key: &str, score: f64, member: &str) -> Result<()> {
        self.client_for_key(key).zadd(key, score, member)
    }

    pub fn zadd_bytes(&mut self, key: &str, score: f64, member: &[u8]) -> Result<()> {
        self.client_for_key(key).zadd_bytes(key, score, member)
    }

    pub fn xadd(&mut self, key: &str, field: &str, value: &str) -> Result<()> {
        self.client_for_key(key).xadd(key, field, value)
    }

    pub fn xadd_binary(&mut self, key: &str, field: &str, value: &[u8]) -> Result<()> {
        self.client_for_key(key).xadd_binary(key, field, value)
    }

    pub fn set_ttl(&mut self, key: &str, ttl: i64) -> Result<()> {
        self.client_for_key(key).set_ttl(key, ttl)
    }

    pub fn rename_key(&mut self, old_key: &str, new_key: &str) -> Result<()> {
        self.client_for_key(old_key).rename_key(old_key, new_key)
    }

    /// Get the first URL (used for stream listener/signal generator connections).
    #[allow(dead_code)]
    pub fn first_url(&self) -> &str {
        &self.clients[0].url
    }

    /// Get the URL for the host that owns a key.
    pub fn url_for_key(&self, key: &str) -> &str {
        let idx = self.host_index_for_key(key);
        &self.clients[idx].url
    }

    pub fn host_count(&self) -> usize {
        self.clients.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::TcpListener;
    use std::process::{Child, Command, Stdio};

    /// Ask the OS for a currently-free port, then release it so redis-server can bind.
    ///
    /// Cargo runs tests in parallel and several `cargo test` runs may overlap on one
    /// machine, so a fixed or sequentially-allocated port collides sooner or later -
    /// with sibling tests, with a dev instance on 6379/6380, or with anything else on
    /// the box. Letting the kernel pick removes the guess; the small window between
    /// releasing the port and redis binding it is covered by the retry in `start`.
    fn free_port() -> Option<u16> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        drop(listener);
        Some(port)
    }

    /// A throwaway redis-server that is killed when the test ends.
    struct TestRedis {
        child: Child,
        port: u16,
    }

    impl TestRedis {
        fn start() -> Self {
            const ATTEMPTS: usize = 5;
            let mut last_error = "no attempt made".to_string();

            for _ in 0..ATTEMPTS {
                let Some(port) = free_port() else {
                    last_error = "could not obtain a free port from the OS".to_string();
                    continue;
                };
                match Self::try_start(port) {
                    Ok(server) => return server,
                    Err(e) => last_error = e,
                }
            }

            panic!("could not start redis-server after {ATTEMPTS} attempts: {last_error}");
        }

        fn try_start(port: u16) -> Result<Self, String> {
            let child = Command::new("redis-server")
                .args([
                    "--port", &port.to_string(),
                    "--save", "",
                    "--appendonly", "no",
                    "--loglevel", "warning",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    format!("could not spawn redis-server ({e}); it must be installed to run --ignored tests")
                })?;

            let mut server = TestRedis { child, port };
            let url = server.url();

            for _ in 0..100 {
                // A bind failure makes redis exit almost immediately - notice that
                // instead of waiting out the full readiness timeout.
                match server.child.try_wait() {
                    Ok(Some(status)) => {
                        return Err(format!(
                            "redis-server on port {port} exited early ({status})"
                        ))
                    }
                    Ok(None) => {}
                    Err(e) => return Err(format!("could not poll redis-server: {e}")),
                }

                if let Ok(client) = redis::Client::open(url.as_str()) {
                    if client.get_connection().is_ok() {
                        return Ok(server);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            // Dropping `server` kills the child, so the next attempt starts clean.
            Err(format!("redis-server on port {port} never became ready"))
        }

        fn url(&self) -> String {
            format!("redis://127.0.0.1:{}", self.port)
        }
    }

    impl Drop for TestRedis {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    // ---- #12: SCAN reports undecodable keys instead of hiding them ----

    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn scan_counts_keys_skipped_for_invalid_utf8() {
        let server = TestRedis::start();
        let mut client = RedisClient::connect(&server.url()).unwrap();

        set_key(&mut client, "good:one", "v1");
        set_key(&mut client, "good:two", "v2");
        // 0xff 0xfe 0xfd is not valid UTF-8, so Iter<String> yields Err for it.
        redis::cmd("SET")
            .arg(&[0xffu8, 0xfe, 0xfd][..])
            .arg("binary")
            .exec(&mut client.connection)
            .unwrap();

        let (keys, skipped) = client.scan_keys("*").unwrap();

        assert_eq!(keys, vec!["good:one".to_string(), "good:two".to_string()]);
        assert_eq!(
            skipped, 1,
            "the non-UTF-8 key must be counted, not silently dropped"
        );
    }

    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn scan_reports_zero_skipped_when_all_keys_decode() {
        let server = TestRedis::start();
        let mut client = RedisClient::connect(&server.url()).unwrap();

        set_key(&mut client, "good:one", "v1");

        let (keys, skipped) = client.scan_keys("*").unwrap();

        assert_eq!(keys.len(), 1);
        assert_eq!(skipped, 0);
    }

    fn set_key(client: &mut RedisClient, key: &str, value: &str) {
        redis::cmd("SET")
            .arg(key)
            .arg(value)
            .exec(&mut client.connection)
            .unwrap();
    }

    /// The DB the connection is actually on, straight from the server.
    fn actual_db(client: &mut RedisClient) -> i64 {
        let info: String = redis::cmd("CLIENT")
            .arg("INFO")
            .query(&mut client.connection)
            .unwrap();
        info.split_whitespace()
            .find_map(|f| f.strip_prefix("db="))
            .and_then(|v| v.parse().ok())
            .unwrap()
    }

    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn reported_db_matches_actual_db_with_query_string() {
        let server = TestRedis::start();
        // The redis crate honours the /3 path and connects to db 3 either way; the bug
        // was that RedisClient::db reported 0, so the status bar lied about which
        // database the user was looking at.
        let url = format!("{}/3?timeout=5", server.url());
        let mut client = RedisClient::connect(&url).unwrap();

        assert_eq!(
            actual_db(&mut client),
            3,
            "sanity: connection should be on db 3"
        );
        assert_eq!(client.db, 3, "reported db must match the real one");
    }

    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn reported_db_matches_actual_db_without_path() {
        let server = TestRedis::start();
        let mut client = RedisClient::connect(&server.url()).unwrap();

        assert_eq!(actual_db(&mut client), 0);
        assert_eq!(client.db, 0);
    }

    /// End-to-end: the skipped count must reach the status line the user actually reads.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn refresh_keys_status_line_reports_skipped_keys() {
        let server = TestRedis::start();
        let mut seed = RedisClient::connect(&server.url()).unwrap();
        set_key(&mut seed, "good:one", "v1");
        set_key(&mut seed, "good:two", "v2");
        redis::cmd("SET")
            .arg(&[0xffu8, 0xfe, 0xfd][..])
            .arg("binary")
            .exec(&mut seed.connection)
            .unwrap();
        drop(seed);

        let mut multi = MultiRedisClient::from_single(&server.url()).unwrap();
        let mut app = crate::app::App::new();
        app.refresh_keys(&mut multi);

        assert_eq!(app.keys.len(), 2);
        assert!(
            app.status_message.contains("1 skipped"),
            "status line should disclose the skipped key, got: {}",
            app.status_message
        );
    }

    // ---- #10: TTL validation ----

    #[test]
    fn ttl_zero_is_rejected() {
        // EXPIRE key 0 deletes the key immediately; refuse before sending anything.
        assert!(validate_ttl(0, "mykey").is_err());
    }

    #[test]
    fn ttl_zero_error_names_the_key() {
        let err = validate_ttl(0, "mykey").unwrap_err().to_string();
        assert!(
            err.contains("mykey"),
            "error should name the key, got: {}",
            err
        );
    }

    #[test]
    fn ttl_negative_is_accepted_as_persist() {
        assert!(validate_ttl(-1, "mykey").is_ok());
    }

    #[test]
    fn ttl_any_negative_is_accepted_as_persist() {
        // set_ttl routes every ttl < 0 to PERSIST, so validation must not single out -1.
        for ttl in [-1, -2, -100, i64::MIN] {
            assert!(
                validate_ttl(ttl, "mykey").is_ok(),
                "ttl {} should be allowed",
                ttl
            );
        }
    }

    #[test]
    fn ttl_zero_error_does_not_name_a_specific_sentinel() {
        // The edit field is labelled "empty=persist"; the message must not invent a
        // different contract by telling users to type -1.
        let err = validate_ttl(0, "mykey").unwrap_err().to_string();
        assert!(
            err.contains("empty"),
            "message should point at the documented path: {}",
            err
        );
    }

    #[test]
    fn ttl_positive_is_accepted() {
        assert!(validate_ttl(60, "mykey").is_ok());
    }

    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn set_ttl_zero_leaves_the_key_intact() {
        let server = TestRedis::start();
        let mut client = RedisClient::connect(&server.url()).unwrap();
        set_key(&mut client, "keeper", "precious");

        let result = client.set_ttl("keeper", 0);

        assert!(result.is_err(), "TTL 0 must be refused");
        let exists: bool = redis::cmd("EXISTS")
            .arg("keeper")
            .query(&mut client.connection)
            .unwrap();
        assert!(
            exists,
            "the key must still exist - no command should have been sent"
        );
    }

    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn set_ttl_positive_applies_expiry() {
        let server = TestRedis::start();
        let mut client = RedisClient::connect(&server.url()).unwrap();
        set_key(&mut client, "temp", "v");

        client.set_ttl("temp", 120).unwrap();

        let ttl: i64 = redis::cmd("TTL")
            .arg("temp")
            .query(&mut client.connection)
            .unwrap();
        assert!(ttl > 0 && ttl <= 120, "expected a live ttl, got {}", ttl);
    }

    // ---- #9: DB number parsing from URL ----

    #[test]
    fn db_parses_from_plain_path() {
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/3"), 3);
    }

    #[test]
    fn db_parses_with_query_string() {
        // The original bug: rsplit('/') yields "3?timeout=5", which fails to parse.
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/3?timeout=5"), 3);
    }

    #[test]
    fn db_parses_with_fragment() {
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/7#frag"), 7);
    }

    #[test]
    fn db_defaults_to_zero_without_path() {
        // No path component at all - must be 0 deliberately, not by parse failure.
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379"), 0);
    }

    #[test]
    fn db_defaults_to_zero_for_non_numeric_path() {
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/notanumber"), 0);
    }

    #[test]
    fn db_parses_with_auth_in_url() {
        assert_eq!(parse_db_from_url("redis://user:pw@127.0.0.1:6379/5"), 5);
    }

    #[test]
    fn db_parses_with_trailing_slash() {
        // rsplit('/') on "3/" yields an empty final segment, which must not be
        // mistaken for "no db given".
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/3/"), 3);
    }

    #[test]
    fn db_parses_with_query_string_after_trailing_slash() {
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/3/?timeout=5"), 3);
    }

    #[test]
    fn db_rejects_negative_number() {
        // A negative DB is nonsense and would only fail later at SELECT.
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/-1"), 0);
    }

    #[test]
    fn db_parses_with_empty_path() {
        assert_eq!(parse_db_from_url("redis://127.0.0.1:6379/"), 0);
    }

    /// A host that is well-formed but not listening must not hold startup for the
    /// old 15 x (10s connect + 2s sleep) budget. Uses a port the OS just handed
    /// back and released, so the connection is refused immediately and what this
    /// measures is the retry budget rather than network latency.
    #[test]
    fn from_urls_gives_up_on_an_unreachable_host_quickly() {
        let port = match free_port() {
            Some(p) => p,
            None => return,
        };
        let urls = vec![("dead".to_string(), format!("redis://127.0.0.1:{}", port))];

        let start = std::time::Instant::now();
        let result = MultiRedisClient::from_urls(&urls, 2, std::time::Duration::from_secs(1));
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "no host connected, so this must be an error"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "gave up after {:?}; the retry budget is not being honoured",
            elapsed
        );
    }
}
