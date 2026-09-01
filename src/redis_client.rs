use anyhow::{Context, Result};
use redis::{Commands, ConnectionLike, IntoConnectionInfo};
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
/// A loaded value plus how much of it exists.
///
/// `total_items` is `Some(n)` for a collection, whether or not it was capped -
/// the pane compares it against what it received to report what is hidden -
/// and `None` for types that are not collections.
#[derive(Debug, Clone)]
pub struct LoadedValue {
    pub value: RedisValue,
    pub total_items: Option<usize>,
}

impl LoadedValue {
    fn whole(value: RedisValue) -> Self {
        LoadedValue {
            value,
            total_items: None,
        }
    }

    fn capped(value: RedisValue, total: usize) -> Self {
        LoadedValue {
            value,
            total_items: Some(total),
        }
    }
}

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

pub struct RedisClient {
    connection: redis::Connection,
    info: redis::ConnectionInfo,
    /// `host:port`. Display only, and never contains credentials.
    pub label: String,
    pub db: i64,
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
    /// Connect using a URL. Retained for the unit tests here and the
    /// `#[ignore]`d live tests, which have a URL and no defaults to apply.
    ///
    /// Nothing in the production binary calls this any more: `StreamListener`
    /// and `SignalGenerator` take a `ConnectionInfo` directly and go through
    /// `connect_with_info`, each with a hardcoded 10s timeout rather than the
    /// user's `--connect-timeout` - threading that through is a separate
    /// change (#44) alongside the socket *read* timeout, which is out of
    /// scope here too. So rustc sees this as dead code in a plain
    /// `cargo build`; it is exercised by `cargo test`.
    #[allow(dead_code)]
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
        let client = redis::Client::open(info.clone())
            .with_context(|| format!("Failed to create Redis client for {}", label))?;
        let connection = client
            .get_connection_with_timeout(timeout)
            .with_context(|| format!("Failed to connect to {}", label))?;

        Ok(Self {
            connection,
            info,
            label: label.to_string(),
            db,
        })
    }

    /// The parsed connection info this client was built from, for a
    /// background thread to open its own connection with.
    pub fn connection_info(&self) -> redis::ConnectionInfo {
        self.info.clone()
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

    /// Read a key's value, fetching at most `max_items` elements of a
    /// collection.
    ///
    /// Nothing here may fetch a collection whole. Arrow-key navigation
    /// auto-loads the selected key, so an unbounded LRANGE/SMEMBERS/HGETALL
    /// meant merely scrolling onto a large key pulled all of it onto the main
    /// thread (#87). Sets and hashes are read through SSCAN/HSCAN and stopped
    /// at the cap rather than fetched and trimmed, so the server never
    /// materialises the whole reply either.
    ///
    /// `total_items` carries the collection's true length so the pane can say
    /// how much is hidden. Strings are not collections: they are returned
    /// whole, because truncating a binary payload corrupts it.
    pub fn get_value(&mut self, key: &str, max_items: usize) -> Result<LoadedValue> {
        let key_type: String = redis::cmd("TYPE")
            .arg(key)
            .query(&mut self.connection)
            .unwrap_or_else(|_| "unknown".to_string());

        // COUNT is a per-batch hint: large enough to avoid a round trip per
        // element, small enough not to undo the cap on a huge collection.
        let scan_hint = max_items.clamp(10, 1000);

        match key_type.as_str() {
            "string" => {
                let val: Vec<u8> = self.connection.get(key).context("Failed to GET")?;
                Ok(LoadedValue::whole(RedisValue::String(val)))
            }
            "list" => {
                let total: usize = self.connection.llen(key).context("Failed to LLEN")?;
                let vals: Vec<Vec<u8>> = self
                    .connection
                    .lrange(key, 0, max_items as isize - 1)
                    .context("Failed to LRANGE")?;
                Ok(LoadedValue::capped(RedisValue::List(vals), total))
            }
            "set" => {
                let total: usize = self.connection.scard(key).context("Failed to SCARD")?;
                let iter: redis::Iter<Vec<u8>> = redis::cmd("SSCAN")
                    .arg(key)
                    .cursor_arg(0)
                    .arg("COUNT")
                    .arg(scan_hint)
                    .clone()
                    .iter(&mut self.connection)
                    .context("Failed to SSCAN")?;
                let mut vals = Vec::new();
                for item in iter.take(max_items) {
                    vals.push(item.context("Failed to read a set member")?);
                }
                Ok(LoadedValue::capped(RedisValue::Set(vals), total))
            }
            "zset" => {
                let total: usize = self.connection.zcard(key).context("Failed to ZCARD")?;
                let vals: Vec<(Vec<u8>, f64)> = self
                    .connection
                    .zrange_withscores(key, 0, max_items as isize - 1)
                    .context("Failed to ZRANGE")?;
                Ok(LoadedValue::capped(RedisValue::ZSet(vals), total))
            }
            "hash" => {
                let total: usize = self.connection.hlen(key).context("Failed to HLEN")?;
                let iter: redis::Iter<(String, Vec<u8>)> = redis::cmd("HSCAN")
                    .arg(key)
                    .cursor_arg(0)
                    .arg("COUNT")
                    .arg(scan_hint)
                    .clone()
                    .iter(&mut self.connection)
                    .context("Failed to HSCAN")?;
                let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
                for item in iter.take(max_items) {
                    pairs.push(item.context("Failed to read a hash field")?);
                }
                pairs.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(LoadedValue::capped(RedisValue::Hash(pairs), total))
            }
            "stream" => {
                let entries = self.get_stream_entries(key)?;
                Ok(LoadedValue::whole(RedisValue::Stream(entries)))
            }
            other => Ok(LoadedValue::whole(RedisValue::Unknown(format!(
                "Unsupported type: {}",
                other
            )))),
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

        // First pass: try to connect to all hosts, track failures
        let mut pending: Vec<usize> = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            labels.push(entry.label.clone());
            match RedisClient::connect_with_info(entry.info.clone(), &entry.label, connect_timeout)
            {
                Ok(client) => clients.push(Some(client)),
                Err(_) => {
                    clients.push(None);
                    pending.push(i);
                }
            }
        }

        // Retry failed connections
        let retry_delay = std::time::Duration::from_secs(2);
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
                // Read from the client itself rather than the `labels` array
                // tracked above - the two always agree (both trace back to
                // `entries[i].label`), but this way there is one source of
                // truth instead of two copies that merely happen to match.
                final_labels.push(client.label.clone());
                final_clients.push(client);
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
            anyhow::bail!(
                "Could not connect to any of the {} host(s) given",
                entries.len()
            );
        }

        let db = final_clients[0].db;
        Ok(Self {
            clients: final_clients,
            labels: final_labels,
            key_owner: HashMap::new(),
            collisions: Vec::new(),
            db,
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

    pub fn get_value(&mut self, key: &str, max_items: usize) -> Result<LoadedValue> {
        self.client_for_key(key).get_value(key, max_items)
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

    /// Connection info for the host owning `key`, for a background thread to
    /// open its own connection with.
    pub fn info_for_key(&self, key: &str) -> redis::ConnectionInfo {
        let idx = self.host_index_for_key(key);
        self.clients[idx].connection_info()
    }

    pub fn host_count(&self) -> usize {
        self.clients.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::TcpListener;
    use std::path::PathBuf;
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
        dir: PathBuf,
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
            // Every server gets its own empty directory, and it is not optional.
            // `--save ""` below stops this server *writing* a dump, but redis
            // *loads* `dump.rdb` from `dir` at startup regardless, and `dir`
            // defaults to the working directory - the package root under cargo.
            // So any dump.rdb lying there is adopted as the server's starting
            // dataset, and every key-listing assertion in these tests then sees
            // somebody else's data. `start-dev.sh` used to leave exactly such a
            // file behind, which made these tests fail on a developer machine
            // and pass in CI, where the checkout is clean.
            let dir = std::env::temp_dir().join(format!(
                "redis-tui-test-{}-{}",
                std::process::id(),
                port
            ));
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("could not create data directory {}: {e}", dir.display()))?;
            let dir_arg = dir.to_string_lossy().into_owned();

            let child = Command::new("redis-server")
                .args([
                    "--port", &port.to_string(),
                    "--dir", &dir_arg,
                    "--save", "",
                    "--appendonly", "no",
                    "--loglevel", "warning",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| {
                    let _ = std::fs::remove_dir_all(&dir);
                    format!("could not spawn redis-server ({e}); it must be installed to run --ignored tests")
                })?;

            let mut server = TestRedis { child, port, dir };
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
            // Only after the child is reaped, so nothing can recreate the
            // directory between the removal and the process actually exiting.
            let _ = std::fs::remove_dir_all(&self.dir);
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

    /// A single-host `MultiRedisClient` for tests that just need a live
    /// connection, not any particular entry defaults or retry behaviour.
    fn connect_multi(url: &str) -> MultiRedisClient {
        let entry =
            crate::hosts::parse_host_entry(url, &crate::hosts::HostDefaults::default()).unwrap();
        MultiRedisClient::from_entries(
            std::slice::from_ref(&entry),
            0,
            std::time::Duration::from_secs(3),
        )
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

    // ─── #87: collection fetches must be bounded ─────────────

    /// Seed `n` members of each collection type, then read them back with a
    /// cap far below `n`.
    fn seed_big_collections(url: &str, n: usize) {
        let mut c = RedisClient::connect(url).unwrap();
        let mut lpush = redis::cmd("RPUSH");
        lpush.arg("big:list");
        let mut sadd = redis::cmd("SADD");
        sadd.arg("big:set");
        let mut zadd = redis::cmd("ZADD");
        zadd.arg("big:zset");
        let mut hset = redis::cmd("HSET");
        hset.arg("big:hash");
        for i in 0..n {
            lpush.arg(format!("item-{:06}", i));
            sadd.arg(format!("member-{:06}", i));
            zadd.arg(i as f64).arg(format!("scored-{:06}", i));
            hset.arg(format!("field-{:06}", i)).arg(i);
        }
        for cmd in [&lpush, &sadd, &zadd, &hset] {
            cmd.query::<()>(&mut c.connection).unwrap();
        }
    }

    // The reported hang: arrow-key navigation auto-loads the selected key, so
    // merely scrolling onto a large collection pulled the whole thing into
    // memory on the main thread. Every collection type must stop at the cap.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn every_collection_type_is_capped_at_max_items() {
        let server = TestRedis::start();
        seed_big_collections(&server.url(), 5000);
        let mut c = RedisClient::connect(&server.url()).unwrap();

        for key in ["big:list", "big:set", "big:zset", "big:hash"] {
            let loaded = c.get_value(key, 100).unwrap();
            let n = match &loaded.value {
                RedisValue::List(v) | RedisValue::Set(v) => v.len(),
                RedisValue::ZSet(v) => v.len(),
                RedisValue::Hash(v) => v.len(),
                other => panic!("{} loaded as {:?}", key, other),
            };
            assert_eq!(n, 100, "{} should stop at the cap, got {}", key, n);
            assert_eq!(
                loaded.total_items,
                Some(5000),
                "{} should report its true length",
                key
            );
        }
    }

    // Under the cap nothing is hidden, and the totals still agree.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn a_small_collection_is_returned_whole() {
        let server = TestRedis::start();
        seed_big_collections(&server.url(), 7);
        let mut c = RedisClient::connect(&server.url()).unwrap();

        for key in ["big:list", "big:set", "big:zset", "big:hash"] {
            let loaded = c.get_value(key, 100).unwrap();
            assert_eq!(loaded.total_items, Some(7), "{}", key);
        }
    }

    // A list keeps insertion order, so the cap must take the *first* N rather
    // than an arbitrary N -- otherwise the pane shows different data per load.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn a_capped_list_keeps_its_first_items_in_order() {
        let server = TestRedis::start();
        seed_big_collections(&server.url(), 500);
        let mut c = RedisClient::connect(&server.url()).unwrap();

        let loaded = c.get_value("big:list", 3).unwrap();
        match loaded.value {
            RedisValue::List(v) => {
                let got: Vec<String> = v
                    .iter()
                    .map(|b| String::from_utf8_lossy(b).into())
                    .collect();
                assert_eq!(got, vec!["item-000000", "item-000001", "item-000002"]);
            }
            other => panic!("expected a list, got {:?}", other),
        }
    }

    // Strings are not collections: the cap must not truncate binary payloads,
    // which would corrupt them exactly as #82 did.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn a_string_is_never_truncated_by_the_item_cap() {
        let server = TestRedis::start();
        let blob: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let mut seed = RedisClient::connect(&server.url()).unwrap();
        redis::cmd("SET")
            .arg("blob")
            .arg(&blob)
            .query::<()>(&mut seed.connection)
            .unwrap();

        let loaded = seed.get_value("blob", 10).unwrap();
        match loaded.value {
            RedisValue::String(b) => assert_eq!(b, blob, "the string was truncated"),
            other => panic!("expected a string, got {:?}", other),
        }
        assert_eq!(loaded.total_items, None, "a string has no item count");
    }

    // #82: the exact reported sequence -- select a binary key, press `s`, press
    // Enter without typing anything -- grew a 4000-byte float32 blob to 6847
    // bytes of U+FFFD. It must now leave the bytes untouched.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn enter_on_an_untouched_edit_popup_cannot_destroy_a_binary_key() {
        let server = TestRedis::start();

        // 1000 little-endian float32 samples, as `start-dev.sh` seeds.
        let blob: Vec<u8> = (0..1000u32)
            .flat_map(|i| ((i as f32) * 0.01).sin().to_le_bytes())
            .collect();
        assert_eq!(blob.len(), 4000);

        let mut seed = RedisClient::connect(&server.url()).unwrap();
        redis::cmd("SET")
            .arg("blob:float32_1k")
            .arg(&blob)
            .query::<()>(&mut seed.connection)
            .unwrap();
        drop(seed);

        let mut multi = connect_multi(&server.url());
        let mut app = crate::app::App::new();
        app.refresh_keys(&mut multi);
        app.key_list_state.select(Some(0));
        app.load_selected_value(&mut multi);

        app.start_edit();
        let result = app.execute_edit(&mut multi);
        assert!(
            result.is_err(),
            "an untouched popup must not write, got {:?}",
            result
        );

        let mut check = RedisClient::connect(&server.url()).unwrap();
        let after: Vec<u8> = redis::cmd("GET")
            .arg("blob:float32_1k")
            .query(&mut check.connection)
            .unwrap();
        assert_eq!(after.len(), blob.len(), "length changed");
        assert_eq!(after, blob, "the blob was modified");
    }

    /// Regression test for the delete confirmation acting on the live selection.
    ///
    /// The popup is half the frame wide and does not cover the key list, and mouse
    /// input is not gated by `InputMode`, so a click can move the selection while
    /// the prompt is up. The delete must still hit the key the prompt named.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn confirmed_delete_targets_the_key_the_prompt_named() {
        let server = TestRedis::start();
        let mut seed = RedisClient::connect(&server.url()).unwrap();
        set_key(&mut seed, "alpha", "keep-me");
        set_key(&mut seed, "beta", "keep-me-too");
        drop(seed);

        let mut multi = connect_multi(&server.url());
        let mut app = crate::app::App::new();
        app.refresh_keys(&mut multi);
        assert_eq!(app.keys, vec!["alpha".to_string(), "beta".to_string()]);

        // `d` on alpha captures alpha as the target.
        app.key_list_state.select(Some(0));
        app.confirm_action = Some(crate::app::ConfirmAction::DeleteKey {
            key: app.selected_key_name().unwrap().to_string(),
        });
        app.input_mode = crate::app::InputMode::Confirm;

        // A click lands on beta while the popup is still open.
        app.key_list_state.select(Some(1));
        assert_eq!(app.selected_key_name(), Some("beta"));

        crate::handle_confirm_input(&mut app, &mut multi, crossterm::event::KeyCode::Char('y'));

        let mut check = RedisClient::connect(&server.url()).unwrap();
        let alpha: bool = redis::cmd("EXISTS")
            .arg("alpha")
            .query(&mut check.connection)
            .unwrap();
        let beta: bool = redis::cmd("EXISTS")
            .arg("beta")
            .query(&mut check.connection)
            .unwrap();
        assert!(
            !alpha,
            "the key named in the prompt should have been deleted"
        );
        assert!(beta, "the key merely selected at the time must survive");
        assert!(
            app.status_message.contains("alpha"),
            "status line should name the deleted key, got: {}",
            app.status_message
        );
    }

    /// Documents what a total scan failure currently looks like to `App`.
    ///
    /// `MultiRedisClient::scan_keys` never returns `Err` - it logs each per-host
    /// failure with `eprintln!` and returns whatever succeeded - so losing every
    /// host yields `Ok((vec![], 0))` and the status line reads "Loaded 0 keys".
    /// An empty keyspace and an unreachable server are indistinguishable, and the
    /// only account of the failure goes to stderr, over the alternate screen.
    ///
    /// That is #92, not something this change fixes. This test pins the current
    /// behaviour so fixing #92 fails here and the assertion gets updated
    /// deliberately rather than the change landing unnoticed.
    ///
    /// It also covers the half that IS in scope: a refresh that finds nothing
    /// must not leave a stale value on screen.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn total_scan_failure_is_currently_indistinguishable_from_an_empty_keyspace() {
        let server = TestRedis::start();
        let url = server.url();
        let mut seed = RedisClient::connect(&url).unwrap();
        set_key(&mut seed, "aaa", "value-of-aaa");
        drop(seed);

        let mut multi = connect_multi(&url);
        let mut app = crate::app::App::new();
        app.refresh_keys(&mut multi);
        app.key_list_state.select(Some(0));
        app.load_selected_value(&mut multi);
        assert!(matches!(
            app.current_value,
            Some(RedisValue::String(ref b)) if b == b"value-of-aaa"
        ));

        // The server disappears mid-session; dropping the harness kills it.
        drop(server);

        app.refresh_keys(&mut multi);
        assert!(
            app.status_message.contains("Loaded 0 keys"),
            "known #92 behaviour: the host error is swallowed and reported as an \
             empty keyspace. If this now names the failure, #92 is fixed - update \
             this test. Got: {}",
            app.status_message
        );
        assert_eq!(
            app.key_list_state.selected(),
            None,
            "an empty listing must clear the selection"
        );
    }

    /// Regression test for the value pane outliving the key it describes.
    ///
    /// `refresh_keys` preserves the selection index, not the selected key, so a
    /// refresh that changes what that index points at must reload the value too.
    /// Otherwise `start_edit` prefills the new key's editor with the old key's
    /// value and saving writes it back under the new name.
    #[test]
    #[ignore = "requires redis-server; run with: cargo test -- --ignored"]
    fn refresh_reloads_the_value_when_the_selected_key_changes() {
        let server = TestRedis::start();
        let mut seed = RedisClient::connect(&server.url()).unwrap();
        set_key(&mut seed, "aaa", "value-of-aaa");

        let mut multi = connect_multi(&server.url());
        let mut app = crate::app::App::new();
        app.refresh_keys(&mut multi);
        app.key_list_state.select(Some(0));
        app.load_selected_value(&mut multi);
        assert!(matches!(
            app.current_value,
            Some(RedisValue::String(ref b)) if b == b"value-of-aaa"
        ));

        // The key at index 0 is now a different key entirely.
        redis::cmd("DEL")
            .arg("aaa")
            .exec(&mut seed.connection)
            .unwrap();
        set_key(&mut seed, "aab", "value-of-aab");
        drop(seed);

        app.refresh_keys(&mut multi);
        assert_eq!(app.keys, vec!["aab".to_string()]);
        assert_eq!(app.key_list_state.selected(), Some(0));
        assert!(
            matches!(app.current_value, Some(RedisValue::String(ref b)) if b == b"value-of-aab"),
            "refresh must reload the value for the newly selected key, got: {:?}",
            app.current_value
        );
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

        let mut multi = connect_multi(&server.url());
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

    /// A host that is well-formed but not listening must not hold startup for the
    /// old 15 x (10s connect + 2s sleep) budget. Uses a port the OS just handed
    /// back and released, so the connection is refused immediately and what this
    /// measures is the retry budget rather than network latency.
    #[test]
    fn from_entries_gives_up_on_an_unreachable_host_quickly() {
        let port = match free_port() {
            Some(p) => p,
            None => return,
        };
        let entry = crate::hosts::parse_host_entry(
            &format!("127.0.0.1:{}", port),
            &crate::hosts::HostDefaults::default(),
        )
        .unwrap();

        let start = std::time::Instant::now();
        let result = MultiRedisClient::from_entries(
            std::slice::from_ref(&entry),
            2,
            std::time::Duration::from_secs(1),
        );
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
}
