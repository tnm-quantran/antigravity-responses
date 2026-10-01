use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPLAY_HEADER: &str = "antigravity-replay-v1\n";

pub struct Replay {
    database: Connection,
    ttl: Duration,
}

impl Replay {
    pub fn new(limit: usize, ttl: Duration) -> Self {
        Self::initialize(
            Connection::open_in_memory().expect("open replay database"),
            limit,
            ttl,
        )
        .expect("initialize replay database")
    }

    pub fn open(limit: usize, ttl: Duration, path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .with_context(|| format!("create replay database {}", path.display()))?;
        let mut replay = Self::initialize(
            Connection::open(&path)
                .with_context(|| format!("open replay database {}", path.display()))?,
            limit,
            ttl,
        )?;
        replay.import_legacy(&path.with_extension("json"))?;
        Ok(replay)
    }

    fn initialize(database: Connection, limit: usize, ttl: Duration) -> Result<Self> {
        let kibibytes = limit.div_ceil(1024).clamp(1, i32::MAX as usize) as i32;
        database.pragma_update(None, "cache_size", -kibibytes)?;
        database
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS replay (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                expires INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS replay_expiry ON replay(expires);
            CREATE TABLE IF NOT EXISTS replay_links (
                source TEXT NOT NULL,
                target TEXT NOT NULL,
                PRIMARY KEY (source, target)
            );
            CREATE TABLE IF NOT EXISTS replay_meta (key TEXT PRIMARY KEY);",
            )
            .context("initialize replay schema")?;
        Ok(Self { database, ttl })
    }

    pub fn insert(&mut self, key: String, part: Value) -> Result<()> {
        let expires = self.expires()?;
        let transaction = self
            .database
            .transaction()
            .context("begin replay entry transaction")?;
        write_entry(&transaction, &key, &part, expires)?;
        transaction
            .commit()
            .with_context(|| format!("commit replay entry {key}"))
    }

    pub fn get(&mut self, key: &str) -> Result<Value> {
        let value = self.read_entry(key)?;
        renew_entry(&self.database, key, self.expires()?)?;
        Ok(value)
    }

    fn read_entry(&self, key: &str) -> Result<Value> {
        let now = now_seconds()?;
        let value: Option<String> = self
            .database
            .prepare_cached("SELECT value FROM replay WHERE key = ?1 AND expires > ?2")?
            .query_row(params![key, now], |row| row.get(0))
            .optional()
            .with_context(|| format!("read replay entry {key}"))?;
        let value =
            value.context("provider replay state expired or missing; start a new conversation")?;
        serde_json::from_str(&value).with_context(|| format!("parse replay entry {key}"))
    }

    pub fn response_history(&mut self, id: &str) -> Result<Vec<Value>> {
        self.get(&format!("response:{id}"))
            .with_context(|| format!("previous response {id} expired or is unknown"))?;
        let mut history = Vec::new();
        let mut current = Some(id.to_owned());
        while let Some(id) = current {
            let response = self
                .read_entry(&format!("response:{id}"))
                .with_context(|| format!("previous response {id} expired or is unknown"))?;
            current = response["parent"].as_str().map(str::to_owned);
            let mut items = Vec::new();
            for key in response["item_ids"]
                .as_array()
                .context("invalid previous response history")?
            {
                items.push(self.read_entry(key.as_str().context("invalid history item id")?)?);
            }
            history.push(items);
        }
        history.reverse();
        Ok(history.into_iter().flatten().collect())
    }

    pub fn store_response(
        &mut self,
        id: &str,
        parent: Option<&str>,
        items: Vec<Value>,
    ) -> Result<()> {
        let expires = self.expires()?;
        let now = now_seconds()?;
        let transaction = self
            .database
            .transaction()
            .context("begin replay response transaction")?;
        write_response(
            &transaction,
            &format!("response:{id}"),
            &json!({"parent":parent,"items":items}),
            expires,
        )?;
        renew_entry(&transaction, &format!("response:{id}"), expires)?;
        let removed = transaction
            .execute("DELETE FROM replay WHERE expires <= ?1", [now])
            .context("remove expired replay entries")?;
        if removed > 0 {
            transaction.execute(
                "DELETE FROM replay_links WHERE source NOT IN (SELECT key FROM replay)",
                [],
            )?;
        }
        transaction.commit().context("commit replay response")
    }

    fn expires(&self) -> Result<i64> {
        Ok(now_seconds()?.saturating_add(i64::try_from(self.ttl.as_secs()).unwrap_or(i64::MAX)))
    }

    fn import_legacy(&mut self, path: &Path) -> Result<()> {
        let imported: bool = self.database.query_row(
            "SELECT EXISTS(SELECT 1 FROM replay_meta WHERE key = 'legacy_imported')",
            [],
            |row| row.get(0),
        )?;
        if imported {
            return Ok(());
        }
        let transaction = self
            .database
            .transaction()
            .context("begin legacy replay import")?;
        if path.exists() {
            import_file(&transaction, path)?;
            retain_imported_history(&transaction)?;
        }
        transaction.execute(
            "INSERT INTO replay_meta(key) VALUES ('legacy_imported')",
            [],
        )?;
        transaction.commit().context("commit legacy replay import")
    }
}

fn now_seconds() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()
        .context("replay timestamp exceeds SQLite integer range")
}

fn write_entry(database: &Connection, key: &str, value: &Value, expires: i64) -> Result<()> {
    let changed = database.prepare_cached(
        "INSERT INTO replay(key, value, expires) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, expires = MAX(replay.expires, excluded.expires)
         WHERE replay.value != excluded.value",
    )?.execute(params![key, serde_json::to_string(value)?, expires])
        .with_context(|| format!("write replay entry {key}"))?;
    if changed > 0 {
        write_links(database, key, value)?;
    } else {
        database
            .prepare_cached("UPDATE replay SET expires = ?2 WHERE key = ?1 AND expires < ?2")?
            .execute(params![key, expires])
            .with_context(|| format!("renew replay entry {key}"))?;
    }
    Ok(())
}

fn write_links(database: &Connection, key: &str, value: &Value) -> Result<()> {
    let targets = entry_targets(key, value);
    database
        .prepare_cached("DELETE FROM replay_links WHERE source = ?1")?
        .execute([key])?;
    for target in targets {
        database
            .prepare_cached("INSERT OR IGNORE INTO replay_links(source, target) VALUES (?1, ?2)")?
            .execute(params![key, target])?;
    }
    Ok(())
}

fn entry_targets(key: &str, value: &Value) -> Vec<String> {
    let mut targets = Vec::new();
    if key.starts_with("response:") {
        for id in value["item_ids"].as_array().into_iter().flatten() {
            if let Some(id) = id.as_str() {
                targets.push(id.to_owned());
            }
        }
        if let Some(parent) = value["parent"].as_str() {
            targets.push(format!("response:{parent}"));
        }
    } else if key.starts_with("item:") {
        for field in ["id", "call_id"] {
            if let Some(id) = value[field].as_str() {
                targets.extend([id.to_owned(), format!("tool_group:{id}")]);
            }
        }
    } else if key.starts_with("tool_group:") {
        if let Some(id) = value.as_str() {
            targets.extend([id.to_owned(), format!("group_outputs:{id}")]);
        }
    } else if key.starts_with("group_outputs:") {
        for item in value.as_array().into_iter().flatten() {
            if let Some(id) = item["call_id"].as_str() {
                targets.push(id.to_owned());
            }
        }
    }
    targets
}

fn renew_entry(database: &Connection, key: &str, expires: i64) -> Result<()> {
    database
        .prepare_cached(
            "WITH RECURSIVE retained(key) AS (
            SELECT ?1 UNION SELECT target FROM replay_links JOIN retained ON source = retained.key
         ) UPDATE replay SET expires = ?2 WHERE key IN retained AND expires < ?2",
        )?
        .execute(params![key, expires])
        .with_context(|| format!("retain replay history {key}"))?;
    Ok(())
}

fn write_response(database: &Connection, key: &str, response: &Value, expires: i64) -> Result<()> {
    let items = response["items"]
        .as_array()
        .context("invalid response history")?;
    let mut keys = Vec::with_capacity(items.len());
    for item in items {
        keys.push(write_item(database, item, expires)?);
    }
    write_entry(
        database,
        key,
        &json!({"parent":response["parent"],"item_ids":keys}),
        expires,
    )
}

fn write_item(database: &Connection, item: &Value, expires: i64) -> Result<String> {
    let serialized = serde_json::to_string(item)?;
    let key = format!("item:{:x}", Sha256::digest(serialized.as_bytes()));
    let inserted = database.prepare_cached(
        "INSERT INTO replay(key, value, expires) VALUES (?1, ?2, ?3) ON CONFLICT(key) DO NOTHING",
    )?.execute(params![key, serialized, expires])
        .with_context(|| format!("write history item {key}"))?;
    if inserted > 0 {
        write_links(database, &key, item)?;
    }
    // Existing items are renewed together with their response's dependency graph.
    Ok(key)
}

fn import_batch(database: &Connection, batch: &str) -> Result<()> {
    let entries: Vec<(String, Value, u64)> =
        serde_json::from_str(batch).context("parse legacy replay batch")?;
    let now = now_seconds()?;
    for (key, value, expires) in entries {
        let expires = i64::try_from(expires).unwrap_or(i64::MAX);
        if expires <= now {
            continue;
        }
        if key.starts_with("response:") {
            write_response(database, &key, &value, expires)?;
        } else {
            write_entry(database, &key, &value, expires)?;
        }
    }
    Ok(())
}

fn import_file(database: &Connection, path: &Path) -> Result<()> {
    let mut reader = BufReader::new(
        std::fs::File::open(path)
            .with_context(|| format!("read legacy replay store {}", path.display()))?,
    );
    let mut first = String::new();
    reader.read_line(&mut first)?;
    if first == REPLAY_HEADER {
        import_journal(database, reader, path)
    } else {
        use std::io::Read;
        reader.read_to_string(&mut first)?;
        import_batch(database, &first)
    }
}

fn retain_imported_history(database: &Connection) -> Result<()> {
    let mut statement =
        database.prepare("SELECT key, expires FROM replay WHERE key LIKE 'response:%'")?;
    let responses = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (key, expires) in responses {
        renew_entry(database, &key, expires)?;
    }
    Ok(())
}

fn import_journal(database: &Connection, mut reader: impl BufRead, path: &Path) -> Result<()> {
    let mut batch = String::new();
    loop {
        batch.clear();
        if reader.read_line(&mut batch)? == 0 {
            return Ok(());
        }
        if !batch.ends_with('\n') {
            eprintln!(
                "replay store {}: discard incomplete final batch",
                path.display()
            );
            return Ok(());
        }
        ensure!(!batch.trim().is_empty(), "empty legacy replay batch");
        import_batch(database, &batch)
            .with_context(|| format!("import replay store {}", path.display()))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_history_renews_ttl_without_rewriting_payloads_or_links() {
        let mut replay = Replay::new(1024, Duration::from_secs(60));
        let part = json!([{"text":"answer","thoughtSignature":"signed"}]);
        let item = json!({"id":"ag_answer","type":"message","role":"assistant","content":"answer"});
        replay.insert("ag_answer".into(), part.clone()).unwrap();
        replay
            .store_response("first", None, vec![item.clone()])
            .unwrap();
        replay.database.execute_batch(
            "CREATE TRIGGER reject_redundant_payload BEFORE UPDATE OF value ON replay
             WHEN OLD.value = NEW.value BEGIN SELECT RAISE(ABORT, 'redundant payload write'); END;
             CREATE TRIGGER reject_item_link_deletion BEFORE DELETE ON replay_links
             WHEN OLD.source LIKE 'item:%' BEGIN SELECT RAISE(ABORT, 'redundant item link write'); END;",
        ).unwrap();
        replay.insert("ag_answer".into(), part.clone()).unwrap();
        replay
            .database
            .execute(
                "UPDATE replay SET expires = 0 WHERE key LIKE 'item:%' OR key = 'ag_answer'",
                [],
            )
            .unwrap();
        replay
            .store_response("second", Some("first"), vec![item.clone()])
            .unwrap();
        assert_eq!(replay.get("ag_answer").unwrap(), part);
        assert_eq!(
            replay.response_history("second").unwrap(),
            vec![item.clone(), item]
        );
    }

    #[test]
    fn changed_replay_payload_replaces_its_dependency_links() {
        let mut replay = Replay::new(1024, Duration::from_secs(60));
        replay
            .insert("tool_group:call".into(), json!("ag_old"))
            .unwrap();
        replay
            .insert("tool_group:call".into(), json!("ag_new"))
            .unwrap();
        assert_eq!(replay.get("tool_group:call").unwrap(), json!("ag_new"));
        let mut statement = replay
            .database
            .prepare(
                "SELECT target FROM replay_links WHERE source = 'tool_group:call' ORDER BY target",
            )
            .unwrap();
        let targets = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(targets, vec!["ag_new", "group_outputs:ag_new"]);
    }
}
