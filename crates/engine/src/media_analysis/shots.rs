//! Bounded source shot candidates. No timeline command or semantic scene classification.
use filmcraft_media::FrameRequest;
use filmcraft_project::ItemId;
use filmcraft_render::scene::{SceneDetector, SceneOptions};
use filmcraft_time::{TICKS_PER_SECOND as TPS, Tick};
use serde_json::{Value, json};

use super::{
    request,
    source::{AnalysisSource, bounds, check_cancelled, failure, media_failure},
};
use crate::{Result, Session};

const MAX_SAMPLES: usize = 72_000;
const MAX_SHOTS: usize = 1000;
const MAX_GAPS: usize = 1000;
#[derive(Clone, Debug, serde::Serialize)]
pub struct Request {
    pub item: ItemId,
    pub request_id: Option<String>,
    pub expected_version: Option<String>,
    interval: Option<(Tick, Tick)>,
    sensitivity: u32,
    min_shot_frames: usize,
}
pub fn start(session: &mut Session, params: &Value) -> Result<Value> {
    const CMD: &str = "media.analysis.shots";
    let fields = request::object(params, CMD, &["start_ticks", "end_ticks", "sensitivity", "min_shot_frames"])?;
    let common = request::common(fields, CMD)?;
    let interval = match (fields.get("start_ticks"), fields.get("end_ticks")) {
        (None, None) => None,
        (Some(a), Some(b)) => {
            let (a, b) = (request::tick(a)?, request::tick(b)?);
            if a >= b {
                return Err(failure("INVALID_RANGE", "shot interval must be positive", false));
            }
            Some((a, b))
        }
        _ => return Err(failure("INVALID_PARAMS", "supply start_ticks and end_ticks together", false)),
    };
    let sensitivity = match fields.get("sensitivity") {
        None => 50,
        Some(v) => v.as_u64().filter(|s| *s <= 100).ok_or_else(|| failure("INVALID_PARAMS", "sensitivity must be an integer from 0 to 100", false))? as u32,
    };
    let min_shot_frames = request::bounded_integer(fields, "min_shot_frames", 6, 10_000)? as usize;
    super::start(
        session,
        super::jobs::Request::Shots(Request {
            item: common.item,
            request_id: common.request_id,
            expected_version: common.expected_version,
            interval,
            sensitivity,
            min_shot_frames,
        }),
    )
}
fn span(start: Tick, end: Tick) -> Value {
    json!({"start_ticks":start.0.to_string(),"end_ticks":end.0.to_string()})
}
#[derive(Default)]
struct Segment {
    start: Option<Tick>,
    times: Vec<Tick>,
    detector: SceneDetector,
}
impl Segment {
    /// Return the first undelivered source time if the candidate budget is exhausted.
    fn finish(&mut self, end: Tick, options: &SceneOptions, shots: &mut Vec<(Tick, Tick)>) -> Option<Tick> {
        let start = self.start.take()?;
        let mut previous = start;
        let cuts = self.detector.cuts(options);
        let mut remaining = None;
        for boundary in cuts.into_iter().filter_map(|i| self.times.get(i).copied()).chain(std::iter::once(end)) {
            if boundary <= previous || boundary > end {
                continue;
            }
            if shots.len() >= MAX_SHOTS {
                remaining = Some(previous);
                break;
            }
            shots.push((previous, boundary));
            previous = boundary;
        }
        self.times.clear();
        self.detector = SceneDetector::new();
        remaining
    }
}
fn decoded(source: &AnalysisSource, time: Tick, scale: f32) -> Result<std::sync::Arc<filmcraft_frame::VideoFrame>> {
    check_cancelled()?;
    let frame = source.source.analysis_frame(FrameRequest { time, scale }).map_err(media_failure)?;
    super::frames::validate(&frame)?;
    if frame.pts.0 < 0 || frame.pts >= source.source_duration {
        return Err(failure("INVALID_FRAME", "shot sample timestamp lies outside source duration", false));
    }
    Ok(frame)
}
pub(super) fn analyse(source: &AnalysisSource, request: &Request) -> Result<Value> {
    let (available_start, available_end) = bounds(source.available)?;
    let (start, end) = request.interval.unwrap_or((available_start, available_end));
    if start < available_start || end > available_end || start >= end {
        return Err(failure("INVALID_RANGE", "shot interval is outside the source range", false));
    }
    source.source.analysis_support().map_err(media_failure)?;
    let video = source.source.info().video.as_ref().ok_or_else(|| failure("NO_STREAM", "shot detection requires video", false))?;
    let rate = video.frame_rate;
    if rate.num <= 0 || rate.den <= 0 || i128::from(rate.num) > i128::from(TPS) * i128::from(rate.den) {
        return Err(failure("INVALID_SOURCE", "source frame rate cannot be represented in ticks", false));
    }
    let scale = (256.0 / video.width.max(video.height).max(1) as f32).min(1.0);
    let stop = Tick(start.0.saturating_add(600 * TPS).min(end.0));
    let options = SceneOptions { sensitivity: request.sensitivity as f32, min_shot_frames: request.min_shot_frames };
    let mut segment = Segment::default();
    let mut shots = Vec::new();
    let mut missing: Vec<Value> = Vec::new();
    let mut errors = Vec::new();
    let mut time = start;
    let mut samples = 0;
    let mut remaining = None;
    while time < stop && samples < MAX_SAMPLES {
        check_cancelled()?;
        let next = Tick(
            rate.tick_of(rate.frame_at(time).checked_add(1).ok_or_else(|| failure("INVALID_SOURCE", "source frame index overflow", false))?).0.min(stop.0),
        );
        if next <= time {
            return Err(failure("INVALID_SOURCE", "source frame clock did not advance", false));
        }
        let frame = decoded(source, time, scale).and_then(|frame| {
            if segment.times.last().is_some_and(|last| *last > frame.pts) {
                return Err(failure("INVALID_FRAME", "source presentation timestamps moved backwards", false));
            }
            Ok(frame)
        });
        match frame {
            Ok(frame) => {
                if segment.times.last() != Some(&frame.pts) {
                    segment.start.get_or_insert(time);
                    segment.detector.push_rgba(&frame.to_rgba8(), frame.width as usize, frame.height as usize);
                    segment.times.push(frame.pts);
                }
            }
            Err(error) => {
                check_cancelled()?;
                if let Some(uncovered) = segment.finish(time, &options, &mut shots) {
                    remaining = Some(uncovered);
                    break;
                }
                let boundary = time.0.to_string();
                if let Some(last) = missing.last_mut().filter(|last| last["end_ticks"].as_str() == Some(boundary.as_str())) {
                    last["end_ticks"] = json!(next.0.to_string());
                } else {
                    if missing.len() >= MAX_GAPS {
                        remaining = Some(time);
                        break;
                    }
                    missing.push(span(time, next));
                    errors.push(json!({"range":span(time,next),"code":"DECODE_FAILED","message":error.to_string().chars().take(1024).collect::<String>()}));
                }
            }
        }
        time = next;
        samples += 1;
    }
    if remaining.is_none() {
        remaining = segment.finish(time, &options, &mut shots);
    }
    let remaining_start = remaining.unwrap_or(time);
    let remaining_ranges = if remaining_start < end { vec![span(remaining_start, end)] } else { Vec::new() };
    let mut values = Vec::with_capacity(shots.len());
    let mut covered: Vec<Value> = Vec::new();
    for (a, b) in shots {
        check_cancelled()?;
        let boundary = a.0.to_string();
        if let Some(last) = covered.last_mut().filter(|last| last["end_ticks"].as_str() == Some(boundary.as_str())) {
            last["end_ticks"] = json!(b.0.to_string());
        } else {
            covered.push(span(a, b));
        }
        let middle = Tick(a.0 + (b.0 - a.0) / 2);
        let mut value = span(a, b);
        value["representative_requested_ticks"] = json!(middle.0.to_string());
        match decoded(source, middle, scale) {
            Ok(frame) => {
                value["representative_ticks"] = json!(frame.pts.0.to_string());
            }
            Err(error) => {
                check_cancelled()?;
                value["representative_ticks"] = Value::Null;
                errors.push(json!({"requested_ticks":middle.0.to_string(),"code":"REPRESENTATIVE_UNAVAILABLE","message":error.to_string().chars().take(1024).collect::<String>()}));
            }
        }
        values.push(value);
    }
    let status = if errors.is_empty() && remaining_ranges.is_empty() {
        "complete"
    } else if values.is_empty() {
        "error"
    } else {
        "partial"
    };
    Ok(json!({"schema_version":1,"status":status,"item":source.item.0,"root_item":source.root_item.0,"coordinate_space":"source",
        "candidate_only":true,"origin":"deterministic","algorithm":"filmcraft-scene-difference-v1","sampling":"source_nominal_frame_grid",
        "parameters":{"sensitivity":request.sensitivity,"min_shot_frames":request.min_shot_frames},"requested_range":span(start,end),
        "covered_ranges":covered,"missing_ranges":missing,"remaining_ranges":remaining_ranges,"samples":samples,
        "shots":values,"errors":errors}))
}
