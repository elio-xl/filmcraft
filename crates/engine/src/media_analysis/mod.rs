//! Read-only, bounded analysis of project source media.

pub mod cache;
pub mod frames;
pub mod jobs;
pub mod request;
pub mod source;

/// Machine-readable failure shared by analysis jobs and per-frame results.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct AnalysisFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[cfg(test)]
mod tests;

/// Start read-only source analysis without media I/O on the caller's thread.
fn start(session: &mut crate::Session, request: jobs::Request) -> crate::Result<serde_json::Value> {
    if let Some(reply) = session.analysis.lookup(&request)? {
        return Ok(reply);
    }
    let input = source::AnalysisInput::capture(session, request.item())?;
    let id = session.allocate_job_id()?;
    session.analysis.submit(id, input, request)
}
pub fn inspect(session: &mut crate::Session, params: &serde_json::Value) -> crate::Result<serde_json::Value> {
    start(session, jobs::Request::Inspect(request::parse_inspect(params)?))
}
pub fn frames(session: &mut crate::Session, params: &serde_json::Value) -> crate::Result<serde_json::Value> {
    start(session, jobs::Request::Frames(request::parse(params)?))
}
pub fn clear_cache(session: &mut crate::Session, params: &serde_json::Value) -> crate::Result<serde_json::Value> {
    if params.as_object().is_none_or(|fields| !fields.is_empty()) {
        return Err(source::failure("INVALID_PARAMS", "clearCache accepts an empty object", false));
    }
    Ok(session.analysis.clear_cache())
}
pub fn result(session: &crate::Session, params: &serde_json::Value) -> crate::Result<serde_json::Value> {
    let fields = params.as_object().ok_or_else(|| source::failure("INVALID_PARAMS", "expected a job object", false))?;
    if fields.len() != 1 {
        return Err(source::failure("INVALID_PARAMS", "result accepts only job", false));
    }
    let id = fields
        .get("job")
        .and_then(serde_json::Value::as_u64)
        .filter(|&id| id > 0)
        .ok_or_else(|| source::failure("INVALID_PARAMS", "job must be a positive integer", false))?;
    session.analysis.result(id)
}
pub fn commands() -> Vec<crate::CommandSpec> {
    use crate::commands::{CommandSpec, always};
    vec![
        CommandSpec {
            id: "media.analysis.clearCache",
            label: "Clear Unused Analysis Cache",
            menu: &[],
            shortcut: None,
            params: "{}",
            enabled: always,
            run: clear_cache,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.inspect",
            label: "Inspect Source Media",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"request_id":str?,"expected_version":str?}"#,
            enabled: always,
            run: inspect,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.frames",
            label: "Sample Source Frames",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"request_id":str?,"expected_version":str?,"strategy":"uniform|timestamps"?,"start_ticks":str?,"end_ticks":str?,"times_ticks":[str]?,"max_frames":1..16?,"max_side":1..1920?}"#,
            enabled: always,
            run: frames,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.result",
            label: "Get Media Analysis Result",
            menu: &[],
            shortcut: None,
            params: r#"{"job":id}"#,
            enabled: always,
            run: |s, p| result(s, p),
            journal: false,
        },
    ]
}
