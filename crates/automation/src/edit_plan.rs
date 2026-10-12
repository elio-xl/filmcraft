//! Transaction identity is fixed before any retry; optional short polling releases backend locks.
use crate::{AutomationError, FilmcraftMcp};
use rmcp::{model::CallToolResult, service::RequestContext};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn is_plan(id: &str) -> bool {
    matches!(id, "edit.plan.validate" | "edit.plan.apply")
}
pub(crate) fn needs_session(id: &str, p: &Value) -> bool {
    is_plan(id) && p.as_object().is_some_and(|p| !p.contains_key("session_id") && !p.contains_key("request_id"))
}
pub(crate) fn identity(id: &str, p: &mut Value, session: Option<Value>) -> Result<(), AutomationError> {
    if !is_plan(id) {
        return Ok(());
    }
    let p = p.as_object_mut().ok_or_else(|| AutomationError::BadRequest("plan parameters must be an object".into()))?;
    if let Some(session) = session {
        p.insert("session_id".into(), session);
    }
    if !p.get("session_id").is_some_and(Value::is_string) {
        return Err(AutomationError::BadRequest("explicit request_id requires its original session_id; query edit.plan.session for a new plan".into()));
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    p.entry("request_id").or_insert_with(|| {
        let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        json!(format!("mcp-plan-{}-{time}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
    });
    Ok(())
}
impl FilmcraftMcp {
    pub(crate) async fn run_edit_plan(
        &self,
        id: &str,
        mut params: Value,
        context: Option<&RequestContext<rmcp::RoleServer>>,
    ) -> Result<CallToolResult, AutomationError> {
        let fields = params.as_object_mut().ok_or_else(|| AutomationError::BadRequest("plan parameters must be an object".into()))?;
        let wait = match fields.remove("wait") {
            None => false,
            Some(Value::Bool(b)) => b,
            _ => return Err(AutomationError::BadRequest("wait must be boolean".into())),
        };
        let ack = self.run(id, params).await?;
        if !wait {
            return crate::media_analysis::to_tool_result(ack);
        }
        let job = ack["job"].as_u64().ok_or_else(|| AutomationError::Other("plan returned no job".into()))?;
        let session = ack["session_id"].clone();
        loop {
            if context.is_some_and(|ctx| ctx.ct.is_cancelled()) {
                let current = self.run("edit.plan.session", json!({})).await?;
                if current["session_id"] != session {
                    return Err(AutomationError::App("SESSION_EXPIRED: read the document before retrying".into()));
                }
                self.run("jobs.cancel", json!({"job":job})).await?;
                let page = self.run("edit.plan.result", json!({"job":job,"session_id":session})).await?;
                return crate::media_analysis::to_tool_result(page);
            }
            let page = self.run("edit.plan.result", json!({"job":job,"session_id":session})).await?;
            if page["finished"] == true {
                return crate::media_analysis::to_tool_result(page);
            }
            match context {
                Some(ctx) => tokio::select! {_=ctx.ct.cancelled()=>{},_=tokio::time::sleep(Duration::from_millis(100))=>{}},
                None => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
}
