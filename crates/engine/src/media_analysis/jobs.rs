//! One cooperative worker per session; the worker never owns or locks a Session.

use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use super::{
    AnalysisFailure,
    request::{AnalysisRequest, InspectRequest},
    source::{AnalysisInput, failure},
};
use crate::{EngineError, Result};

const MAX_RESULTS: usize = 32;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_JOBS: usize = 10_000;

pub enum Request {
    Inspect(InspectRequest),
    Frames(AnalysisRequest),
}
impl Request {
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Inspect(r) => r.request_id.as_deref(),
            Self::Frames(r) => r.request_id.as_deref(),
        }
    }
}
struct Work {
    id: u64,
    input: AnalysisInput,
    request: Request,
}
struct Job {
    id: u64,
    request_id: String,
    status: String,
    cancel: Arc<AtomicBool>,
    finished: bool,
    failure: Option<AnalysisFailure>,
}
struct Page {
    value: Value,
    bytes: usize,
}
#[derive(Default)]
struct State {
    worker: bool,
    active: Option<u64>,
    pending: VecDeque<Work>,
    jobs: BTreeMap<u64, Job>,
    pages: BTreeMap<u64, Arc<Page>>,
    order: VecDeque<u64>,
    bytes: usize,
}
#[derive(Default)]
pub struct AnalysisQueue {
    inner: Arc<Mutex<State>>,
}

impl AnalysisQueue {
    pub fn submit(&self, id: u64, input: AnalysisInput, request: Request) -> Result<Value> {
        if cfg!(target_arch = "wasm32") {
            return Err(failure("CAPABILITY_UNAVAILABLE", "media analysis requires a native worker host", false));
        }
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if state.pending.len() + usize::from(state.active.is_some()) >= 9 {
            return Err(failure("ANALYSIS_BUSY", "one analysis is active and eight requests are queued", true));
        }
        if state.jobs.len() >= MAX_JOBS {
            return Err(failure("RESOURCE_LIMIT", "session analysis job capacity reached; open a new session", false));
        }
        if state.jobs.contains_key(&id) {
            return Err(failure("INTERNAL_ERROR", "duplicate analysis job id", false));
        }
        let request_id = request.request_id().map(str::to_owned).unwrap_or_else(|| format!("analysis-{id}"));
        let reply = json!({"job":id,"request_id":request_id});
        state.jobs.insert(id, Job { id, request_id, status: "queued".into(), cancel: Arc::new(AtomicBool::new(false)), finished: false, failure: None });
        state.pending.push_back(Work { id, input, request });
        if !state.worker {
            state.worker = true;
            let inner = self.inner.clone();
            if let Err(e) = std::thread::Builder::new().name("filmcraft-media-analysis".into()).spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&inner)));
                if result.is_err() {
                    let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
                    let ids: Vec<_> = state.jobs.values().filter(|j| !j.finished).map(|j| j.id).collect();
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
                finish(&mut state, id, Err(failure("WORKER_UNAVAILABLE", e.to_string(), true)));
            }
        }
        Ok(reply)
    }

    pub fn result(&self, id: u64) -> Result<Value> {
        let page = {
            let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let job = state.jobs.get(&id).ok_or_else(|| failure("JOB_NOT_FOUND", "analysis job does not exist", false))?;
            if !job.finished {
                return Ok(json!({"schema_version":1,"job":id,"request_id":job.request_id,"status":job.status,"finished":false}));
            }
            if let Some(error) = &job.failure {
                return Ok(json!({"schema_version":1,"job":id,"request_id":job.request_id,"status":job.status,"finished":true,"error":error}));
            }
            state.pages.get(&id).cloned().ok_or_else(|| failure("RESULT_EXPIRED", "analysis result has expired; submit an explicit new request", false))?
        };
        Ok(page.value.clone())
    }

    pub fn list(&self) -> Vec<Value> {
        let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .jobs
            .values()
            .map(|job| {
                json!({"id":job.id,"request_id":job.request_id,"label":"Media analysis","kind":"media_analysis",
            "status":job.status,"finished":job.finished,"progress":if job.finished {1.0}else{0.0},"done":u8::from(job.finished),"total":1,
            "etaSeconds":null,"result":null,"result_expired":job.finished && job.failure.is_none() && !state.pages.contains_key(&job.id)})
            })
            .collect()
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
}
impl Drop for AnalysisQueue {
    fn drop(&mut self) {
        let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        for job in state.jobs.values().filter(|j| !j.finished) {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
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
        // Catch inside with_cancel so its thread-local restoration runs even after a decoder panic.
        let output = filmcraft_media::cancel::with_cancel(&cancel, || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                filmcraft_media::cancel::with_background(true, || {
                    super::source::check_cancelled()?;
                    let source = work.input.open()?;
                    match work.request {
                        Request::Inspect(_) => super::source::inspect(&source),
                        Request::Frames(request) => super::frames::decode(&source, &request),
                    }
                })
            }))
        })
        .unwrap_or_else(|_| Err(failure("WORKER_PANIC", "source analysis panicked; the session remains usable", false)));
        let output = if cancel.load(Ordering::Relaxed) { Err(failure("CANCELLED", "analysis was cancelled", false)) } else { output };
        let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
        finish(&mut state, id, output);
        state.active = None;
    }
}

struct Size(usize);
impl std::io::Write for Size {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| std::io::Error::other("result length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn error_page(error: EngineError) -> Value {
    let error = match error {
        EngineError::Analysis(error) => error,
        other => AnalysisFailure { code: "INTERNAL_ERROR".into(), message: other.to_string().chars().take(1024).collect(), retryable: false },
    };
    json!({"schema_version":1,"status":if error.code=="CANCELLED" {"cancelled"} else {"error"},"error":error})
}
fn finish(state: &mut State, id: u64, output: Result<Value>) {
    let Some(job) = state.jobs.get_mut(&id) else { return };
    let mut value = output.unwrap_or_else(error_page);
    value["job"] = json!(id);
    value["request_id"] = json!(job.request_id);
    value["finished"] = json!(true);
    let mut size = Size(0);
    if serde_json::to_writer(&mut size, &value).is_err() || size.0 > MAX_BYTES {
        value = error_page(failure("RESOURCE_LIMIT", "analysis result exceeds the session cache budget", false));
        value["job"] = json!(id);
        value["request_id"] = json!(job.request_id);
        value["finished"] = json!(true);
        size = Size(0);
        if serde_json::to_writer(&mut size, &value).is_err() {
            size.0 = MAX_BYTES;
        }
    }
    job.finished = true;
    job.status = value["status"].as_str().unwrap_or("error").to_owned();
    // Pinned pages are held only while a consumer copies them, never by job metadata.
    let attempts = state.order.len();
    for _ in 0..attempts {
        if state.pages.len() < MAX_RESULTS && state.bytes.saturating_add(size.0) <= MAX_BYTES {
            break;
        }
        let Some(victim) = state.order.pop_front() else { break };
        if state.pages.get(&victim).is_some_and(|page| Arc::strong_count(page) > 1) {
            state.order.push_back(victim);
            continue;
        }
        if let Some(page) = state.pages.remove(&victim) {
            state.bytes = state.bytes.saturating_sub(page.bytes);
        }
    }
    if state.pages.len() >= MAX_RESULTS || state.bytes.saturating_add(size.0) > MAX_BYTES {
        // Terminal metadata remains available even if every cached page is being consumed.
        job.status = "error".into();
        job.failure = Some(AnalysisFailure {
            code: "RESOURCE_LIMIT".into(),
            message: "all analysis cache slots are in use; retry after consumers finish".into(),
            retryable: true,
        });
        return;
    }
    state.bytes = state.bytes.saturating_add(size.0);
    state.pages.insert(id, Arc::new(Page { value, bytes: size.0 }));
    state.order.push_back(id);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_capacity_failure_is_terminal_and_preserves_leased_results() {
        let queue = AnalysisQueue::default();
        let leases = {
            let mut state = queue.inner.lock().unwrap();
            for id in 1..=33 {
                state.jobs.insert(
                    id,
                    Job {
                        id,
                        request_id: format!("r{id}"),
                        status: "running".into(),
                        cancel: Arc::new(AtomicBool::new(false)),
                        finished: false,
                        failure: None,
                    },
                );
            }
            for id in 1..=32 {
                finish(&mut state, id, Ok(json!({"status":"complete"})));
            }
            state.pages.values().cloned().collect::<Vec<_>>()
        };
        finish(&mut queue.inner.lock().unwrap(), 33, Ok(json!({"status":"complete"})));
        let page = queue.result(33).unwrap();
        assert_eq!(page["finished"], true);
        assert_eq!(page["error"]["code"], "RESOURCE_LIMIT");
        assert_eq!(leases.len(), 32);
        assert!(queue.result(1).is_ok());
    }
}
