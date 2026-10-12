//! One cooperative worker per session; the worker never owns or locks a Session.

use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use web_time::Instant;

use super::{
    AnalysisFailure,
    cache::{self, Page, ResultCache},
    request::{AnalysisRequest, InspectRequest},
    source::{AnalysisInput, failure},
};
use crate::{EngineError, Result};

const MAX_JOBS: usize = 10_000;
#[derive(serde::Serialize)]
#[serde(tag = "operation", content = "params")]
pub enum Request {
    Inspect(InspectRequest),
    Frames(AnalysisRequest),
    Preview(super::preview::Request),
    Shots(super::shots::Request),
    Transcript(super::transcripts::Request),
    Audio(super::transcripts::Request),
    SummarySet(super::summaries::SetRequest),
    SummarySearch(super::summaries::SearchRequest),
}
impl Request {
    fn request_id(&self) -> Option<&str> {
        match self {
            Self::Inspect(r) => r.request_id.as_deref(),
            Self::Frames(r) => r.request_id.as_deref(),
            Self::Preview(r) => r.request_id.as_deref(),
            Self::Shots(r) => r.request_id.as_deref(),
            Self::Transcript(r) | Self::Audio(r) => r.request_id.as_deref(),
            Self::SummarySet(r) => r.request_id.as_deref(),
            Self::SummarySearch(r) => r.request_id.as_deref(),
        }
    }
    fn expected_version(&self) -> Option<&str> {
        match self {
            Self::Inspect(r) => r.expected_version.as_deref(),
            Self::Frames(r) => r.expected_version.as_deref(),
            Self::Preview(r) => r.expected_version.as_deref(),
            Self::Shots(r) => r.expected_version.as_deref(),
            Self::Transcript(r) | Self::Audio(r) => r.expected_version.as_deref(),
            Self::SummarySet(r) => Some(&r.source_version),
            Self::SummarySearch(_) => None,
        }
    }
    pub fn item(&self) -> Option<filmcraft_project::ItemId> {
        match self {
            Self::Inspect(r) => Some(r.item),
            Self::Frames(r) => Some(r.item),
            Self::Preview(r) => Some(r.item),
            Self::Shots(r) => Some(r.item),
            Self::Transcript(r) | Self::Audio(r) => Some(r.item),
            Self::SummarySet(r) => Some(r.item),
            Self::SummarySearch(_) => None,
        }
    }
    fn canonical(&self, include_version: bool) -> Result<Value> {
        let mut value = serde_json::to_value(self).map_err(|e| failure("INTERNAL_ERROR", e.to_string(), false))?;
        let params = value.get_mut("params").and_then(Value::as_object_mut).ok_or_else(|| failure("INTERNAL_ERROR", "invalid typed request", false))?;
        params.remove("request_id");
        if !include_version {
            params.remove("expected_version");
        }
        Ok(value)
    }
}
struct Work {
    id: u64,
    input: Option<AnalysisInput>,
    request: Request,
}
#[derive(Clone)]
struct Job {
    id: u64,
    request_id: String,
    status: String,
    cancel: Arc<AtomicBool>,
    finished: bool,
    failure: Option<AnalysisFailure>,
    result_key: Option<String>,
    cache_hit: bool,
    timings: Value,
}
struct Outcome {
    artifact: Option<(String, Arc<Vec<u8>>)>,
    key: String,
    page: Arc<Page>,
    cache_hit: bool,
    timings: Value,
    summary: Option<Arc<super::summaries::Record>>,
    _evidence: Vec<Arc<Page>>,
}
#[derive(Default)]
struct State {
    worker: bool,
    active: Option<u64>,
    pending: VecDeque<Work>,
    jobs: BTreeMap<u64, Job>,
    receipts: BTreeMap<String, (u64, String)>,
    cache: ResultCache,
    artifacts: cache::ArtifactCache,
    summaries: super::summaries::Index,
}
impl State {
    fn available(&self, key: &str) -> bool {
        self.cache.peek(key).is_some_and(|page| self.page_available(page))
    }
    fn page_available(&self, page: &Page) -> bool {
        page.value.get("preview").and_then(|p| p.get("artifact_id")).and_then(Value::as_str).is_none_or(|id| self.artifacts.contains(id))
    }
}
#[derive(Default)]
pub struct AnalysisQueue {
    inner: Arc<Mutex<State>>,
}
fn receipt(state: &State, request_id: Option<&str>, fingerprint: &str) -> Result<Option<Value>> {
    let Some((id, original)) = request_id.and_then(|key| state.receipts.get(key)) else { return Ok(None) };
    if original != fingerprint {
        return Err(failure("REQUEST_ID_CONFLICT", "request_id was already used with different parameters", false));
    }
    let job = state.jobs.get(id).ok_or_else(|| failure("INTERNAL_ERROR", "missing analysis receipt job", false))?;
    if job.finished && job.failure.is_none() && job.result_key.as_ref().is_none_or(|key| !state.available(key)) {
        return Err(failure("RESULT_EXPIRED", "original result expired; submit an explicit new request_id", false));
    }
    Ok(Some(json!({"job":id,"request_id":job.request_id})))
}
impl AnalysisQueue {
    pub fn lookup(&self, request: &Request) -> Result<Option<Value>> {
        let fingerprint = cache::key(&request.canonical(true)?)?;
        receipt(&self.inner.lock().unwrap_or_else(PoisonError::into_inner), request.request_id(), &fingerprint)
    }
    pub fn submit(&self, id: u64, input: Option<AnalysisInput>, request: Request) -> Result<Value> {
        if cfg!(target_arch = "wasm32") {
            return Err(failure("CAPABILITY_UNAVAILABLE", "media analysis requires a native worker host", false));
        }
        let fingerprint = cache::key(&request.canonical(true)?)?;
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(reply) = receipt(&state, request.request_id(), &fingerprint)? {
            return Ok(reply);
        }
        if state.pending.len() + usize::from(state.active.is_some()) >= 9 {
            return Err(failure("ANALYSIS_BUSY", "one analysis is active and eight requests are queued", true));
        }
        if state.jobs.len() >= MAX_JOBS {
            return Err(failure("RESOURCE_LIMIT", "session analysis receipt capacity reached; open a new session", false));
        }
        if id == 0 || state.jobs.contains_key(&id) {
            return Err(failure("INTERNAL_ERROR", "invalid or duplicate analysis job id", false));
        }
        let request_id = match request.request_id() {
            Some(id) => id.to_owned(),
            None => (0..=MAX_JOBS)
                .map(|suffix| format!("analysis-{id}-{suffix}"))
                .find(|key| !state.receipts.contains_key(key))
                .ok_or_else(|| failure("RESOURCE_LIMIT", "automatic request ids exhausted", false))?,
        };
        let reply = json!({"job":id,"request_id":request_id});
        state.receipts.insert(request_id.clone(), (id, fingerprint));
        state.jobs.insert(
            id,
            Job {
                id,
                request_id,
                status: "queued".into(),
                cancel: Arc::new(AtomicBool::new(false)),
                finished: false,
                failure: None,
                result_key: None,
                cache_hit: false,
                timings: Value::Null,
            },
        );
        state.pending.push_back(Work { id, input, request });
        if !state.worker {
            state.worker = true;
            let inner = self.inner.clone();
            if let Err(error) = std::thread::Builder::new().name("filmcraft-media-analysis".into()).spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&inner))).is_err() {
                    let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
                    let ids: Vec<_> = state.jobs.values().filter(|job| !job.finished).map(|job| job.id).collect();
                    for id in ids {
                        finish(&mut state, id, Err(failure("WORKER_PANIC", "analysis worker failed; the session remains usable", false)));
                    }
                    state.pending.clear();
                    state.active = None;
                    state.worker = false;
                }
            }) {
                state.pending.clear();
                state.worker = false;
                finish(&mut state, id, Err(failure("WORKER_UNAVAILABLE", error.to_string(), true)));
            }
        }
        Ok(reply)
    }
    pub fn result(&self, id: u64) -> Result<Value> {
        let (job, page) = {
            let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let job = state.jobs.get(&id).cloned().ok_or_else(|| failure("JOB_NOT_FOUND", "analysis job does not exist", false))?;
            if !job.finished {
                return Ok(json!({"schema_version":1,"job":id,"request_id":job.request_id,"status":job.status,"finished":false}));
            }
            if let Some(error) = &job.failure {
                return Ok(json!({"schema_version":1,"job":id,"request_id":job.request_id,"status":job.status,"finished":true,"error":error}));
            }
            let page = job
                .result_key
                .as_ref()
                .and_then(|key| state.cache.get(key))
                .ok_or_else(|| failure("RESULT_EXPIRED", "analysis result expired; submit an explicit new request", false))?;
            if !state.page_available(&page) {
                return Err(failure("RESULT_EXPIRED", "preview artifact expired; submit a new request", false));
            }
            (job, page)
        };
        // The lease protects this page from eviction while copying, without holding the queue lock.
        let mut value = page.value.clone();
        value["job"] = json!(id);
        value["request_id"] = json!(job.request_id);
        value["finished"] = json!(true);
        value["cache_hit"] = json!(job.cache_hit);
        value["timings_ms"] = job.timings;
        Ok(value)
    }
    pub fn read_artifact(&self, id: &str) -> Result<Value> {
        use base64::Engine;
        let data = self
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .artifacts
            .get(id)
            .ok_or_else(|| failure("RESULT_EXPIRED", "preview artifact is missing or expired", false))?;
        // The Arc lease prevents eviction while the bounded binary response is encoded.
        Ok(json!({"artifact_id":id,"mime_type":"video/mp4","bytes":data.len(),
            "uri":format!("filmcraft://analysis/{id}"),"data":base64::engine::general_purpose::STANDARD.encode(&*data)}))
    }
    pub fn list(&self) -> Vec<Value> {
        let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.jobs.values().map(|job|json!({"id":job.id,"request_id":job.request_id,"label":"Media analysis","kind":"media_analysis",
            "status":job.status,"finished":job.finished,"progress":if job.finished {1.0}else{0.0},"done":u8::from(job.finished),"total":1,
            "etaSeconds":null,"result":null,"result_expired":job.finished && job.failure.is_none() && job.result_key.as_ref().is_none_or(|key|!state.available(key))})).collect()
    }
    pub fn cancel(&self, id: u64) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(job) = state.jobs.get(&id) else { return false };
        if job.finished {
            return true;
        }
        job.cancel.store(true, Ordering::Relaxed);
        if let Some(index) = state.pending.iter().position(|work| work.id == id) {
            state.pending.remove(index);
            finish(&mut state, id, Err(failure("CANCELLED", "analysis was cancelled before it started", false)));
        }
        true
    }
    pub(super) fn summary_records(&self) -> Vec<Arc<super::summaries::Record>> {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let State { cache, summaries, .. } = &mut *state;
        summaries.prune(cache);
        summaries.records()
    }
    pub fn clear_cache(&self) -> Value {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let (removed, bytes) = state.cache.clear_unused();
        let (artifacts, video_bytes) = state.artifacts.clear_unused();
        let summaries = state.summaries.clear_unused();
        json!({"removed":removed,"bytes_removed":bytes,"artifacts_removed":artifacts,"video_bytes_removed":video_bytes,"summaries_removed":summaries})
    }
}
impl Drop for AnalysisQueue {
    fn drop(&mut self) {
        let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        for job in state.jobs.values().filter(|job| !job.finished) {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}
fn process(work: Work, cancel: &Arc<AtomicBool>, inner: &Arc<Mutex<State>>) -> Result<Outcome> {
    if matches!(&work.request, Request::SummarySearch(_)) {
        return process_search(work, cancel, inner);
    }
    let input = work.input.ok_or_else(|| failure("INTERNAL_ERROR", "source analysis is missing its input", false))?;
    let begin = Instant::now();
    let version = cache::input_version(&input, cancel)?;
    let hash_before = begin.elapsed().as_secs_f64() * 1000.0;
    if work.request.expected_version().is_some_and(|expected| expected != version) {
        return Err(failure("VERSION_MISMATCH", "source content does not match expected_version; inspect again before editing", false));
    }
    let key = cache::key(&json!({"version":version,"request":work.request.canonical(false)?,"context":input.cache_context()}))?;
    // Stored words may change independently of source content; preserve the submitted snapshot.
    let key = if matches!(&work.request, Request::Transcript(_) | Request::SummarySet(_)) { format!("snapshot-job:{}", work.id) } else { key };
    let cached = {
        let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.cache.get(&key).and_then(|page| {
            let artifact = match page.value.get("preview").and_then(|p| p.get("artifact_id")).and_then(Value::as_str) {
                Some(id) => Some((id.to_owned(), state.artifacts.get(id)?)),
                None => None,
            };
            Some((page, artifact))
        })
    };
    if let Some((page, artifact)) = cached {
        let begin = Instant::now();
        if cache::input_version(&input, cancel)? != version {
            return Err(failure("SOURCE_CHANGED", "source changed while validating cached output", true));
        }
        return Ok(Outcome {
            artifact,
            key,
            page,
            cache_hit: true,
            summary: None,
            _evidence: Vec::new(),
            timings: json!({"hash_before":hash_before,"open":0.0,"decode_or_cache":0.0,"hash_after":begin.elapsed().as_secs_f64()*1000.0}),
        });
    }
    let begin = Instant::now();
    let source = input.open()?;
    let open_ms = begin.elapsed().as_secs_f64() * 1000.0;
    let begin = Instant::now();
    let mut artifact = None;
    let mut summary = None;
    let mut evidence = Vec::new();
    let mut value = match &work.request {
        Request::Inspect(_) => super::source::inspect(&source)?,
        Request::Frames(request) => super::frames::decode(&source, request)?,
        Request::Shots(request) => super::shots::analyse(&source, request)?,
        Request::Transcript(request) => super::transcripts::transcript(&source, request)?,
        Request::Audio(request) => super::transcripts::audio(&source, request)?,
        Request::SummarySet(request) => {
            let (record, leases) = super::summaries::prepare(&source, request, &mut inner.lock().unwrap_or_else(PoisonError::into_inner).cache)?;
            let value = json!({"schema_version":1,"status":"complete","summary":record.value()});
            summary = Some(record);
            evidence = leases;
            value
        }
        Request::SummarySearch(_) => return Err(failure("INTERNAL_ERROR", "search must use the source inventory worker", false)),
        Request::Preview(request) => {
            let (mut value, bytes) = super::preview::generate(&source, request, cancel.clone())?;
            if let Some(bytes) = bytes {
                let id = format!("preview:{key}");
                value["preview"]["artifact_id"] = json!(id);
                value["preview"]["resource_uri"] = json!(format!("filmcraft://analysis/{id}"));
                artifact = Some((id, bytes));
            }
            value
        }
    };
    value["source_version"] = json!(version);
    let strategy = value.get("strategy").cloned().unwrap_or(Value::Null);
    if let Some(frames) = value.get_mut("frames").and_then(Value::as_array_mut) {
        for frame in frames {
            let time = frame.get("actual_ticks").and_then(Value::as_str).ok_or_else(|| failure("INTERNAL_ERROR", "frame has no source timestamp", false))?;
            let frame_id = cache::frame_id(&version, time, &frame["width"], &frame["height"])?;
            frame["frame_id"] = json!(frame_id);
            frame["item"] = json!(source.item.0);
            frame["source_version"] = json!(version);
            frame["strategy"] = strategy.clone();
        }
    }
    // Transient decode failures are retained per job; a new request retries the source.
    let key = if value.get("errors").and_then(Value::as_array).is_some_and(|errors| !errors.is_empty()) { format!("job:{}", work.id) } else { key };
    let page = Page::new(value)?;
    let decode_ms = begin.elapsed().as_secs_f64() * 1000.0;
    let begin = Instant::now();
    if cache::source_version(&source, cancel)? != version {
        return Err(failure("SOURCE_CHANGED", "source changed during analysis; all generated output was discarded", true));
    }
    Ok(Outcome {
        artifact,
        key,
        page,
        cache_hit: false,
        summary,
        _evidence: evidence,
        timings: json!({"hash_before":hash_before,"open":open_ms,"decode_or_cache":decode_ms,"hash_after":begin.elapsed().as_secs_f64()*1000.0}),
    })
}

fn process_search(work: Work, cancel: &Arc<AtomicBool>, inner: &Arc<Mutex<State>>) -> Result<Outcome> {
    use super::source::{bounds, check_cancelled};
    let Request::SummarySearch(mut request) = work.request else {
        return Err(failure("INTERNAL_ERROR", "invalid search work", false));
    };
    let begin = Instant::now();
    let mut leases = BTreeMap::new();
    let mut retained = Vec::new();
    {
        let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
        for record in &request.records {
            if !record.evidence_pages.iter().all(|key| state.cache.peek(key).is_some()) {
                continue;
            }
            for key in &record.evidence_pages {
                if let Some(page) = state.cache.get(key) {
                    leases.insert(key.clone(), page);
                }
            }
            retained.push(record.clone());
        }
    }
    let mut current = Vec::new();
    for (item, input) in std::mem::take(&mut request.inputs) {
        check_cancelled()?;
        let verified: Result<_> = (|| {
            let version = cache::input_version(&input, cancel)?;
            let source = input.open()?;
            let (a, b) = bounds(source.available)?;
            if cache::source_version(&source, cancel)? != version {
                return Err(failure("SOURCE_CHANGED", "source changed during search", true));
            }
            Ok((version, a, b))
        })();
        match verified {
            Ok((version, a, b)) => current.extend(retained.iter().filter(|r| r.item == item && r.version == version && r.start >= a && r.end <= b).cloned()),
            Err(error) => {
                check_cancelled()?;
                request.unavailable.push(json!({"item":item.0,"reason":error.to_string()}));
            }
        }
    }
    check_cancelled()?;
    let results = super::summaries::ranked(&request, current);
    let value = json!({"schema_version":1,"status":"complete","origin":"model","results":results,"unavailable_sources":request.unavailable});
    Ok(Outcome {
        artifact: None,
        key: format!("summary-search-job:{}", work.id),
        page: Page::new(value)?,
        cache_hit: false,
        timings: json!({"total":begin.elapsed().as_secs_f64()*1000.0}),
        summary: None,
        _evidence: leases.into_values().collect(),
    })
}

fn worker(inner: &Arc<Mutex<State>>) {
    loop {
        let (work, cancel) = {
            let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(work) = state.pending.pop_front() else {
                state.worker = false;
                state.active = None;
                return;
            };
            let Some(job) = state.jobs.get_mut(&work.id) else { continue };
            job.status = "running".into();
            let cancel = job.cancel.clone();
            state.active = Some(work.id);
            (work, cancel)
        };
        let id = work.id;
        let output = filmcraft_media::cancel::with_cancel(&cancel, || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filmcraft_media::cancel::with_background(true, || process(work, &cancel, inner))))
        })
        .unwrap_or_else(|_| Err(failure("WORKER_PANIC", "source analysis panicked; the session remains usable", false)));
        let output = if cancel.load(Ordering::Relaxed) { Err(failure("CANCELLED", "analysis was cancelled", false)) } else { output };
        let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
        finish(&mut state, id, output);
        state.active = None;
    }
}
fn finish(state: &mut State, id: u64, output: Result<Outcome>) {
    let output = output.and_then(|output| {
        if state.jobs.get(&id).is_some_and(|job| job.cancel.load(Ordering::Relaxed)) {
            return Err(failure("CANCELLED", "analysis was cancelled", false));
        }
        state.summaries.prune(&state.cache);
        if let Some(record) = &output.summary {
            state.summaries.check_capacity(record)?;
        }
        state.cache.insert(output.key.clone(), output.page.clone())?;
        if let Some((id, data)) = &output.artifact {
            state.artifacts.insert(id.clone(), data.clone())?;
        }
        if let Some(record) = &output.summary {
            state.summaries.insert(record.clone());
        }
        Ok(output)
    });
    let Some(job) = state.jobs.get_mut(&id) else { return };
    job.finished = true;
    match output {
        Ok(output) => {
            job.status = output.page.value["status"].as_str().unwrap_or("error").into();
            job.result_key = Some(output.key);
            job.cache_hit = output.cache_hit;
            job.timings = output.timings;
        }
        Err(error) => {
            let error = match error {
                EngineError::Analysis(error) => error,
                other => AnalysisFailure { code: "INTERNAL_ERROR".into(), message: other.to_string().chars().take(1024).collect(), retryable: false },
            };
            job.status = if error.code == "CANCELLED" { "cancelled" } else { "error" }.into();
            job.failure = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_capacity_failure_is_terminal_and_preserves_leased_results() {
        let queue = AnalysisQueue::default();
        let leases = {
            let mut state = queue.inner.lock().unwrap();
            for id in 1..=32 {
                state.cache.insert(id.to_string(), Page::new(json!({"status":"complete"})).unwrap()).unwrap();
            }
            state.jobs.insert(
                33,
                Job {
                    id: 33,
                    request_id: "r33".into(),
                    status: "running".into(),
                    cancel: Arc::new(AtomicBool::new(false)),
                    finished: false,
                    failure: None,
                    result_key: None,
                    cache_hit: false,
                    timings: Value::Null,
                },
            );
            (1..=32).map(|id| state.cache.get(&id.to_string()).unwrap()).collect::<Vec<_>>()
        };
        finish(
            &mut queue.inner.lock().unwrap(),
            33,
            Ok(Outcome {
                artifact: None,
                key: "33".into(),
                page: Page::new(json!({"status":"complete"})).unwrap(),
                cache_hit: false,
                timings: Value::Null,
                summary: None,
                _evidence: Vec::new(),
            }),
        );
        let page = queue.result(33).unwrap();
        assert_eq!(page["finished"], true);
        assert_eq!(page["error"]["code"], "RESOURCE_LIMIT");
        assert_eq!(leases.len(), 32);
        assert_eq!(queue.clear_cache()["removed"], 0);
    }
}
