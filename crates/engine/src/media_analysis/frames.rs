//! Bounded image pages with exact source presentation timestamps.

use std::collections::BTreeMap;
use std::io::Write;

use base64::Engine;
use filmcraft_frame::{PixelData, VideoFrame};
use filmcraft_media::FrameRequest;
use image::ImageEncoder;
use serde_json::{Value, json};

use super::{
    request::{AnalysisRequest, sample_times},
    source::{AnalysisSource, check_cancelled, failure, media_failure},
};
use crate::{EngineError, Result};

const MAX_PIXELS: usize = 64 * 1024 * 1024;
const MAX_PNG: usize = 6 * 1024 * 1024;

fn invalid(message: &str) -> EngineError {
    failure("INVALID_FRAME", message, false)
}

fn validate(frame: &VideoFrame) -> Result<()> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let pixels = w.checked_mul(h).filter(|&n| n > 0 && n <= MAX_PIXELS).ok_or_else(|| invalid("decoded dimensions exceed the image budget"))?;
    let rgba = pixels.checked_mul(4).ok_or_else(|| invalid("pixel count overflow"))?;
    let planes_valid = |lengths: [usize; 3], chroma: filmcraft_frame::Chroma, alpha: Option<usize>| {
        let (sx, sy) = chroma.shifts();
        let c = w.div_ceil(1 << sx) * h.div_ceil(1 << sy);
        lengths == [pixels, c, c] && alpha.is_none_or(|n| n == pixels)
    };
    let valid = match &frame.data {
        PixelData::Rgba8(data) => data.len() == rgba,
        PixelData::RgbaF32(data) => data.len() == rgba && data.iter().all(|v| v.is_finite()),
        PixelData::Yuv8 { planes, chroma, alpha } => planes_valid(planes.each_ref().map(|p| p.len()), *chroma, alpha.as_ref().map(|p| p.len())),
        PixelData::Yuv16 { planes, chroma, bits, alpha } => {
            (9..=16).contains(bits) && planes_valid(planes.each_ref().map(|p| p.len()), *chroma, alpha.as_ref().map(|p| p.len()))
        }
    };
    if !valid {
        return Err(invalid("decoded pixel planes are malformed"));
    }
    Ok(())
}

struct PngBuffer(Vec<u8>);
impl Write for PngBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().checked_add(bytes.len()).is_none_or(|len| len > MAX_PNG) {
            return Err(std::io::Error::other("PNG exceeds the image response budget"));
        }
        self.0.try_reserve(bytes.len()).map_err(std::io::Error::other)?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode(frame: &VideoFrame, par: (u32, u32), max_side: u32) -> Result<Value> {
    validate(frame)?;
    if par.0 == 0 || par.1 == 0 {
        return Err(invalid("pixel aspect ratio must be positive"));
    }
    let display_w = f64::from(frame.width) * f64::from(par.0) / f64::from(par.1);
    let display_h = f64::from(frame.height);
    let scale = (f64::from(max_side) / display_w.max(display_h)).min(1.0);
    let width = (display_w * scale).round().max(1.0).min(f64::from(max_side)) as u32;
    let height = (display_h * scale).round().max(1.0).min(f64::from(max_side)) as u32;
    check_cancelled()?;
    let rgba = image::RgbaImage::from_raw(frame.width, frame.height, frame.to_rgba8()).ok_or_else(|| invalid("invalid RGBA buffer"))?;
    let scaled = if (width, height) == rgba.dimensions() { rgba } else { image::imageops::resize(&rgba, width, height, image::imageops::FilterType::Triangle) };
    check_cancelled()?;
    let mut png = PngBuffer(Vec::new());
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(scaled.as_raw(), width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| failure("RESOURCE_LIMIT", format!("could not encode bounded PNG; try a smaller max_side: {e}"), false))?;
    Ok(json!({"actual_ticks":frame.pts.0.to_string(),"width":width,"height":height,
        "mime_type":"image/png","png_base64":base64::engine::general_purpose::STANDARD.encode(&png.0),
        "request_indices":[],"requested_ticks":[]}))
}

/// Return successful frames even when individual samples cannot be decoded.
pub fn decode(source: &AnalysisSource, request: &AnalysisRequest) -> Result<Value> {
    let times = sample_times(request, source.available)?;
    let mut frames = BTreeMap::<filmcraft_time::Tick, Value>::new();
    let mut errors = Vec::new();
    let mut remaining = Vec::new();
    let mut image_bytes = 0usize;
    let par = source.source.info().video.as_ref().map_or((1, 1), |v| v.par);
    for (index, time) in times.iter().enumerate() {
        check_cancelled()?;
        let result = (|| {
            let frame = source.source.analysis_frame(FrameRequest::full(*time)).map_err(media_failure)?;
            if frame.pts.0 < 0 || frame.pts >= source.source_duration {
                return Err(invalid("decoded presentation timestamp lies outside source duration"));
            }
            let value = match frames.entry(frame.pts) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let value = encode(&frame, par, request.limits.max_side)?;
                    let bytes = super::cache::serialized_size(&value)?;
                    if bytes > super::cache::FRAME_PAGE_BYTES {
                        return Err(failure("RESOURCE_LIMIT", "a single frame exceeds the response budget; reduce max_side", false));
                    }
                    if image_bytes.saturating_add(bytes) > super::cache::FRAME_PAGE_BYTES {
                        remaining.push(time.0.to_string());
                        return Ok(());
                    }
                    image_bytes = image_bytes.saturating_add(bytes);
                    entry.insert(value)
                }
            };
            value["request_indices"].as_array_mut().ok_or_else(|| invalid("missing frame indices"))?.push(json!(index));
            value["requested_ticks"].as_array_mut().ok_or_else(|| invalid("missing requested timestamps"))?.push(json!(time.0.to_string()));
            Ok(())
        })();
        if let Err(error) = result {
            let failure = match error {
                EngineError::Analysis(f) => f,
                other => super::AnalysisFailure { code: "INTERNAL_ERROR".into(), message: other.to_string(), retryable: false },
            };
            if failure.code == "CANCELLED" {
                return Err(failure.into());
            }
            errors.push(
                json!({"request_index":index,"requested_ticks":time.0.to_string(),"code":failure.code,"message":failure.message,"retryable":failure.retryable}),
            );
        }
    }
    let status = if errors.is_empty() && remaining.is_empty() {
        "complete"
    } else if frames.is_empty() {
        "error"
    } else {
        "partial"
    };
    Ok(json!({"schema_version":1,"status":status,"item":source.item.0,"root_item":source.root_item.0,
        "coordinate_space":"source","strategy":match request.sampling {super::request::Sampling::Uniform {..}=>"uniform",super::request::Sampling::Timestamps(_)=>"timestamps"},
        "source_range":{"start_ticks":source.available.start.0.to_string(),"end_ticks":super::source::bounds(source.available)?.1.0.to_string()},
        "frames":frames.into_values().collect::<Vec<_>>(),"errors":errors,"remaining_times_ticks":remaining}))
}
