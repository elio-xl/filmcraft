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
}
impl Request {
    fn request_id(&self) -> Option<&str> {
        match self {
            Self::Inspect(r) => r.request_id.as_deref(),
            Self::Frames(r) => r.request_id.as_deref(),
        }
    }
    fn expected_version(&self) -> Option<&str> {
        match self {
            Self::Inspect(r) => r.expected_version.as_deref(),
            Self::Frames(r) => r.expected_version.as_deref(),
        }
    }
    pub fn item(&self) -> filmcraft_project::ItemId {
        match self {
            Self::Inspect(r) => r.item,
            Self::Frames(r) => r.item,
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
    input: AnalysisInput,
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
    key: String,
    page: Arc<Page>,
    cache_hit: bool,
    timings: Value,
}
#[derive(Default)]
struct State {
    worker: bool,
    active: Option<u64>,
    pending: VecDeque<Work>,
    jobs: BTreeMap<u64, Job>,
    receipts: BTreeMap<String, (u64, String)>,
    cache: ResultCache,
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
    if job.finished && job.failure.is_none() && job.result_key.as_ref().is_none_or(|key| !state.cache.contains(key)) {
        return Err(failure("RESULT_EXPIRED", "original result expired; submit an explicit new request_id", false));
    }
    Ok(Some(json!({"job":id,"request_id":job.request_id})))
}
impl AnalysisQueue {
    pub fn lookup(&self, request: &Request) -> Result<Option<Value>> {
        let fingerprint = cache::key(&request.canonical(true)?)?;
        receipt(&self.inner.lock().unwrap_or_else(PoisonError::into_inner), request.request_id(), &fingerprint)
    }
    pub fn submit(&self, id: u64, input: AnalysisInput, request: Request) -> Result<Value> {
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
    pub fn list(&self) -> Vec<Value> {
        let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.jobs.values().map(|job|json!({"id":job.id,"request_id":job.request_id,"label":"Media analysis","kind":"media_analysis",
            "status":job.status,"finished":job.finished,"progress":if job.finished {1.0}else{0.0},"done":u8::from(job.finished),"total":1,
            "etaSeconds":null,"result":null,"result_expired":job.finished && job.failure.is_none() && job.result_key.as_ref().is_none_or(|key|!state.cache.contains(key))})).collect()
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
    pub fn clear_cache(&self) -> Value {
        let (removed, bytes) = self.inner.lock().unwrap_or_else(PoisonError::into_inner).cache.clear_unused();
        json!({"removed":removed,"bytes_removed":bytes})
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
fn process(work: Work, cancel: &AtomicBool, inner: &Arc<Mutex<State>>) -> Result<Outcome> {
    let begin = Instant::now();
    let version = cache::input_version(&work.input, cancel)?;
    let hash_before = begin.elapsed().as_secs_f64() * 1000.0;
    if work.request.expected_version().is_some_and(|expected| expected != version) {
        return Err(failure("VERSION_MISMATCH", "source content does not match expected_version; inspect again before editing", false));
    }
    let key = cache::key(&json!({"version":version,"request":work.request.canonical(false)?,"context":work.input.cache_context()}))?;
    let cached = inner.lock().unwrap_or_else(PoisonError::into_inner).cache.get(&key);
    if let Some(page) = cached {
        let begin = Instant::now();
        if cache::input_version(&work.input, cancel)? != version {
            return Err(failure("SOURCE_CHANGED", "source changed while validating cached output", true));
        }
        return Ok(Outcome {
            key,
            page,
            cache_hit: true,
            timings: json!({"hash_before":hash_before,"open":0.0,"decode_or_cache":0.0,"hash_after":begin.elapsed().as_secs_f64()*1000.0}),
        });
    }
    let begin = Instant::now();
    let source = work.input.open()?;
    let open_ms = begin.elapsed().as_secs_f64() * 1000.0;
    let begin = Instant::now();
    let mut value = match &work.request {
        Request::Inspect(_) => super::source::inspect(&source)?,
        Request::Frames(request) => super::frames::decode(&source, request)?,
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
        key,
        page,
        cache_hit: false,
        timings: json!({"hash_before":hash_before,"open":open_ms,"decode_or_cache":decode_ms,"hash_after":begin.elapsed().as_secs_f64()*1000.0}),
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
        state.cache.insert(output.key.clone(), output.page.clone())?;
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
            Ok(Outcome { key: "33".into(), page: Page::new(json!({"status":"complete"})).unwrap(), cache_hit: false, timings: Value::Null }),
        );
        let page = queue.result(33).unwrap();
        assert_eq!(page["finished"], true);
        assert_eq!(page["error"]["code"], "RESOURCE_LIMIT");
        assert_eq!(leases.len(), 32);
        assert_eq!(queue.clear_cache()["removed"], 0);
    }
}
