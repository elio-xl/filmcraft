//! Discardable model claims grounded in retained, versioned source evidence.
use super::{
    cache::{self, Page, ResultCache},
    request,
    source::{AnalysisInput, AnalysisSource, bounds, failure},
};
use crate::{Result, Session};
use filmcraft_project::ItemId;
use filmcraft_time::Tick;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(serde::Serialize)]
pub struct SetRequest {
    pub item: ItemId,
    pub request_id: Option<String>,
    pub source_version: String,
    start: Tick,
    end: Tick,
    summary: String,
    tags: Vec<String>,
    evidence_frame_ids: Vec<String>,
    model_label: String,
}
#[derive(serde::Serialize)]
pub struct SearchRequest {
    pub request_id: Option<String>,
    query: String,
    pub(super) limit: usize,
    #[serde(skip)]
    pub(super) records: Vec<Arc<Record>>,
    #[serde(skip)]
    pub(super) inputs: Vec<(ItemId, AnalysisInput)>,
    #[serde(skip)]
    pub(super) unavailable: Vec<Value>,
}
fn fields<'a>(p: &'a Value, allowed: &[&str]) -> Result<&'a Map<String, Value>> {
    let map = p.as_object().ok_or_else(|| failure("INVALID_PARAMS", "expected summary parameters object", false))?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(failure("INVALID_PARAMS", "unknown summary parameter", false));
    }
    Ok(map)
}
fn text(value: Option<&Value>, max: usize) -> Result<String> {
    let value = value
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty() && v.len() <= max * 4 && v.chars().count() <= max)
        .ok_or_else(|| failure("INVALID_PARAMS", "text is missing, empty or exceeds its character budget", false))?;
    Ok(value.to_owned())
}
fn strings(value: Option<&Value>, max: usize, chars: usize, nonempty: bool) -> Result<Vec<String>> {
    let list = value
        .and_then(Value::as_array)
        .filter(|a| a.len() <= max && (!nonempty || !a.is_empty()))
        .ok_or_else(|| failure("INVALID_PARAMS", "invalid or oversized summary list", false))?;
    list.iter().map(|v| text(Some(v), chars)).collect()
}
pub fn set(s: &mut Session, p: &Value) -> Result<Value> {
    let f = fields(p, &["item", "request_id", "source_version", "start_ticks", "end_ticks", "summary", "tags", "evidence_frame_ids", "model_label"])?;
    let common = request::common(f, "media.analysis.summary.set")?;
    let source_version = text(f.get("source_version"), 71)?;
    if source_version.len() != 71
        || !source_version.starts_with("sha256:")
        || !source_version.bytes().skip(7).all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(failure("INVALID_PARAMS", "source_version must be the SHA256 version returned by analysis", false));
    }
    let start = request::tick(f.get("start_ticks").unwrap_or(&Value::Null))?;
    let end = request::tick(f.get("end_ticks").unwrap_or(&Value::Null))?;
    if start >= end {
        return Err(failure("INVALID_RANGE", "summary range must be positive", false));
    }
    let r = SetRequest {
        item: common.item,
        request_id: common.request_id,
        source_version,
        start,
        end,
        summary: text(f.get("summary"), 4096)?,
        tags: strings(f.get("tags"), 32, 64, false)?,
        evidence_frame_ids: strings(f.get("evidence_frame_ids"), 16, 128, true)?,
        model_label: text(f.get("model_label"), 128)?,
    };
    super::start(s, super::jobs::Request::SummarySet(r))
}
pub fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let f = fields(p, &["request_id", "query", "limit"])?;
    let query = text(f.get("query"), 512)?;
    if normalize(&query).is_empty() {
        return Err(failure("INVALID_PARAMS", "query must contain words", false));
    }
    let limit = request::bounded_integer(f, "limit", 20, 100)? as usize;
    let request_id = request::identifier(f, "request_id", 128, "media.analysis.summary.search")?;
    let mut work =
        super::jobs::Request::SummarySearch(SearchRequest { request_id, query, limit, records: Vec::new(), inputs: Vec::new(), unavailable: Vec::new() });
    if let Some(reply) = s.analysis.lookup(&work)? {
        return Ok(reply);
    }
    if let super::jobs::Request::SummarySearch(r) = &mut work {
        r.records = s.analysis.summary_records().into_iter().filter(|record| record.score(&r.query).is_some()).collect();
        let items = r.records.iter().map(|r| r.item).collect::<BTreeSet<_>>();
        if items.len() > 256 {
            return Err(failure("RESOURCE_LIMIT", "query matches more than 256 sources; narrow the query", false));
        }
        for item in items {
            match AnalysisInput::capture(s, item) {
                Ok(input) => r.inputs.push((item, input)),
                Err(error) => r.unavailable.push(json!({"item":item.0,"reason":error.to_string()})),
            }
        }
    }
    super::start(s, work)
}
fn normalize(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric()).filter(|s| !s.is_empty()).map(str::to_lowercase).collect::<Vec<_>>().join(" ")
}
pub(super) struct Record {
    pub item: ItemId,
    pub version: String,
    pub start: Tick,
    pub end: Tick,
    pub evidence_pages: Vec<String>,
    id: String,
    value: Value,
    bytes: usize,
    normalized_text: String,
    normalized_tags: BTreeSet<String>,
}
impl Record {
    pub fn score(&self, query: &str) -> Option<(usize, bool)> {
        let query = normalize(query);
        let words = query.split_whitespace().collect::<Vec<_>>();
        let tags = self.normalized_tags.iter().filter(|tag| words.iter().any(|word| tag.split_whitespace().any(|part| part == *word))).count();
        let text = self.normalized_text.contains(&query);
        (tags > 0 || text).then_some((tags, text))
    }
    pub fn value(&self) -> Value {
        self.value.clone()
    }
}
pub(super) fn prepare(source: &AnalysisSource, r: &SetRequest, cache: &mut ResultCache) -> Result<(Arc<Record>, Vec<Arc<Page>>)> {
    let (a, b) = bounds(source.available)?;
    if r.start < a || r.end > b {
        return Err(failure("INVALID_RANGE", "summary range lies outside the source", false));
    }
    let (keys, leases) = cache.evidence(r.item, &r.source_version, (r.start, r.end), &r.evidence_frame_ids)?;
    let id = format!("summary:{}", cache::key(&json!([r.item, r.source_version, r.start, r.end, r.summary, r.tags, r.evidence_frame_ids, r.model_label]))?);
    let value = json!({"id":id,"item":r.item.0,"root_item":source.root_item.0,"source_version":r.source_version,
        "start_ticks":r.start.0.to_string(),"end_ticks":r.end.0.to_string(),"origin":"model","summary":r.summary,
        "tags":r.tags,"evidence_frame_ids":r.evidence_frame_ids,"model_label":r.model_label});
    let bytes = cache::serialized_size(&value)?;
    Ok((
        Arc::new(Record {
            item: r.item,
            version: r.source_version.clone(),
            start: r.start,
            end: r.end,
            evidence_pages: keys,
            id,
            value,
            bytes,
            normalized_text: normalize(&r.summary),
            normalized_tags: r.tags.iter().map(|tag| normalize(tag)).collect(),
        }),
        leases,
    ))
}
pub(super) fn ranked(r: &SearchRequest, records: Vec<Arc<Record>>) -> Vec<Value> {
    let mut rows = records.into_iter().filter_map(|record| record.score(&r.query).map(|score| (record, score))).collect::<Vec<_>>();
    rows.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then(a.start.cmp(&b.start)).then(a.item.cmp(&b.item)).then(a.id.cmp(&b.id)));
    rows.into_iter()
        .take(r.limit)
        .map(|(record, (tags, text))| {
            let mut value = record.value();
            value["match"] = json!({"tag_hits":tags,"text_match":text});
            value
        })
        .collect()
}
#[derive(Default)]
pub(super) struct Index {
    records: BTreeMap<String, Arc<Record>>,
    bytes: usize,
}
impl Index {
    pub fn records(&self) -> Vec<Arc<Record>> {
        self.records.values().cloned().collect()
    }
    pub fn prune(&mut self, cache: &ResultCache) {
        self.records.retain(|_, r| r.evidence_pages.iter().all(|key| cache.peek(key).is_some()));
        self.bytes = self.records.values().map(|r| r.bytes).sum();
    }
    pub fn check_capacity(&self, r: &Record) -> Result<()> {
        if self.records.contains_key(&r.id) {
            return Ok(());
        }
        if self.records.len() >= 4096
            || self.records.values().filter(|row| row.item == r.item).count() >= 1000
            || self.bytes.saturating_add(r.bytes) > 32 * 1024 * 1024
        {
            return Err(failure("RESOURCE_LIMIT", "summary index budget reached (1000/item,4096/session,32 MiB); clear unused analysis data", false));
        }
        Ok(())
    }
    pub fn insert(&mut self, r: Arc<Record>) {
        if !self.records.contains_key(&r.id) {
            self.bytes = self.bytes.saturating_add(r.bytes);
            self.records.insert(r.id.clone(), r);
        }
    }
    pub fn clear_unused(&mut self) -> usize {
        let before = self.records.len();
        self.records.retain(|_, r| Arc::strong_count(r) > 1);
        self.bytes = self.records.values().map(|r| r.bytes).sum();
        before - self.records.len()
    }
}
