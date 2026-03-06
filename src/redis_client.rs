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

impl RedisClient {
    pub fn connect(url: &str) -> Result<Self> {
        let client = redis::Client::open(url)
            .with_context(|| format!("Failed to create Redis client for {}", url))?;
        let connection = client
            .get_connection()
            .with_context(|| format!("Failed to connect to {}", url))?;

        // Parse db number from URL (e.g., redis://host:port/3)
        let db = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        Ok(Self {
            connection,
            url: url.to_string(),
            db: db as i64,
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

    pub fn scan_keys(&mut self, pattern: &str) -> Result<Vec<String>> {
        let iter: redis::Iter<String> = redis::cmd("SCAN")
            .cursor_arg(0)
            .arg("MATCH")
            .arg(pattern)
            .arg("COUNT")
            .arg(1000)
            .clone()
            .iter(&mut self.connection)
            .context("Failed to SCAN keys")?;

        let mut keys: Vec<String> = iter.filter_map(|r| r.ok()).collect();
        keys.sort();
        Ok(keys)
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
                let map: HashMap<String, Vec<u8>> = self
                    .connection
                    .hgetall(key)
                    .context("Failed to HGETALL")?;
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
                        redis::Value::BulkString(b) => {
                            String::from_utf8_lossy(b).to_string()
                        }
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
    pub fn xread_blocking(&mut self, key: &str, last_id: &str, timeout_ms: u64) -> Result<Vec<StreamEntry>> {
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
                                                _ => { i += 2; continue; }
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
        let _: () = self.connection
            .del(key)
            .context("Failed to DEL key")?;
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
        let _: () = self.connection.set(key, value).context("Failed to SET bytes")?;
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

    pub fn set_ttl(&mut self, key: &str, ttl: i64) -> Result<()> {
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

    /// Create from multiple URLs with retry logic for hosts that aren't ready yet.
    pub fn from_urls(urls: &[(String, String)]) -> Result<Self> {
        let mut clients = Vec::new();
        let mut labels = Vec::new();
        let max_retries = 15;
        let retry_delay = std::time::Duration::from_secs(2);

        // First pass: try to connect to all hosts, track failures
        let mut pending: Vec<(usize, String, String)> = Vec::new();
        for (i, (label, url)) in urls.iter().enumerate() {
            match RedisClient::connect(url) {
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
        let mut attempt = 0;
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
                match RedisClient::connect(url) {
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
                "Warning: could not connect to {} host(s): {}",
                failed.len(),
                failed.join(", ")
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

    pub fn scan_keys(&mut self, pattern: &str) -> Result<Vec<String>> {
        let mut all_keys: Vec<String> = Vec::new();
        let mut seen: HashMap<String, Vec<usize>> = HashMap::new();
        self.key_owner.clear();
        self.collisions.clear();

        for (idx, client) in self.clients.iter_mut().enumerate() {
            match client.scan_keys(pattern) {
                Ok(keys) => {
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
                let host_names: Vec<String> = hosts.iter().map(|&i| self.labels[i].clone()).collect();
                self.collisions.push((key.clone(), host_names));
            }
        }
        self.collisions.sort_by(|a, b| a.0.cmp(&b.0));

        all_keys.sort();
        Ok(all_keys)
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
        self.clients.iter_mut().filter(|c| c.connection.is_open()).count()
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
