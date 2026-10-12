//! Original source media opened without project edits or display fallback images.

use std::sync::Arc;

use filmcraft_media::{Generator, MediaError, MediaKind, Opener, SharedReader, SharedSource};
use filmcraft_project::{ItemId, MediaRef};
use filmcraft_time::{FrameRate, Tick, TimeRange};
use serde_json::{Value, json};

use super::AnalysisFailure;
use crate::{EngineError, Result, Services, Session};

pub const MAX_WHOLE_BYTES: u64 = 64 * 1024 * 1024;
const READ_CHUNK: usize = 1024 * 1024;

/// Inputs affecting generated content, or the original file to verify for each analysis request.
#[derive(Clone, Debug, serde::Serialize)]
pub enum SourceIdentity {
    File { path: String },
    Sequence { first: String, rate: FrameRate, frames: Vec<Option<String>> },
    Generator { generator: Generator, width: u32, height: u32, rate: FrameRate, duration: Tick },
}

pub struct AnalysisSource {
    pub item: ItemId,
    pub root_item: ItemId,
    pub name: String,
    pub source: SharedSource,
    pub source_duration: Tick,
    pub duration_origin: &'static str,
    pub available: TimeRange,
    pub services: Arc<dyn Services>,
    pub identity: SourceIdentity,
}

/// Cheap, immutable job input. Capture holds no file handles and performs no media I/O.
pub struct AnalysisInput {
    item: ItemId,
    root_item: ItemId,
    name: String,
    interval: Option<TimeRange>,
    still_duration: Option<Tick>,
    pub(super) identity: SourceIdentity,
    pub(super) services: Arc<dyn Services>,
    openers: Vec<Opener>,
}

pub(crate) fn failure(code: &str, message: impl AsRef<str>, retryable: bool) -> EngineError {
    AnalysisFailure { code: code.into(), message: message.as_ref().chars().take(1024).collect(), retryable }.into()
}

pub(crate) fn check_cancelled() -> Result<()> {
    if filmcraft_media::cancel::cancelled() { Err(failure("CANCELLED", "analysis was cancelled", false)) } else { Ok(()) }
}

pub(crate) fn io_failure(error: std::io::Error) -> EngineError {
    let (code, retryable) = match error.kind() {
        std::io::ErrorKind::NotFound => ("MEDIA_OFFLINE", false),
        std::io::ErrorKind::Unsupported => ("CAPABILITY_UNAVAILABLE", false),
        std::io::ErrorKind::StorageFull => ("DISK_FULL", false),
        _ => ("IO_ERROR", true),
    };
    failure(code, error.to_string(), retryable)
}

pub(crate) fn media_failure(error: MediaError) -> EngineError {
    let (code, retryable) = match &error {
        MediaError::Unsupported(_) => ("UNSUPPORTED_MEDIA", false),
        MediaError::Offline(_) => ("MEDIA_OFFLINE", false),
        MediaError::Decode(_) => ("DECODE_FAILED", false),
        MediaError::NoStream(_) => ("NO_STREAM", false),
        MediaError::Cancelled => ("CANCELLED", false),
        MediaError::Io(_) => ("IO_ERROR", true),
    };
    failure(code, error.to_string(), retryable)
}

pub(crate) fn bounds(range: TimeRange) -> Result<(Tick, Tick)> {
    let end = range.start.0.checked_add(range.duration.0);
    if range.start.0 < 0 || range.duration.0 <= 0 || end.is_none() {
        return Err(failure("INVALID_RANGE", "source range must be positive, nonnegative and representable in ticks", false));
    }
    Ok((range.start, Tick(end.ok_or_else(|| failure("INVALID_RANGE", "source range overflows ticks", false))?)))
}

impl AnalysisInput {
    pub fn capture(session: &Session, item: ItemId) -> Result<Self> {
        let selected = session.project.item(item).ok_or_else(|| failure("SOURCE_NOT_FOUND", "project item does not exist", false))?;
        let (root_item, media, interval) = session
            .project
            .resolve_media(item)
            .ok_or_else(|| failure("SOURCE_NOT_FOUND", "item is not media, or its parent chain is missing, cyclic or deeper than 16 items", false))?;
        if media.offline {
            return Err(failure("MEDIA_OFFLINE", "source was made offline; reconnect it before analysis", false));
        }
        if selected.name.len() > 4096 {
            return Err(failure("RESOURCE_LIMIT", "source display name exceeds the analysis budget", false));
        }
        if let Some(span) = interval {
            bounds(span)?;
        }
        let identity = match &media.media {
            MediaRef::File { path } => {
                if path.is_empty() || path.len() > 16384 {
                    return Err(failure("RESOURCE_LIMIT", "source path is empty or too long", false));
                }
                if media.info.kind == MediaKind::ImageSequence {
                    let rate = media.info.frame_rate();
                    if rate.num <= 0 || rate.den <= 0 {
                        return Err(failure("INVALID_SOURCE", "invalid sequence frame rate", false));
                    }
                    SourceIdentity::Sequence { first: path.clone(), rate, frames: Vec::new() }
                } else {
                    SourceIdentity::File { path: path.clone() }
                }
            }
            MediaRef::Generator(generator) => {
                // Audio-only generators have no video dimensions; these internal placeholders
                // are never exposed as a video stream or used to allocate pixels.
                let (width, height, rate) = media.info.video.as_ref().map_or((1, 1, FrameRate::FPS_25), |v| (v.width, v.height, v.frame_rate));
                if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 64 * 1024 * 1024 || rate.num <= 0 || rate.den <= 0 {
                    return Err(failure("INVALID_SOURCE", "generator dimensions or frame rate are invalid or exceed the analysis budget", false));
                }
                let finite = match generator {
                    Generator::ColorMatte { color } => color.iter().all(|v| v.is_finite()),
                    Generator::Tone { hz, db } => hz.is_finite() && db.is_finite(),
                    _ => true,
                };
                if !finite {
                    return Err(failure("INVALID_SOURCE", "generator parameters must be finite", false));
                }
                bounds(TimeRange::new(Tick::ZERO, media.info.duration))?;
                SourceIdentity::Generator { generator: generator.clone(), width, height, rate, duration: media.info.duration }
            }
        };
        Ok(Self {
            item,
            root_item,
            name: selected.name.clone(),
            interval,
            still_duration: (media.info.kind == MediaKind::Still).then_some(media.info.duration),
            identity,
            services: session.services.clone(),
            openers: session.media.openers.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone(),
        })
    }

    /// Interpretation context independent of mutable file metadata. Used only as an internal
    /// session-cache key after a complete content hash has verified the source bytes.
    pub(super) fn cache_context(&self) -> Value {
        json!({"item":self.item,"root_item":self.root_item,"name":self.name,"interval":self.interval,
            "still_duration":self.still_duration,"identity":self.identity,
            "openers":self.openers.iter().map(|opener|*opener as usize).collect::<Vec<_>>()})
    }

    /// Run on the analysis worker, after releasing the live Session lock.
    pub fn open(mut self) -> Result<AnalysisSource> {
        check_cancelled()?;
        let source: SharedSource = match &mut self.identity {
            SourceIdentity::File { path } => open_file(&*self.services, path, &self.openers)?,
            SourceIdentity::Sequence { first, rate, frames } => {
                *frames = sequence_inventory(&*self.services, first)?;
                let services = self.services.clone();
                let loader = Arc::new(move |path: &str| read_bytes(&*services, path).map_err(std::io::Error::other));
                Arc::new(filmcraft_media::sequence::ImageSequenceSource::new(&self.name, frames.clone(), *rate, loader).map_err(media_failure)?)
            }
            SourceIdentity::Generator { generator, width, height, rate, duration } => {
                Arc::new(filmcraft_media::generators::GeneratorSource::new(generator.clone(), *width, *height, *rate, *duration).with_name(&self.name))
            }
        };
        // Stills have no intrinsic playback duration. Use the project's configured duration
        // instead of the decoder's five-second import default, including for late subclips.
        let project_duration = (source.info().kind == MediaKind::Still).then_some(self.still_duration).flatten();
        let source_duration = project_duration.unwrap_or(source.info().duration);
        let duration_origin = if project_duration.is_some() { "project_still_duration" } else { "source" };
        let whole = TimeRange::new(Tick::ZERO, source_duration);
        let (_, whole_end) = bounds(whole)?;
        let available = self.interval.unwrap_or(whole);
        let (_, end) = bounds(available)?;
        if end > whole_end {
            return Err(failure("INVALID_RANGE", "subclip extends beyond the original media duration", false));
        }
        Ok(AnalysisSource {
            item: self.item,
            root_item: self.root_item,
            name: self.name,
            source,
            source_duration,
            duration_origin,
            available,
            services: self.services,
            identity: self.identity,
        })
    }
}

/// Inventory is capped before allocating a sparse sequence; hosts must opt into bounded listing.
pub(crate) fn sequence_inventory(services: &dyn Services, path: &str) -> Result<Vec<Option<String>>> {
    use filmcraft_media::sequence::{Numbered, sequence_frames};
    const CAP: usize = 100_000;
    let first = Numbered::parse(path).ok_or_else(|| failure("UNSUPPORTED_MEDIA", "source is not a numbered image sequence", false))?;
    let dir = first.dir.trim_end_matches(['/', '\\']);
    // Preserve a filesystem root rather than turning / into the current directory.
    let dir = if dir.is_empty() && first.dir.starts_with('/') { "/" } else { dir };
    let siblings = services.list_dir_bounded(dir, CAP).map_err(io_failure)?;
    if siblings.len() > CAP || siblings.iter().any(|n| n.len() > 1024 || n.contains(['/', '\\'])) {
        return Err(failure("RESOURCE_LIMIT", "directory inventory exceeds the analysis budget", false));
    }
    let last = siblings.iter().filter_map(|n| first.number_of(n)).max().unwrap_or(first.number);
    if last.saturating_sub(first.number) >= CAP as u64 {
        return Err(failure("RESOURCE_LIMIT", "sequence span exceeds 100000 frames", false));
    }
    check_cancelled()?;
    Ok(sequence_frames(&first, &siblings))
}

fn read_reader(reader: &SharedReader, cap: u64) -> Result<Vec<u8>> {
    check_cancelled()?;
    let length = reader.len();
    if length > cap {
        return Err(failure("RESOURCE_LIMIT", "this format requires a whole-file read exceeding 64 MiB; use a supported streaming container", false));
    }
    let len = usize::try_from(length).map_err(|_| failure("RESOURCE_LIMIT", "source cannot fit in the host address space", false))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| failure("RESOURCE_LIMIT", "not enough memory for bounded source read", true))?;
    bytes.resize(len, 0);
    let mut offset = 0u64;
    for chunk in bytes.chunks_mut(READ_CHUNK) {
        check_cancelled()?;
        reader.read_at(offset, chunk).map_err(io_failure)?;
        offset = offset
            .checked_add(u64::try_from(chunk.len()).map_err(|_| failure("RESOURCE_LIMIT", "read length overflows", false))?)
            .ok_or_else(|| failure("RESOURCE_LIMIT", "read offset overflows", false))?;
    }
    Ok(bytes)
}

/// No call to the unbounded Services::read_file or its file_size fallback is allowed here.
pub(crate) fn read_bytes(services: &dyn Services, path: &str) -> Result<Vec<u8>> {
    check_cancelled()?;
    if let Some(reader) = services.reader(path) {
        return read_reader(&reader.map_err(io_failure)?, MAX_WHOLE_BYTES);
    }
    let bytes = services.read_file_bounded(path, MAX_WHOLE_BYTES).map_err(io_failure)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_WHOLE_BYTES) {
        return Err(failure("RESOURCE_LIMIT", "host returned more than the 64 MiB read budget", false));
    }
    check_cancelled()?;
    Ok(bytes)
}

fn open_file(services: &dyn Services, path: &str, openers: &[Opener]) -> Result<SharedSource> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if let Some(reader) = services.reader(path) {
        let reader = reader.map_err(io_failure)?;
        let head = filmcraft_media::reader::read_range(&*reader, 0, filmcraft_media::reader::HEAD_LEN).map_err(io_failure)?;
        for opener in filmcraft_codecs::reader_openers() {
            if let Some(source) = opener(name, &head, &reader) {
                return source.map_err(media_failure);
            }
        }
        if filmcraft_media::wav::sniff(&head) {
            return Ok(Arc::new(filmcraft_media::wav::WavSource::open(name, reader).map_err(media_failure)?));
        }
        return filmcraft_media::open_bytes(name, read_reader(&reader, MAX_WHOLE_BYTES)?.into(), openers).map_err(media_failure);
    }
    filmcraft_media::open_bytes(name, read_bytes(services, path)?.into(), openers).map_err(media_failure)
}

/// Synchronous convenience for tests and callers already on an analysis worker.
pub fn resolve(session: &Session, item: ItemId) -> Result<AnalysisSource> {
    AnalysisInput::capture(session, item)?.open()
}

/// Read actual source metadata. Missing orientation/encoded-size information stays explicit.
pub fn inspect(source: &AnalysisSource) -> Result<Value> {
    let info = source.source.info();
    let (start, end) = bounds(source.available)?;
    if info.audio_streams.len() > filmcraft_media::MAX_AUDIO_STREAMS {
        return Err(failure("RESOURCE_LIMIT", "too many audio streams", false));
    }
    let frame_support = source.source.analysis_support();
    let preview_support = frame_support.is_ok() && info.video.is_some();
    let video = info.video.as_ref().map(|v| {
        let rate = (info.kind != MediaKind::Still && v.frame_rate.num > 0 && v.frame_rate.den > 0).then_some(v.frame_rate);
        json!({"width":v.width,"height":v.height,"frame_rate":rate,"pixel_aspect":{"num":v.par.0,"den":v.par.1},
            "codec":v.codec,"pixel_format":v.pixel_format,"has_alpha":v.has_alpha,"color":v.color})
    });
    Ok(json!({
        "schema_version":1,"status":"complete","item":source.item.0,"root_item":source.root_item.0,
        "name":source.name,"kind":info.kind,"container":info.container,"availability":"online",
        "coordinate_space":"source","duration_ticks":source.source_duration.0.to_string(),"duration_origin":source.duration_origin,
        "source_range":{"start_ticks":start.0.to_string(),"end_ticks":end.0.to_string()},
        "video":video,"audio_streams":info.audio_streams,"file_size":info.file_size,
        "analysis_capabilities":{
            "inspect":{"supported":true},
            "frames":{"supported":frame_support.is_ok(),"reason":frame_support.err().map(|error|error.to_string())},
            "preview_video":{"supported":preview_support,"default_delivery":"frames","model_consumption":"requires_client_verification",
                "max_duration_ticks":(30 * filmcraft_time::TICKS_PER_SECOND).to_string(),"max_bytes":super::preview::MAX_VIDEO_BYTES}
        },
        "rotation_degrees":null,"encoded_size":null,"display_size":null,
        "unavailable_reasons":{
            "rotation_degrees":"the source metadata interface does not expose container or EXIF orientation",
            "encoded_size":"decoded dimensions cannot establish stored dimensions before crop or rotation",
            "display_size":"final display geometry is reported with decoded analysis frames"
        }
    }))
}
