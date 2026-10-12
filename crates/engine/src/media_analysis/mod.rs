//! Read-only, bounded analysis of project source media.

pub mod cache;
pub mod frames;
pub mod jobs;
pub mod preview;
pub mod request;
pub mod shots;
pub mod source;
pub mod summaries;
pub mod transcripts;

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
    let input = request.item().map(|item| source::AnalysisInput::capture(session, item)).transpose()?;
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
            id: "media.analysis.summary.set",
            label: "Store Grounded Source Summary",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"request_id":str?,"source_version":str,"start_ticks":str,"end_ticks":str,"summary":str,"tags":[str],"evidence_frame_ids":[str],"model_label":str}"#,
            enabled: always,
            run: summaries::set,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.summary.search",
            label: "Search Grounded Source Summaries",
            menu: &[],
            shortcut: None,
            params: r#"{"request_id":str?,"query":str,"limit":1..100?}"#,
            enabled: always,
            run: summaries::search,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.transcript",
            label: "Read Source Transcript",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"request_id":str?,"expected_version":str?,"start_ticks":str?,"end_ticks":str?}"#,
            enabled: always,
            run: transcripts::start,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.audio",
            label: "Measure Source Audio",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"request_id":str?,"expected_version":str?,"start_ticks":str?,"end_ticks":str?,"threshold_db":number?,"min_duration_ticks":str?}"#,
            enabled: always,
            run: transcripts::start_audio,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.shots",
            label: "Find Source Shot Candidates",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"request_id":str?,"expected_version":str?,"start_ticks":str?,"end_ticks":str?,"sensitivity":0..100?,"min_shot_frames":1..10000?}"#,
            enabled: always,
            run: shots::start,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.preview",
            label: "Preview Source Clip",
            menu: &[],
            shortcut: None,
            params: r#"{"item":id,"request_id":str?,"expected_version":str?,"start_ticks":str?,"end_ticks":str?,"max_side":1..1920?,"max_fps":1..12?,"audio":bool?,"video_capability":"unknown|unsupported|verified"?}"#,
            enabled: always,
            run: preview::start,
            journal: false,
        },
        CommandSpec {
            id: "media.analysis.preview.read",
            label: "Read Preview Artifact",
            menu: &[],
            shortcut: None,
            params: r#"{"artifact":str}"#,
            enabled: always,
            run: preview::read,
            journal: false,
        },
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

#[cfg(test)]
mod preview_tests;

#[cfg(test)]
mod shots_tests;

#[cfg(test)]
mod transcripts_tests;

#[cfg(test)]
mod summaries_tests;
