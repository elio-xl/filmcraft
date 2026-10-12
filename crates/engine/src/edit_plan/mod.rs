//! Opt-in editing plans. Hashing and pure editing execute outside the live Session.
mod queue;
#[cfg(test)]
mod tests;
mod transaction;
pub mod validation;
use crate::media_analysis::source::{AnalysisInput, failure};
use crate::{Result, Session};
pub use queue::PlanQueue;
use serde_json::{Value, json};

pub fn validate(s: &mut Session, p: &Value) -> Result<Value> {
    start(s, p, false)
}
pub fn apply(s: &mut Session, p: &Value) -> Result<Value> {
    start(s, p, true)
}
fn start(s: &mut Session, p: &Value, apply: bool) -> Result<Value> {
    let mut plan = validation::parse(p)?;
    plan.apply = apply;
    plan.canonical["operation"] = json!(if apply { "apply" } else { "validate" });
    if apply && s.trim_play.active() {
        return Err(failure("CONTEXT_CHANGED", "finish interactive trim before applying a plan", false));
    }
    if let Some(reply) = s.edit_plans.lookup(&plan)? {
        return Ok(reply);
    }
    if plan.expected_revision != s.revision {
        return Err(failure("REVISION_MISMATCH", "read the current document before submitting this plan", false));
    }
    let inputs = plan.sources.iter().map(|r| AnalysisInput::capture(s, r.item)).collect::<Result<Vec<_>>>()?;
    let snapshot = validation::Snapshot { project: s.project.clone(), state: s.state.clone(), revision: s.revision };
    let id = s.allocate_job_id()?;
    s.edit_plans.submit(id, plan, snapshot, inputs)
}
pub fn result(s: &mut Session, p: &Value) -> Result<Value> {
    let f = p.as_object().filter(|f| f.len() == 1).ok_or_else(|| failure("INVALID_PARAMS", "result accepts only job", false))?;
    let job = f.get("job").and_then(Value::as_u64).filter(|n| *n > 0).ok_or_else(|| failure("INVALID_PARAMS", "job must be positive", false))?;
    poll(s);
    s.edit_plans.result(job)
}
pub fn poll(s: &mut Session) {
    queue::poll(s);
}
pub fn commands() -> Vec<crate::CommandSpec> {
    use crate::commands::{CommandSpec, always};
    vec![
        CommandSpec {
            id: "edit.plan.apply",
            label: "Apply Editing Plan",
            menu: &[],
            shortcut: None,
            params: r#"{"request_id":str,"expected_revision":str,"source_versions":[{"item":id,"source_version":str}],"steps":[{"id":str,"params":object,"reason":str}]}"#,
            enabled: always,
            run: apply,
            journal: false,
        },
        CommandSpec {
            id: "edit.plan.validate",
            label: "Validate Editing Plan",
            menu: &[],
            shortcut: None,
            params: r#"{"request_id":str,"expected_revision":str,"source_versions":[{"item":id,"source_version":str}],"steps":[{"id":str,"params":object,"reason":str}]}"#,
            enabled: always,
            run: validate,
            journal: false,
        },
        CommandSpec {
            id: "edit.plan.result",
            label: "Read Editing Plan Result",
            menu: &[],
            shortcut: None,
            params: r#"{"job":id}"#,
            enabled: always,
            run: |s, p| result(s, p),
            journal: false,
        },
    ]
}
fn receipt(id: u64, request_id: &str) -> Value {
    json!({"job":id,"request_id":request_id})
}
