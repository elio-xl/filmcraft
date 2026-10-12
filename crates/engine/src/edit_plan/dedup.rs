//! Session-scoped commit receipts. Never evict a successful commit to make room.
use crate::Result;
use crate::media_analysis::{cache, source::failure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
const MAX_COMMITS: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;
#[derive(Default)]
pub(super) struct Receipts {
    entries: BTreeMap<String, (Value, usize)>,
    bytes: usize,
}
impl Receipts {
    pub fn get(&self, id: &str) -> Option<&Value> {
        self.entries.get(id).map(|(v, _)| v)
    }
    /// Reserve and insert before the project changes, while holding the same queue mutex.
    /// The caller removes this reservation if Session::edit returns an error.
    pub fn record(&mut self, id: &str, hash: &str, result: &Value) -> Result<()> {
        if self.entries.contains_key(id) {
            return Err(failure("REQUEST_ID_CONFLICT", "commit receipt already exists", false));
        }
        let bytes = cache::serialized_size(&json!({"request_id":id,"params_hash":hash,"result":result}))?;
        let total = self.bytes.checked_add(bytes).ok_or_else(|| failure("DEDUP_CAPACITY", "receipt byte count overflow", false))?;
        if self.entries.len() >= MAX_COMMITS || total > MAX_BYTES {
            return Err(failure(
                "DEDUP_CAPACITY",
                "committed receipt capacity reached (1024 requests / 8 MiB); read the project before starting a new session",
                false,
            ));
        }
        self.entries.insert(id.to_owned(), (result.clone(), bytes));
        self.bytes = total;
        Ok(())
    }
    pub fn rollback(&mut self, id: &str) {
        if let Some((_, bytes)) = self.entries.remove(id) {
            self.bytes = self.bytes.saturating_sub(bytes);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_plan_dedup_byte_capacity_preserves_old_receipts() {
        let mut store = Receipts::default();
        let report = json!({"committed":true,"details":"x".repeat(128*1024)});
        let mut inserted = 0;
        for n in 0..100 {
            match store.record(&format!("request-{n}"), "hash", &report) {
                Ok(()) => inserted += 1,
                Err(error) => {
                    assert!(error.to_string().contains("DEDUP_CAPACITY"));
                    break;
                }
            }
        }
        assert!((1..100).contains(&inserted));
        assert!(store.bytes <= MAX_BYTES);
        assert_eq!(store.get("request-0"), Some(&report));
        let bytes = store.bytes;
        store.rollback("request-0");
        assert!(store.bytes < bytes);
        store.record("replacement", "hash", &report).unwrap();
    }
}
