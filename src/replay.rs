use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct Replay {
    entries: VecDeque<(String, Value, Instant, usize)>,
    bytes: usize,
    limit: usize,
    ttl: Duration,
}

impl Replay {
    pub fn new(limit: usize, ttl: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            limit,
            ttl,
        }
    }

    pub fn insert(&mut self, key: String, part: Value) -> Result<()> {
        let bytes = key.len() + serde_json::to_vec(&part)?.len();
        ensure!(
            bytes <= self.limit,
            "provider part exceeds replay state limit"
        );
        self.prune();
        if let Some(index) = self.entries.iter().position(|(id, _, _, _)| id == &key) {
            let (_, _, _, removed) = self.entries.remove(index).context("missing replay entry")?;
            self.bytes -= removed;
        }
        while self.bytes + bytes > self.limit || self.entries.len() >= 4096 {
            if let Some((_, _, _, removed)) = self.entries.pop_front() {
                self.bytes -= removed;
            }
        }
        self.entries.push_back((key, part, Instant::now(), bytes));
        self.bytes += bytes;
        Ok(())
    }

    pub fn get(&mut self, key: &str) -> Result<Value> {
        self.prune();
        // ponytail: linear lookup capped at 4096 entries; index by ID if this becomes a bottleneck.
        let index = self
            .entries
            .iter()
            .position(|(id, _, _, _)| id == key)
            .context("provider replay state expired or missing; start a new conversation")?;
        let (id, part, _, bytes) = self.entries.remove(index).context("missing replay entry")?;
        let value = part.clone();
        self.entries.push_back((id, part, Instant::now(), bytes));
        Ok(value)
    }

    fn prune(&mut self) {
        while self
            .entries
            .front()
            .is_some_and(|(_, _, time, _)| time.elapsed() >= self.ttl)
        {
            if let Some((_, _, _, bytes)) = self.entries.pop_front() {
                self.bytes -= bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replacing_provider_state_reuses_memory_budget() {
        let mut replay = Replay::new(20, Duration::from_secs(60));
        replay.insert("call".into(), json!("old")).unwrap();
        replay.insert("call".into(), json!("new")).unwrap();
        assert_eq!(replay.entries.len(), 1);
        assert_eq!(replay.get("call").unwrap(), json!("new"));
        assert_eq!(replay.bytes, 9);
    }

    #[test]
    fn reading_provider_state_renews_ttl_and_expired_state_is_removed() {
        let mut replay = Replay::new(1024, Duration::from_secs(60));
        replay.insert("expired".into(), json!("old")).unwrap();
        replay.insert("active".into(), json!("signed")).unwrap();
        replay.entries[0].2 = Instant::now() - Duration::from_secs(61);
        replay.entries[1].2 = Instant::now() - Duration::from_secs(30);
        let before_read = Instant::now();
        assert_eq!(replay.get("active").unwrap(), json!("signed"));
        assert!(replay.get("expired").is_err());
        assert_eq!(replay.entries.len(), 1);
        assert!(replay.entries[0].2 >= before_read);
    }
}
