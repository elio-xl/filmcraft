//! Complete content fingerprints and a byte-bounded LRU of encoded result pages.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::source::{AnalysisInput, AnalysisSource, SourceIdentity, failure, read_bytes, sequence_inventory};
use crate::{Result, Services};

pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const FRAME_PAGE_BYTES: usize = MAX_RESPONSE_BYTES - 256 * 1024;
const MAX_RESULTS: usize = 32;
const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;

fn cancelled(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Relaxed) || filmcraft_media::cancel::cancelled() {
        Err(failure("CANCELLED", "content hashing was cancelled", false))
    } else {
        Ok(())
    }
}
fn hash_file(services: &dyn Services, path: &str, flag: &AtomicBool) -> Result<[u8; 32]> {
    cancelled(flag)?;
    let mut hash = Sha256::new();
    if let Some(reader) = services.reader(path) {
        let reader = reader.map_err(super::source::io_failure)?;
        let len = reader.len();
        let mut block = vec![0u8; CHUNK];
        let mut offset = 0u64;
        while offset < len {
            cancelled(flag)?;
            let count = usize::try_from((len - offset).min(CHUNK as u64)).map_err(|_| failure("RESOURCE_LIMIT", "hash block length overflows", false))?;
            let part = block.get_mut(..count).ok_or_else(|| failure("INTERNAL_ERROR", "invalid hash block", false))?;
            reader.read_at(offset, part).map_err(super::source::io_failure)?;
            hash.update(&*part);
            offset = offset.checked_add(count as u64).ok_or_else(|| failure("RESOURCE_LIMIT", "hash offset overflows", false))?;
        }
    } else {
        let bytes = read_bytes(services, path)?;
        for part in bytes.chunks(CHUNK) {
            cancelled(flag)?;
            hash.update(part);
        }
    }
    cancelled(flag)?;
    Ok(hash.finalize().into())
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.as_ref().len().saturating_mul(2));
    for &byte in bytes.as_ref() {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    text
}
fn version(identity: &SourceIdentity, services: &dyn Services, flag: &AtomicBool) -> Result<String> {
    cancelled(flag)?;
    let bytes = match identity {
        SourceIdentity::File { path } => hash_file(services, path, flag)?,
        SourceIdentity::Generator { .. } => {
            let mut hash = Sha256::new();
            hash.update(b"filmcraft-generator-v1\0");
            let bytes = serde_json::to_vec(identity).map_err(|e| failure("INVALID_SOURCE", e.to_string(), false))?;
            hash.update(bytes);
            hash.finalize().into()
        }
        SourceIdentity::Sequence { first, rate, .. } => {
            let frames = sequence_inventory(services, first)?;
            let mut hash = Sha256::new();
            hash.update(b"filmcraft-image-sequence-v1\0");
            hash.update(rate.num.to_le_bytes());
            hash.update(rate.den.to_le_bytes());
            hash.update((frames.len() as u64).to_le_bytes());
            for path in frames {
                cancelled(flag)?;
                match path {
                    None => hash.update([0]),
                    Some(path) => {
                        hash.update([1]);
                        hash.update((path.len() as u64).to_le_bytes());
                        hash.update(path.as_bytes());
                        hash.update(hash_file(services, &path, flag)?);
                    }
                }
            }
            hash.finalize().into()
        }
    };
    Ok(format!("sha256:{}", hex(bytes)))
}
/// Hash current source contents; file bytes are read in 1 MiB blocks with cancellation.
pub fn source_version(source: &AnalysisSource, cancelled: &AtomicBool) -> Result<String> {
    version(&source.identity, &*source.services, cancelled)
}
/// Hash before opening the decoder, so an atomic path replacement cannot stamp old pixels with a new version.
pub(super) fn input_version(input: &AnalysisInput, cancelled: &AtomicBool) -> Result<String> {
    version(&input.identity, &*input.services, cancelled)
}
pub(super) fn key(value: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|e| failure("INTERNAL_ERROR", e.to_string(), false))?;
    Ok(hex(Sha256::digest(bytes)))
}
pub(super) fn frame_id(version: &str, time: &str, width: &Value, height: &Value) -> Result<String> {
    Ok(format!("frame:{}", key(&serde_json::json!([version, time, width, height]))?))
}
struct Size(usize);
impl Write for Size {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| std::io::Error::other("JSON length overflows"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn serialized_size(value: &Value) -> Result<usize> {
    let mut size = Size(0);
    serde_json::to_writer(&mut size, value).map_err(|e| failure("INTERNAL_ERROR", e.to_string(), false))?;
    Ok(size.0)
}
pub(super) struct Page {
    pub value: Value,
    bytes: usize,
}
impl Page {
    pub fn new(value: Value) -> Result<Arc<Self>> {
        let bytes = serialized_size(&value)?;
        if bytes > MAX_RESPONSE_BYTES {
            return Err(failure("RESOURCE_LIMIT", "analysis page exceeds the response budget", false));
        }
        Ok(Arc::new(Self { value, bytes }))
    }
}
#[derive(Default)]
pub(super) struct ResultCache {
    pages: BTreeMap<String, Arc<Page>>,
    order: VecDeque<String>,
    bytes: usize,
}
impl ResultCache {
    pub fn peek(&self, key: &str) -> Option<&Page> {
        self.pages.get(key).map(AsRef::as_ref)
    }
    #[cfg(test)]
    pub fn contains(&self, key: &str) -> bool {
        self.pages.contains_key(key)
    }
    pub fn get(&mut self, key: &str) -> Option<Arc<Page>> {
        let page = self.pages.get(key)?.clone();
        self.order.retain(|k| k != key);
        self.order.push_back(key.into());
        Some(page)
    }
    pub fn insert(&mut self, key: String, page: Arc<Page>) -> Result<()> {
        if self.pages.contains_key(&key) {
            return Ok(());
        }
        for _ in 0..self.order.len() {
            if self.pages.len() < MAX_RESULTS && self.bytes.saturating_add(page.bytes) <= MAX_CACHE_BYTES {
                break;
            }
            let Some(victim) = self.order.pop_front() else { break };
            if self.pages.get(&victim).is_some_and(|page| Arc::strong_count(page) > 1) {
                self.order.push_back(victim);
                continue;
            }
            if let Some(removed) = self.pages.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(removed.bytes);
            }
        }
        if self.pages.len() >= MAX_RESULTS || self.bytes.saturating_add(page.bytes) > MAX_CACHE_BYTES {
            return Err(failure("RESOURCE_LIMIT", "all analysis cache slots are in use; retry after consumers finish", true));
        }
        self.bytes = self.bytes.saturating_add(page.bytes);
        self.order.push_back(key.clone());
        self.pages.insert(key, page);
        Ok(())
    }
    pub fn clear_unused(&mut self) -> (usize, usize) {
        let before = (self.pages.len(), self.bytes);
        self.pages.retain(|_, page| Arc::strong_count(page) > 1);
        self.order.retain(|key| self.pages.contains_key(key));
        self.bytes = self.pages.values().map(|page| page.bytes).sum();
        (before.0 - self.pages.len(), before.1 - self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cache_eviction_keeps_active_results_and_uses_lru() {
        let mut cache = ResultCache::default();
        for id in 0..32 {
            cache.insert(id.to_string(), Page::new(json!({"id":id})).unwrap()).unwrap();
        }
        let active = cache.get("0").unwrap();
        cache.get("1").unwrap();
        cache.insert("32".into(), Page::new(json!({"id":32})).unwrap()).unwrap();
        assert!(cache.contains("0") && cache.contains("1") && !cache.contains("2"));
        assert_eq!(cache.clear_unused().0, 31);
        assert!(cache.contains("0"));
        drop(active);
        assert_eq!(cache.clear_unused().0, 1);
    }
    #[test]
    fn cache_capacity_failure_preserves_all_leased_results() {
        let mut cache = ResultCache::default();
        let mut leases = Vec::new();
        for id in 0..32 {
            cache.insert(id.to_string(), Page::new(json!({"id":id})).unwrap()).unwrap();
            leases.push(cache.get(&id.to_string()).unwrap());
        }
        assert!(matches!(cache.insert("33".into(),Page::new(json!({})).unwrap()),Err(crate::EngineError::Analysis(ref e)) if e.code=="RESOURCE_LIMIT"));
        assert_eq!(cache.clear_unused().0, 0);
        assert_eq!(leases.len(), 32);
    }
    #[test]
    fn cache_byte_budget_evicts_before_the_entry_count_limit() {
        let mut cache = ResultCache::default();
        // Synthetic sizes exercise the128MiB boundary without allocating128MiB in a unit test.
        let page = || Arc::new(Page { value: json!({}), bytes: MAX_CACHE_BYTES / 2 + 1 });
        cache.insert("a".into(), page()).unwrap();
        cache.insert("b".into(), page()).unwrap();
        assert!(!cache.contains("a") && cache.contains("b"));
        assert!(cache.bytes <= MAX_CACHE_BYTES);
        let active = cache.get("b").unwrap();
        assert!(cache.insert("c".into(), page()).is_err());
        assert!(cache.contains("b") && !cache.contains("c"));
        drop(active);
        let unicode = json!({"name":"素材", "escaped":"\\\""});
        assert_eq!(serialized_size(&unicode).unwrap(), serde_json::to_vec(&unicode).unwrap().len());
    }
}

/// Video payloads have a separate budget; result pages retain only IDs, never eviction leases.
#[derive(Default)]
pub(super) struct ArtifactCache {
    entries: BTreeMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    bytes: usize,
}
impl ArtifactCache {
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }
    pub fn get(&mut self, id: &str) -> Option<Arc<Vec<u8>>> {
        let data = self.entries.get(id)?.clone();
        self.order.retain(|key| key != id);
        self.order.push_back(id.into());
        Some(data)
    }
    pub fn insert(&mut self, id: String, data: Arc<Vec<u8>>) -> Result<()> {
        if self.entries.contains_key(&id) {
            return Ok(());
        }
        if data.len() as u64 > super::preview::MAX_VIDEO_BYTES {
            return Err(failure("RESOURCE_LIMIT", "preview exceeds the 24 MiB file budget", false));
        }
        for _ in 0..self.order.len() {
            if self.entries.len() < 16 && self.bytes.saturating_add(data.len()) <= 512 * 1024 * 1024 {
                break;
            }
            let Some(victim) = self.order.pop_front() else { break };
            if self.entries.get(&victim).is_some_and(|data| Arc::strong_count(data) > 1) {
                self.order.push_back(victim);
            } else if let Some(removed) = self.entries.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(removed.len());
            }
        }
        if self.entries.len() >= 16 || self.bytes.saturating_add(data.len()) > 512 * 1024 * 1024 {
            return Err(failure("RESOURCE_LIMIT", "all video artifact slots are in use", true));
        }
        self.bytes = self.bytes.saturating_add(data.len());
        self.order.push_back(id.clone());
        self.entries.insert(id, data);
        Ok(())
    }
    pub fn clear_unused(&mut self) -> (usize, usize) {
        let before = (self.entries.len(), self.bytes);
        self.entries.retain(|_, data| Arc::strong_count(data) > 1);
        self.order.retain(|key| self.entries.contains_key(key));
        self.bytes = self.entries.values().map(|data| data.len()).sum();
        (before.0 - self.entries.len(), before.1 - self.bytes)
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;
    #[test]
    fn video_cache_evicts_lru_but_never_active_readers() {
        let mut cache = ArtifactCache::default();
        for i in 0..16 {
            cache.insert(i.to_string(), Arc::new(vec![i])).unwrap();
        }
        let active = cache.get("0").unwrap();
        cache.get("1").unwrap();
        cache.insert("16".into(), Arc::new(vec![16])).unwrap();
        assert!(cache.contains("0") && cache.contains("1") && !cache.contains("2"));
        assert_eq!(cache.clear_unused(), (15, 15));
        assert_eq!(active.as_slice(), &[0]);
        assert!(cache.contains("0"));
        drop(active);
        assert_eq!(cache.clear_unused(), (1, 1));
    }
    #[test]
    fn full_video_cache_refuses_eviction_of_readers() {
        let mut cache = ArtifactCache::default();
        let mut leases = Vec::new();
        for i in 0..16 {
            cache.insert(i.to_string(), Arc::new(vec![i])).unwrap();
            leases.push(cache.get(&i.to_string()).unwrap());
        }
        assert!(cache.insert("17".into(), Arc::new(vec![17])).is_err());
        assert_eq!(cache.clear_unused(), (0, 0));
        assert_eq!(leases.len(), 16);
    }
}
