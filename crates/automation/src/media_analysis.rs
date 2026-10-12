//! Source analysis transport: image blocks, bounded replies and short, cancellable polls.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use serde_json::{Value, json};

use crate::{AutomationError, FilmcraftMcp};

pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
// Leave room for the JSON-RPC envelope and request id in the stdio transport.
const ENVELOPE_RESERVE: usize = 4096;

pub(crate) fn ensure_request_id(command: &str, params: &mut Value) {
    if !matches!(
        command,
        "media.analysis.inspect"
            | "media.analysis.frames"
            | "media.analysis.preview"
            | "media.analysis.shots"
            | "media.analysis.transcript"
            | "media.analysis.audio"
            | "media.analysis.summary.set"
            | "media.analysis.summary.search"
    ) {
        return;
    }
    if let Some(fields) = params.as_object_mut() {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        fields.entry("request_id").or_insert_with(|| {
            let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
            json!(format!("mcp-{}-{time}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
        });
    }
}

fn malformed(message: &str) -> AutomationError {
    AutomationError::BadRequest(message.into())
}

struct Counter(usize);
impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| std::io::Error::other("response length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Keep PNGs only in image blocks. Metadata is repeated as JSON text for older MCP clients.
/// An oversized response drops whole frames, retaining every omitted source request time.
pub fn to_tool_result(mut page: Value) -> Result<CallToolResult, AutomationError> {
    if !page.is_object() {
        return Err(malformed("analysis result must be an object"));
    }
    let mut images = Vec::new();
    if let Some(frames) = page.get_mut("frames") {
        let frames = frames.as_array_mut().ok_or_else(|| malformed("analysis frames must be an array"))?;
        if frames.len() > 16 {
            return Err(malformed("too many analysis frames"));
        }
        for (index, frame) in frames.iter_mut().enumerate() {
            let fields = frame.as_object_mut().ok_or_else(|| malformed("invalid analysis frame"))?;
            let data = fields.remove("png_base64").and_then(|v| v.as_str().map(str::to_owned)).ok_or_else(|| malformed("analysis frame has no PNG"))?;
            if data.len() > MAX_RESPONSE_BYTES {
                return Err(malformed("analysis PNG exceeds response limit"));
            }
            if fields.get("mime_type").and_then(Value::as_str) != Some("image/png") {
                return Err(malformed("unsupported analysis image MIME type"));
            }
            fields.insert("content_index".into(), json!(index + 1));
            images.push(ContentBlock::image(data, "image/png"));
        }
    }
    loop {
        let empty_frames = page.get("frames").and_then(Value::as_array).is_some_and(Vec::is_empty);
        if empty_frames && page["finished"] == true {
            page["status"] = json!("error");
            page["error"] = json!({"code":"NO_FRAMES", "message":"No frames fit or could be decoded; reduce max_side or change the requested interval."});
        }
        let text = serde_json::to_string(&page).map_err(|e| AutomationError::Other(e.to_string()))?;
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.content.extend(images.iter().cloned());
        if let Some(preview) = page.get("preview") {
            let id = preview["artifact_id"]
                .as_str()
                .filter(|id| filmcraft_engine::media_analysis::preview::valid_id(id))
                .ok_or_else(|| malformed("invalid preview artifact"))?;
            let uri = format!("filmcraft://analysis/{id}");
            if preview["resource_uri"] != uri {
                return Err(malformed("invalid preview resource URI"));
            }
            let size = preview["bytes"].as_u64().filter(|size| *size <= 24 * 1024 * 1024).ok_or_else(|| malformed("preview exceeds resource budget"))?;
            result.content.push(ContentBlock::resource_link(rmcp::model::Resource::new(uri, "source-preview").with_mime_type("video/mp4").with_size(size)));
        }
        result.structured_content = Some(page.clone());
        result.is_error = Some(matches!(page["status"].as_str(), Some("error" | "cancelled")));
        let mut counter = Counter(0);
        serde_json::to_writer(&mut counter, &result).map_err(|e| AutomationError::Other(e.to_string()))?;
        if counter.0 <= MAX_RESPONSE_BYTES - ENVELOPE_RESERVE {
            return Ok(result);
        }
        if images.pop().is_none() {
            return Err(malformed("analysis metadata exceeds response limit"));
        }
        let dropped = page.get_mut("frames").and_then(Value::as_array_mut).and_then(Vec::pop).ok_or_else(|| malformed("missing analysis frame metadata"))?;
        let remaining = page.as_object_mut().ok_or_else(|| malformed("invalid analysis page"))?.entry("remaining_times_ticks").or_insert_with(|| json!([]));
        let remaining = remaining.as_array_mut().ok_or_else(|| malformed("invalid remaining source times"))?;
        if let Some(times) = dropped["requested_ticks"].as_array() {
            remaining.extend(times.iter().cloned());
        }
        page["status"] = json!("partial");
    }
}

impl FilmcraftMcp {
    pub(crate) async fn run_analysis(
        &self,
        command: &str,
        mut params: Value,
        context: Option<&RequestContext<rmcp::RoleServer>>,
    ) -> Result<CallToolResult, AutomationError> {
        let wait = match params.as_object_mut().ok_or_else(|| malformed("expected analysis arguments object"))?.remove("wait") {
            None => true,
            Some(Value::Bool(wait)) => wait,
            _ => return Err(malformed("wait must be a boolean")),
        };
        ensure_request_id(command, &mut params);
        let start = self.run(command, params).await?;
        if !wait {
            return to_tool_result(start);
        }
        let job = start["job"].as_u64().ok_or_else(|| malformed("analysis started no job"))?;
        // Each run releases the Session/Bridge mutex. The worker owns its own source snapshot.
        loop {
            if context.is_some_and(|ctx| ctx.ct.is_cancelled()) {
                self.run("jobs.cancel", json!({"job":job})).await?;
                return to_tool_result(
                    json!({"job":job,"status":"cancelled","finished":false,"error":{"code":"CANCELLED","message":"Cancellation requested; poll media_analysis_result for worker completion."}}),
                );
            }
            let page = self.run("media.analysis.result", json!({"job":job})).await?;
            if page["finished"] == true {
                return to_tool_result(page);
            }
            match context {
                Some(ctx) => tokio::select! {
                    _ = ctx.ct.cancelled() => {},
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                },
                None => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
}
