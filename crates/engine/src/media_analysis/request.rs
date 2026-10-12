//! Strict request boundaries for source media analysis.

use filmcraft_project::ItemId;
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Map, Value};

use crate::{EngineError, Result};

pub const MAX_SIDE: u32 = 1920;
pub const MAX_FRAMES: usize = 16;
const FRAMES_COMMAND: &str = "media.analysis.frames";
const INSPECT_COMMAND: &str = "media.analysis.inspect";
const COMMON_FIELDS: &[&str] = &["item", "request_id", "expected_version"];
const FRAME_FIELDS: &[&str] = &["strategy", "start_ticks", "end_ticks", "times_ticks", "max_side", "max_frames"];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AnalysisLimits {
    pub max_side: u32,
    pub max_frames: usize,
}

impl Default for AnalysisLimits {
    fn default() -> Self {
        Self { max_side: 512, max_frames: 8 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Sampling {
    Uniform { interval: Option<(Tick, Tick)> },
    Timestamps(Vec<Tick>),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AnalysisRequest {
    pub item: ItemId,
    pub request_id: Option<String>,
    pub expected_version: Option<String>,
    pub limits: AnalysisLimits,
    pub sampling: Sampling,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct InspectRequest {
    pub item: ItemId,
    pub request_id: Option<String>,
    pub expected_version: Option<String>,
}

fn bad(cmd: &str, message: &str) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: message.into() }
}

pub(super) fn object<'a>(params: &'a Value, cmd: &str, extra: &[&str]) -> Result<&'a Map<String, Value>> {
    let fields = params.as_object().ok_or_else(|| bad(cmd, "expected an object"))?;
    if fields.keys().any(|key| !COMMON_FIELDS.contains(&key.as_str()) && !extra.contains(&key.as_str())) {
        return Err(bad(cmd, "unknown parameter"));
    }
    Ok(fields)
}

pub(super) fn identifier(fields: &Map<String, Value>, key: &str, max_chars: usize, cmd: &str) -> Result<Option<String>> {
    let Some(value) = fields.get(key) else { return Ok(None) };
    let text = value.as_str().ok_or_else(|| bad(cmd, "identifiers must be nonempty strings"))?;
    if text.is_empty() || text.len() > max_chars.saturating_mul(4) || text.chars().count() > max_chars || text.chars().any(char::is_control) {
        return Err(bad(cmd, "identifier is empty, too long, or contains control characters"));
    }
    Ok(Some(text.to_owned()))
}

pub(super) fn common(fields: &Map<String, Value>, cmd: &str) -> Result<InspectRequest> {
    let item = fields.get("item").and_then(Value::as_u64).filter(|id| *id != 0).ok_or_else(|| bad(cmd, "item must be a positive integer"))?;
    Ok(InspectRequest {
        item: ItemId(item),
        request_id: identifier(fields, "request_id", 128, cmd)?,
        expected_version: identifier(fields, "expected_version", 256, cmd)?,
    })
}

/// Inspect accepts only source identity fields; sampling and MCP wait options belong elsewhere.
pub fn parse_inspect(params: &Value) -> Result<InspectRequest> {
    common(object(params, INSPECT_COMMAND, &[])?, INSPECT_COMMAND)
}

pub(super) fn tick(value: &Value) -> Result<Tick> {
    let text = value.as_str().ok_or_else(|| bad(FRAMES_COMMAND, "ticks must be nonnegative decimal strings"))?;
    if text.is_empty() || text.len() > 19 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad(FRAMES_COMMAND, "ticks must be nonnegative decimal strings within i64"));
    }
    text.parse::<i64>().map(Tick).map_err(|_| bad(FRAMES_COMMAND, "ticks exceed i64"))
}

pub(super) fn bounded_integer(fields: &Map<String, Value>, key: &str, default: u64, max: u64) -> Result<u64> {
    match fields.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|n| *n > 0 && *n <= max)
            .ok_or_else(|| bad(FRAMES_COMMAND, "sampling limits must be positive integers within the documented maximum")),
    }
}

/// Validate and bound a request before opening any media or starting a job.
pub fn parse(params: &Value) -> Result<AnalysisRequest> {
    let fields = object(params, FRAMES_COMMAND, FRAME_FIELDS)?;
    let common = common(fields, FRAMES_COMMAND)?;
    let defaults = AnalysisLimits::default();
    let max_side = u32::try_from(bounded_integer(fields, "max_side", u64::from(defaults.max_side), u64::from(MAX_SIDE))?)
        .map_err(|_| bad(FRAMES_COMMAND, "invalid max_side"))?;
    let max_frames = usize::try_from(bounded_integer(fields, "max_frames", 8, 16)?).map_err(|_| bad(FRAMES_COMMAND, "invalid max_frames"))?;
    let strategy = match fields.get("strategy") {
        None => "uniform",
        Some(value) => value.as_str().ok_or_else(|| bad(FRAMES_COMMAND, "strategy must be uniform or timestamps"))?,
    };
    let sampling = match strategy {
        "uniform" => {
            if fields.contains_key("times_ticks") {
                return Err(bad(FRAMES_COMMAND, "times_ticks requires the timestamps strategy"));
            }
            let interval = match (fields.get("start_ticks"), fields.get("end_ticks")) {
                (None, None) => None,
                (Some(start), Some(end)) => {
                    let (start, end) = (tick(start)?, tick(end)?);
                    if start >= end {
                        return Err(bad(FRAMES_COMMAND, "start_ticks must be less than end_ticks"));
                    }
                    Some((start, end))
                }
                _ => return Err(bad(FRAMES_COMMAND, "start_ticks and end_ticks must be supplied together")),
            };
            Sampling::Uniform { interval }
        }
        "timestamps" => {
            if ["start_ticks", "end_ticks", "max_frames"].iter().any(|key| fields.contains_key(*key)) {
                return Err(bad(FRAMES_COMMAND, "timestamps cannot be combined with interval or max_frames"));
            }
            let values = fields.get("times_ticks").and_then(Value::as_array).ok_or_else(|| bad(FRAMES_COMMAND, "timestamps requires times_ticks"))?;
            if values.is_empty() || values.len() > MAX_FRAMES {
                return Err(bad(FRAMES_COMMAND, "times_ticks must contain 1 to 16 entries"));
            }
            Sampling::Timestamps(values.iter().map(tick).collect::<Result<_>>()?)
        }
        _ => return Err(bad(FRAMES_COMMAND, "strategy must be uniform or timestamps")),
    };
    Ok(AnalysisRequest {
        item: common.item,
        request_id: common.request_id,
        expected_version: common.expected_version,
        limits: AnalysisLimits { max_side, max_frames },
        sampling,
    })
}

/// Resolve a request to exact source coordinates in the half-open available interval.
/// Explicit requests retain order and duplicates until actual decoded PTS deduplication.
pub fn sample_times(request: &AnalysisRequest, available: TimeRange) -> Result<Vec<Tick>> {
    let error = || bad(FRAMES_COMMAND, "invalid or out-of-range source interval");
    let end = available.start.0.checked_add(available.duration.0).ok_or_else(error)?;
    if available.start.0 < 0 || available.duration.0 <= 0 {
        return Err(error());
    }
    if request.limits.max_side == 0 || request.limits.max_side > MAX_SIDE || request.limits.max_frames == 0 || request.limits.max_frames > MAX_FRAMES {
        return Err(bad(FRAMES_COMMAND, "invalid sampling limits"));
    }
    match &request.sampling {
        Sampling::Timestamps(times) => {
            if times.is_empty() || times.len() > MAX_FRAMES || times.iter().any(|t| t.0 < available.start.0 || t.0 >= end) {
                return Err(error());
            }
            Ok(times.clone())
        }
        Sampling::Uniform { interval } => {
            let (start, stop) = interval.unwrap_or((available.start, Tick(end)));
            if start < available.start || stop.0 > end || start >= stop {
                return Err(error());
            }
            let span = i128::from(stop.0) - i128::from(start.0);
            let count = i128::try_from(request.limits.max_frames).map_err(|_| error())?;
            let mut result = Vec::with_capacity(request.limits.max_frames);
            for i in 0..count {
                let offset = i.checked_mul(span).and_then(|n| n.checked_div(count)).ok_or_else(error)?;
                let t = i128::from(start.0).checked_add(offset).ok_or_else(error)?;
                let t = Tick(i64::try_from(t).map_err(|_| error())?);
                if result.last() != Some(&t) {
                    result.push(t);
                }
            }
            Ok(result)
        }
    }
}
