//! A bounded cooperative plan worker. It cannot acquire the live Session.
use super::validation::{Candidate, Plan, Snapshot};
use crate::media_analysis::{
    cache,
    source::{AnalysisInput, check_cancelled, failure},
};
use crate::{EngineError, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

struct Work {
    id: u64,
    plan: Plan,
    snapshot: Snapshot,
    inputs: Vec<AnalysisInput>,
}
struct Job {
    request_id: String,
    cancel: Arc<AtomicBool>,
    finished: bool,
    status: String,
    result: Option<Value>,
}
#[derive(Default)]
struct State {
    worker: bool,
    active: Option<u64>,
    pending: VecDeque<Work>,
    jobs: BTreeMap<u64, Job>,
    receipts: BTreeMap<String, (String, u64)>,
    results: VecDeque<u64>,
}
#[derive(Default)]
pub struct PlanQueue {
    inner: Arc<Mutex<State>>,
}
fn lookup(state: &State, plan: &Plan) -> Result<Option<Value>> {
    let Some((hash, id)) = state.receipts.get(&plan.request_id) else {
        return Ok(None);
    };
    if *hash != cache::key(&plan.canonical)? {
        return Err(failure("REQUEST_ID_CONFLICT", "request ID already has different parameters", false));
    }
    let job = state.jobs.get(id).ok_or_else(|| failure("INTERNAL_ERROR", "missing plan receipt", false))?;
    if job.finished && job.result.is_none() {
        return Err(failure("RESULT_EXPIRED", "plan result expired; submit a fresh request ID", false));
    }
    Ok(Some(super::receipt(*id, &job.request_id)))
}
impl PlanQueue {
    pub(super) fn lookup(&self, plan: &Plan) -> Result<Option<Value>> {
        lookup(&self.inner.lock().unwrap_or_else(PoisonError::into_inner), plan)
    }
    pub(super) fn submit(&self, id: u64, plan: Plan, snapshot: Snapshot, inputs: Vec<AnalysisInput>) -> Result<Value> {
        if cfg!(target_arch = "wasm32") {
            return Err(failure("CAPABILITY_UNAVAILABLE", "editing plans require a native worker", false));
        }
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(reply) = lookup(&state, &plan)? {
            return Ok(reply);
        }
        if state.pending.len() + usize::from(state.active.is_some()) >= 9 {
            return Err(failure("PLAN_BUSY", "one plan is active and eight are queued", true));
        }
        if state.jobs.len() >= 10000 {
            return Err(failure("RESOURCE_LIMIT", "plan receipt capacity reached; start a new session", false));
        }
        if id == 0 || state.jobs.contains_key(&id) {
            return Err(failure("INTERNAL_ERROR", "invalid plan job ID", false));
        }
        let reply = super::receipt(id, &plan.request_id);
        state.receipts.insert(plan.request_id.clone(), (cache::key(&plan.canonical)?, id));
        state.jobs.insert(
            id,
            Job { request_id: plan.request_id.clone(), cancel: Arc::new(AtomicBool::new(false)), finished: false, status: "queued".into(), result: None },
        );
        state.pending.push_back(Work { id, plan, snapshot, inputs });
        if !state.worker {
            state.worker = true;
            let inner = self.inner.clone();
            if let Err(e) = std::thread::Builder::new().name("filmcraft-edit-plan".into()).spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&inner))).is_err() {
                    let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
                    let ids = state.jobs.iter().filter(|(_, j)| !j.finished).map(|(id, _)| *id).collect::<Vec<_>>();
                    for id in ids {
                        finish(&mut state, id, Err(failure("WORKER_PANIC", "plan worker failed; live project is unchanged", false)));
                    }
                    state.pending.clear();
                    state.active = None;
                    state.worker = false;
                }
            }) {
                state.worker = false;
                state.pending.clear();
                finish(&mut state, id, Err(failure("WORKER_UNAVAILABLE", e.to_string(), true)));
            }
        }
        Ok(reply)
    }
    pub fn result(&self, id: u64) -> Result<Value> {
        let state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let job = state.jobs.get(&id).ok_or_else(|| failure("JOB_NOT_FOUND", "editing plan job does not exist", false))?;
        let mut page = if job.finished {
            job.result.clone().ok_or_else(|| failure("RESULT_EXPIRED", "plan result expired; submit a fresh request", false))?
        } else {
            json!({"schema_version":1,"status":job.status})
        };
        page["job"] = json!(id);
        page["request_id"] = json!(job.request_id);
        page["finished"] = json!(job.finished);
        Ok(page)
    }
    pub fn list(&self) -> Vec<Value> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).jobs.iter().map(|(id,j)|json!({"id":id,"kind":"edit_plan","label":"Editing plan","request_id":j.request_id,"status":j.status,"finished":j.finished,"progress":if j.finished{1.0}else{0.0},"result_expired":j.finished && j.result.is_none()})).collect()
    }
    pub fn cancel(&self, id: u64) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(job) = state.jobs.get(&id) else {
            return false;
        };
        if job.finished {
            return true;
        }
        job.cancel.store(true, Ordering::Relaxed);
        if let Some(index) = state.pending.iter().position(|w| w.id == id) {
            state.pending.remove(index);
            finish(&mut state, id, Err(failure("CANCELLED", "plan cancelled before computation", false)));
        }
        true
    }
}
impl Drop for PlanQueue {
    fn drop(&mut self) {
        for job in self.inner.lock().unwrap_or_else(PoisonError::into_inner).jobs.values() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}
fn process(work: Work, cancel: &Arc<AtomicBool>) -> Result<Value> {
    let mut verified = BTreeMap::new();
    let mut opened = Vec::new();
    for (input, expected) in work.inputs.into_iter().zip(&work.plan.sources) {
        check_cancelled()?;
        let version = cache::input_version(&input, cancel)?;
        if version != expected.version {
            return Err(failure("VERSION_MISMATCH", format!("source item {} changed; inspect and rebuild the plan", expected.item.0), false));
        }
        let source = input.open()?;
        verified.insert(source.root_item, source.source_duration);
        opened.push((source, version));
    }
    let Candidate { project, state, report } = super::validation::compute(&work.snapshot, &work.plan, &verified)?;
    drop((project, state));
    for (source, version) in opened {
        if cache::source_version(&source, cancel)? != version {
            return Err(failure("SOURCE_CHANGED", "source changed during plan validation", true));
        }
    }
    check_cancelled()?;
    Ok(report)
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
            let Some(job) = state.jobs.get_mut(&work.id) else {
                continue;
            };
            job.status = "running".into();
            let cancel = job.cancel.clone();
            state.active = Some(work.id);
            (work, cancel)
        };
        let id = work.id;
        let result = filmcraft_media::cancel::with_cancel(&cancel, || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filmcraft_media::cancel::with_background(true, || process(work, &cancel))))
        })
        .unwrap_or_else(|_| Err(failure("WORKER_PANIC", "isolated editing failed; live project is unchanged", false)));
        let mut state = inner.lock().unwrap_or_else(PoisonError::into_inner);
        finish(&mut state, id, result);
        state.active = None;
    }
}
fn finish(state: &mut State, id: u64, result: Result<Value>) {
    let Some(job) = state.jobs.get_mut(&id) else {
        return;
    };
    let result = if job.cancel.load(Ordering::Relaxed) { Err(failure("CANCELLED", "plan cancelled; live project is unchanged", false)) } else { result };
    let result = result
        .and_then(|v| if cache::serialized_size(&v)? <= 1024 * 1024 { Ok(v) } else { Err(failure("RESOURCE_LIMIT", "plan report exceeds 1 MiB", false)) });
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            let (code, message) = match e {
                EngineError::Analysis(e) => (e.code, e.message),
                other => ("PLAN_INVALID".into(), other.to_string()),
            };
            json!({"schema_version":1,"status":if code=="CANCELLED"{"cancelled"}else{"error"},"committed":false,"error":{"code":code,"message":message}})
        }
    };
    job.status = value["status"].as_str().unwrap_or("error").into();
    job.finished = true;
    job.result = Some(value);
    state.results.push_back(id);
    while state.results.len() > 32 {
        if let Some(old) = state.results.pop_front()
            && let Some(job) = state.jobs.get_mut(&old)
        {
            job.result = None;
        }
    }
}
