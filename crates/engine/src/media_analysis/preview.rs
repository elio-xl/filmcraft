//! Read-only short previews. Unknown video clients receive real PNG evidence instead.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_export::{ExportSettings, OutputSink, Progress};
use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::{FrameRequest, MediaError, MediaInfo, MediaSource, SharedSource};
use filmcraft_project::{ItemId, ItemKind, Label, MediaClip, MediaRef, Project, SequenceSettings, TrackKind};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND as TPS, Tick, TimeRange};
use serde_json::{Value, json};

use super::{
    request,
    source::{AnalysisSource, bounds, failure, media_failure},
};
use crate::{Result, Session};

pub const MAX_VIDEO_BYTES: u64 = 24 * 1024 * 1024;
#[derive(Clone, Debug, serde::Serialize)]
pub struct Request {
    pub item: ItemId,
    pub request_id: Option<String>,
    pub expected_version: Option<String>,
    interval: Option<(Tick, Tick)>,
    max_side: u32,
    max_fps: u32,
    audio: bool,
    video_capability: String,
}
fn parse(params: &Value) -> Result<Request> {
    const CMD: &str = "media.analysis.preview";
    let fields = request::object(params, CMD, &["start_ticks", "end_ticks", "max_side", "max_fps", "audio", "video_capability"])?;
    let common = request::common(fields, CMD)?;
    let interval = match (fields.get("start_ticks"), fields.get("end_ticks")) {
        (None, None) => None,
        (Some(a), Some(b)) => {
            let (a, b) = (request::tick(a)?, request::tick(b)?);
            if a >= b || b.0 - a.0 > 30 * TPS {
                return Err(failure("INVALID_RANGE", "preview interval must be positive and at most 30 seconds", false));
            }
            Some((a, b))
        }
        _ => return Err(failure("INVALID_PARAMS", "supply start_ticks and end_ticks together", false)),
    };
    let audio = match fields.get("audio") {
        None => true,
        Some(Value::Bool(value)) => *value,
        _ => return Err(failure("INVALID_PARAMS", "audio must be boolean", false)),
    };
    let video_capability = match fields.get("video_capability") {
        None => "unknown",
        Some(Value::String(s)) if matches!(s.as_str(), "unknown" | "unsupported" | "verified") => s,
        _ => return Err(failure("INVALID_PARAMS", "video_capability must be unknown, unsupported or verified", false)),
    }
    .to_owned();
    Ok(Request {
        item: common.item,
        request_id: common.request_id,
        expected_version: common.expected_version,
        interval,
        max_side: request::bounded_integer(fields, "max_side", 960, 1920)? as u32,
        max_fps: request::bounded_integer(fields, "max_fps", 12, 12)? as u32,
        audio,
        video_capability,
    })
}
pub fn start(session: &mut Session, params: &Value) -> Result<Value> {
    super::start(session, super::jobs::Request::Preview(parse(params)?))
}
pub fn read(session: &mut Session, params: &Value) -> Result<Value> {
    let fields = params.as_object().ok_or_else(|| failure("INVALID_PARAMS", "expected artifact object", false))?;
    let id = fields
        .get("artifact")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .ok_or_else(|| failure("INVALID_PARAMS", "invalid preview artifact id", false))?;
    if fields.len() != 1 {
        return Err(failure("INVALID_PARAMS", "read accepts only artifact", false));
    }
    session.analysis.read_artifact(id)
}
pub fn valid_id(id: &str) -> bool {
    id.strip_prefix("preview:").is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// Export rendering may substitute black/silence for unavailable sources. Retain any source
/// failure independently, so the analysis result can never certify such substituted evidence.
struct VerifiedSource {
    source: SharedSource,
    duration: Tick,
    cancel: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}
impl VerifiedSource {
    fn checked<T>(&self, work: impl FnOnce() -> filmcraft_media::Result<T>) -> filmcraft_media::Result<T> {
        let result =
            filmcraft_media::cancel::with_cancel(&self.cancel, || if self.cancel.load(Ordering::Relaxed) { Err(MediaError::Cancelled) } else { work() });
        if let Err(error) = &result {
            let mut first = self.error.lock().unwrap_or_else(PoisonError::into_inner);
            if first.is_none() {
                *first = Some(error.to_string().chars().take(1024).collect());
            }
        }
        result
    }
}
impl MediaSource for VerifiedSource {
    fn info(&self) -> &MediaInfo {
        self.source.info()
    }
    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        self.checked(|| {
            let frame = self.source.analysis_frame(req)?;
            super::frames::validate(&frame).map_err(|e| MediaError::Decode(e.to_string()))?;
            if frame.pts.0 < 0 || frame.pts >= self.duration {
                return Err(MediaError::Decode("preview frame timestamp is outside source duration".into()));
            }
            Ok(frame)
        })
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        self.checked(|| {
            if frames > 1024 * 1024 || rate == 0 {
                return Err(MediaError::Decode("audio request exceeds preview budget".into()));
            }
            let audio = self.source.audio(start, frames, rate)?;
            if audio.sample_rate != rate
                || audio.channels.is_empty()
                || audio.channels.len() > 64
                || audio.channels.iter().any(|c| c.len() != frames || c.iter().any(|v| !v.is_finite()))
            {
                return Err(MediaError::Decode("malformed preview audio buffer".into()));
            }
            Ok(audio)
        })
    }
}

pub(super) fn generate(source: &AnalysisSource, request: &Request, cancel: Arc<AtomicBool>) -> Result<(Value, Option<Arc<Vec<u8>>>)> {
    let (available_start, available_end) = bounds(source.available)?;
    let (start, end) = request.interval.unwrap_or((available_start, Tick(available_start.0.saturating_add(10 * TPS).min(available_end.0))));
    if start < available_start || end > available_end || start >= end {
        return Err(failure("INVALID_RANGE", "preview interval is outside the available source range", false));
    }
    if request.video_capability != "verified" {
        let frames = request::AnalysisRequest {
            item: request.item,
            request_id: request.request_id.clone(),
            expected_version: request.expected_version.clone(),
            limits: request::AnalysisLimits { max_side: request.max_side, max_frames: 8 },
            sampling: request::Sampling::Uniform { interval: Some((start, end)) },
        };
        let mut page = super::frames::decode(source, &frames)?;
        page["delivery"] = json!("frames");
        page["requested_range"] = json!({"start_ticks":start.0.to_string(),"end_ticks":end.0.to_string()});
        page["fallback_reason"] = json!("Client video consumption is unknown or unsupported; PNG evidence was generated for the requested interval.");
        page["fallback_call"] = json!({"name":"media_frames","arguments":{"item":source.item.0,"start_ticks":start.0.to_string(),"end_ticks":end.0.to_string(),"max_side":request.max_side,"max_frames":8}});
        return Ok((page, None));
    }
    source.source.analysis_support().map_err(media_failure)?;
    let info = source.source.info();
    let video = info.video.as_ref().ok_or_else(|| failure("NO_STREAM", "video preview requires a video stream", false))?;
    if video.width == 0
        || video.height == 0
        || u64::from(video.width) * u64::from(video.height) > 64 * 1024 * 1024
        || video.par.0 == 0
        || video.par.1 == 0
        || video.frame_rate.num <= 0
        || video.frame_rate.den <= 0
        || video.frame_rate.num > i64::from(u32::MAX)
        || video.frame_rate.den > i64::from(u32::MAX)
    {
        return Err(failure("INVALID_SOURCE", "invalid preview video geometry or frame rate", false));
    }
    let rate = if i128::from(video.frame_rate.num) > i128::from(request.max_fps) * i128::from(video.frame_rate.den) {
        FrameRate::new(i64::from(request.max_fps), 1)
    } else {
        video.frame_rate
    };
    let display_width = f64::from(video.width) * f64::from(video.par.0) / f64::from(video.par.1);
    let scale = (f64::from(request.max_side) / display_width.max(f64::from(video.height))).min(1.0);
    let width = (display_width * scale).floor() as u32 / 2 * 2;
    let height = (f64::from(video.height) * scale).floor() as u32 / 2 * 2;
    if width < 2 || height < 2 {
        return Err(failure("UNSUPPORTED_MEDIA", "H.264 preview needs at least 2 pixels on each side; use media_frames", false));
    }
    let audio = request.audio && info.has_audio();
    let range = TimeRange::new(start, Tick(end.0 - start.0));
    let mut project = Project::new("Source analysis preview");
    let item = project.add_item(
        &source.name,
        Label::Iris,
        ItemKind::Media(MediaClip {
            media: MediaRef::File { path: "analysis-source".into() },
            info: info.clone(),
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: Vec::new(),
            offline: false,
            proxy: None,
            identity: None,
        }),
        None,
    );
    let sequence = project.new_sequence(
        "Preview",
        SequenceSettings { width, height, par: (1, 1), frame_rate: rate, sample_rate: 48000, ..Default::default() },
        1,
        usize::from(audio),
        None,
    );
    for kind in [TrackKind::Video, TrackKind::Audio] {
        if kind == TrackKind::Audio && !audio {
            continue;
        }
        let mut clip = project
            .make_track_item(item, kind, Tick::ZERO, range, rate)
            .ok_or_else(|| failure("INTERNAL_ERROR", "could not create isolated preview clip", false))?;
        clip.duration = range.duration;
        clip.scale_to_frame = true;
        let seq = project.sequence_mut(sequence).ok_or_else(|| failure("INTERNAL_ERROR", "missing isolated sequence", false))?;
        let tracks = if kind == TrackKind::Video { &mut seq.video_tracks } else { &mut seq.audio_tracks };
        tracks.first_mut().ok_or_else(|| failure("INTERNAL_ERROR", "missing isolated track", false))?.items.push(clip);
    }
    let bytes = Arc::new(Mutex::new(None));
    let output = bytes.clone();
    let error = Arc::new(Mutex::new(None));
    let verified: SharedSource =
        Arc::new(VerifiedSource { source: source.source.clone(), duration: source.source_duration, cancel: cancel.clone(), error: error.clone() });
    let provider = move |id: ItemId| (id == item).then(|| verified.clone());
    let settings = ExportSettings {
        path: "analysis-preview.mp4".into(),
        frame_size: Some((width, height)),
        pixel_aspect: Some((1, 1)),
        frame_rate: Some(rate),
        range: Some(TimeRange::new(Tick::ZERO, range.duration)),
        include_audio: audio,
        bitrate_kbps: 1200,
        output_limit: Some(MAX_VIDEO_BYTES),
        presentation_duration: Some(range.duration),
        sink: Some(OutputSink(Arc::new(move |_, data| {
            *output.lock().unwrap_or_else(PoisonError::into_inner) = Some(data);
            Ok(())
        }))),
        ..Default::default()
    };
    let progress = Progress::default();
    let mut exporter =
        filmcraft_export::Exporter::new(Arc::new(project), sequence, &settings, &progress).map_err(|e| failure("ENCODE_FAILED", e.to_string(), false))?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(failure("CANCELLED", "preview was cancelled", false));
        }
        let step = exporter.step(&provider, &progress).map_err(|e| {
            let text = e.to_string();
            failure(if text.contains("output byte budget") { "RESOURCE_LIMIT" } else { "ENCODE_FAILED" }, text, false)
        })?;
        if let Some(message) = error.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            return Err(failure("DECODE_FAILED", message, false));
        }
        match step {
            filmcraft_export::Step::Done(_) => break,
            filmcraft_export::Step::Pending => return Err(failure("MEDIA_UNAVAILABLE", "source became unavailable during preview", true)),
            filmcraft_export::Step::Progress => {}
        }
    }
    let bytes = bytes.lock().unwrap_or_else(PoisonError::into_inner).take().ok_or_else(|| failure("ENCODE_FAILED", "encoder produced no preview", false))?;
    if bytes.len() as u64 > MAX_VIDEO_BYTES {
        return Err(failure("RESOURCE_LIMIT", "preview exceeded 24 MiB", false));
    }
    let page = json!({"schema_version":1,"status":"complete","delivery":"video","item":source.item.0,"root_item":source.root_item.0,
        "coordinate_space":"source","preview":{"source_start_ticks":start.0.to_string(),"source_end_ticks":end.0.to_string(),
        "duration_ticks":range.duration.0.to_string(),"frame_rate":{"num":rate.num,"den":rate.den},"audio":audio,
        "width":width,"height":height,"mime_type":"video/mp4","bytes":bytes.len(),"timebase":{"num":1,"den":rate.num.to_string()},"movie_timebase":{"num":1,"den":"1000000000"}}});
    Ok((page, Some(Arc::new(bytes))))
}
