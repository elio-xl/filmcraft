//! Stored source transcripts and deterministic, chunked audio measurements.
use super::{
    request,
    source::{AnalysisSource, bounds, check_cancelled, failure, media_failure},
};
use crate::{Result, Session};
use filmcraft_project::{ItemId, Transcript};
use filmcraft_time::{TICKS_PER_SECOND as TPS, Tick};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(serde::Serialize)]
pub struct Request {
    pub item: ItemId,
    pub request_id: Option<String>,
    pub expected_version: Option<String>,
    interval: Option<(Tick, Tick)>,
    threshold_db: f64,
    min_duration: Tick,
    #[serde(skip)]
    transcript: Option<Arc<Transcript>>,
    #[serde(skip)]
    generate_unavailable: Option<String>,
}
fn parse(params: &Value, audio: bool) -> Result<Request> {
    let command = if audio { "media.analysis.audio" } else { "media.analysis.transcript" };
    let extras: &[&str] = if audio { &["start_ticks", "end_ticks", "threshold_db", "min_duration_ticks"] } else { &["start_ticks", "end_ticks"] };
    let fields = request::object(params, command, extras)?;
    let common = request::common(fields, command)?;
    let interval = match (fields.get("start_ticks"), fields.get("end_ticks")) {
        (None, None) => None,
        (Some(a), Some(b)) => {
            let (a, b) = (request::tick(a)?, request::tick(b)?);
            if a >= b {
                return Err(failure("INVALID_RANGE", "source interval must be positive", false));
            }
            Some((a, b))
        }
        _ => return Err(failure("INVALID_PARAMS", "supply start_ticks and end_ticks together", false)),
    };
    let threshold_db = match fields.get("threshold_db") {
        None => -40.0,
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite() && (-120.0..=0.0).contains(v))
            .ok_or_else(|| failure("INVALID_PARAMS", "threshold_db must be finite and between -120 and 0", false))?,
    };
    let min_duration = fields.get("min_duration_ticks").map(request::tick).transpose()?.unwrap_or(Tick(TPS / 2));
    if min_duration.0 <= 0 || min_duration.0 > 600 * TPS {
        return Err(failure("INVALID_PARAMS", "minimum silence duration must be positive and at most ten minutes", false));
    }
    Ok(Request {
        item: common.item,
        request_id: common.request_id,
        expected_version: common.expected_version,
        interval,
        threshold_db,
        min_duration,
        transcript: None,
        generate_unavailable: None,
    })
}
pub fn start(s: &mut Session, p: &Value) -> Result<Value> {
    let mut request = parse(p, false)?;
    // Arc snapshot only: no speech inference, serialization or filesystem I/O on the caller.
    let root = s.project.resolve_media(request.item).map(|(root, _, _)| root).ok_or_else(|| failure("INVALID_SOURCE", "no such source item", false))?;
    request.transcript = s.project.transcripts.get(&root).cloned();
    request.generate_unavailable = crate::transcript::can_transcribe(s).err();
    super::start(s, super::jobs::Request::Transcript(request))
}
pub fn start_audio(s: &mut Session, p: &Value) -> Result<Value> {
    super::start(s, super::jobs::Request::Audio(parse(p, true)?))
}
fn span(a: Tick, b: Tick) -> Value {
    json!({"start_ticks":a.0.to_string(),"end_ticks":b.0.to_string()})
}
fn range(source: &AnalysisSource, r: &Request) -> Result<(Tick, Tick)> {
    let (a, b) = bounds(source.available)?;
    let (start, end) = r.interval.unwrap_or((a, b));
    if start < a || end > b || start >= end {
        return Err(failure("INVALID_RANGE", "interval is outside available source range", false));
    }
    Ok((start, end))
}
pub(super) fn transcript(source: &AnalysisSource, r: &Request) -> Result<Value> {
    let (start, end) = range(source, r)?;
    let mut page = json!({"schema_version":1,"status":"complete","item":source.item.0,"root_item":source.root_item.0,"coordinate_space":"source","requested_range":span(start,end),"origin":"stored_transcript"});
    let Some(t) = &r.transcript else {
        page["availability"] = json!("unavailable");
        page["reason"] = json!("No stored transcript; import words or explicitly run transcription with an installed model.");
        page["actions"] = json!({"import":{"command":"transcript.set","params":{"item":source.root_item.0,"transcript":{"language":"","source":"imported","words":[]}}},"generate":{"command":"transcript.generate","params":{"items":[source.root_item.0]},"build_available":r.generate_unavailable.is_none(),"unavailable_reason":r.generate_unavailable,"requires_installed_model":true}});
        return Ok(page);
    };
    if t.words.len() > 100_000 || t.speakers.len() > 1024 || t.language.len() > 256 || t.source.len() > 4096 || t.speakers.iter().any(|s| s.name.len() > 1024) {
        return Err(failure("RESOURCE_LIMIT", "stored transcript metadata exceeds query budget", false));
    }
    let mut words = Vec::new();
    let mut bytes = 0usize;
    let mut last = Tick::ZERO;
    let mut remaining = None;
    for w in &t.words {
        check_cancelled()?;
        if w.start < last
            || w.end < w.start
            || w.end > source.source_duration
            || !w.confidence.is_finite()
            || !(0.0..=1.0).contains(&w.confidence)
            || w.speaker.is_some_and(|i| i as usize >= t.speakers.len())
        {
            return Err(failure("INVALID_TRANSCRIPT", "stored words have invalid source bounds, ordering, confidence or speaker", false));
        }
        last = w.end;
        if w.end <= start || w.start >= end {
            continue;
        }
        if w.text.len() > 16384 {
            return Err(failure("RESOURCE_LIMIT", "stored word exceeds text budget", false));
        }
        bytes = bytes.saturating_add(w.text.len());
        if words.len() >= 10000 || bytes > 1024 * 1024 {
            remaining = Some(w.start.max(start));
            break;
        }
        words.push(json!({"text":w.text,"start_ticks":w.start.0.to_string(),"end_ticks":w.end.0.to_string(),"speaker":w.speaker,"confidence":w.confidence,"overlap":span(w.start.max(start),w.end.min(end))}));
    }
    page["availability"] = json!("available");
    page["language"] = json!(t.language);
    page["transcript_source"] = json!(t.source);
    page["speakers"] = json!(t.speakers);
    page["words"] = json!(words);
    page["remaining_ranges"] = json!(remaining.map(|a| vec![span(a, end)]).unwrap_or_default());
    if remaining.is_some() {
        page["status"] = json!("partial");
    }
    Ok(page)
}
fn sample(t: Tick, rate: u32, ceil: bool) -> Result<i64> {
    let n = i128::from(t.0) * i128::from(rate);
    let n = if ceil { (n + i128::from(TPS) - 1) / i128::from(TPS) } else { n / i128::from(TPS) };
    i64::try_from(n).map_err(|_| failure("INVALID_RANGE", "source sample position exceeds i64", false))
}
fn silence(a: i64, b: i64, rate: u32) -> Value {
    let mut value = span(Tick::from_units(a, i64::from(rate)), Tick::from_units(b, i64::from(rate)));
    value["start_sample"] = json!(a.to_string());
    value["end_sample"] = json!(b.to_string());
    value
}
pub(super) fn audio(source: &AnalysisSource, r: &Request) -> Result<Value> {
    let (start, end) = range(source, r)?;
    let stream = source.source.info().audio().ok_or_else(|| failure("NO_STREAM", "source has no audio", false))?;
    let rate = stream.sample_rate;
    let channels = stream.channels as usize;
    if !(8000..=192000).contains(&rate) || !(1..=8).contains(&channels) {
        return Err(failure("RESOURCE_LIMIT", "audio analysis supports 8–192 kHz and one to eight channels", false));
    }
    let stop = Tick(start.0.saturating_add(600 * TPS).min(end.0));
    let first = sample(start, rate, true)?;
    let until = sample(stop, rate, false)?;
    if first >= until {
        return Err(failure("INVALID_RANGE", "range contains no complete audio sample", false));
    }
    let minimum = sample(r.min_duration, rate, true)?;
    let threshold = 10f64.powf(r.threshold_db / 20.0);
    let mut meter = filmcraft_audio_dsp::LoudnessMeter::new(f64::from(rate), channels);
    let mut position = first;
    let mut quiet = None;
    let mut ranges = Vec::new();
    while position < until {
        check_cancelled()?;
        let count = (until - position).min(4096) as usize;
        let buffer = source.source.audio(position, count, rate).map_err(media_failure)?;
        if buffer.sample_rate != rate
            || buffer.channels.len() != channels
            || buffer.channels.iter().any(|c| c.len() != count || c.iter().any(|v| !v.is_finite() || v.abs() > 16.0))
        {
            return Err(failure("INVALID_AUDIO", "decoder returned mismatched or nonfinite audio samples", false));
        }
        meter.process(&buffer.channels.iter().map(Vec::as_slice).collect::<Vec<_>>());
        for i in 0..count {
            // Every channel length was checked before entering this hot loop.
            let silent = buffer.channels.iter().all(|c| f64::from(c[i].abs()) <= threshold);
            let at = position + i as i64;
            if silent {
                quiet.get_or_insert(at);
            } else if let Some(a) = quiet.take()
                && at - a >= minimum
            {
                if ranges.len() >= 10000 {
                    return Err(failure("RESOURCE_LIMIT", "too many silence ranges; query a shorter interval", false));
                }
                ranges.push(silence(a, at, rate));
            }
        }
        position += count as i64;
    }
    if let Some(a) = quiet
        && until - a >= minimum
    {
        if ranges.len() >= 10000 {
            return Err(failure("RESOURCE_LIMIT", "too many silence ranges; query a shorter interval", false));
        }
        ranges.push(silence(a, until, rate));
    }
    let lufs = meter.integrated();
    Ok(
        json!({"schema_version":1,"status":if stop<end{"partial"}else{"complete"},"item":source.item.0,"root_item":source.root_item.0,"coordinate_space":"source","origin":"deterministic",
        "requested_range":span(start,end),"analysed_range":silence(first,until,rate),"sample_rate":rate,"stream":0,
        "parameters":{"threshold_db":r.threshold_db,"min_duration_ticks":r.min_duration.0.to_string(),"silence_metric":"all_channels_absolute_sample_amplitude"},
        "silence_ranges":ranges,"remaining_ranges":if stop<end {vec![span(stop,end)]}else{vec![]},
        "loudness":{"availability":"available","algorithm":"ITU-R BS.1770-4 / EBU R128","integrated_lufs":if lufs.is_finite(){Some(lufs)}else{None},"below_gate":!lufs.is_finite()},
        "rhythm":{"availability":"unavailable","reason":"Beat and tempo extraction is unavailable through this analysis endpoint; sample peaks are not beat estimates."}}),
    )
}
