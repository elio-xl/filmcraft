//! Still images (PNG, JPEG, GIF (first frame), WebP, TIFF, BMP, Targa, DPX) as media sources.

use std::sync::Arc;

use filmcraft_frame::VideoFrame;
use filmcraft_time::{FrameRate, TICKS_PER_SECOND, Tick};

use crate::{FrameRequest, MediaError, MediaInfo, MediaKind, MediaSource, Result, VideoStreamInfo};

/// Default still duration (Premiere's preference default is 5 seconds).
pub const DEFAULT_STILL_DURATION: Tick = Tick(5 * TICKS_PER_SECOND);

/// Whether `b` (the contents of the file `name`) is a still image we decode.
pub fn sniff(name: &str, b: &[u8]) -> bool {
    crate::dpx::sniff(b)
        || crate::tga::sniff(name, b)
        || b.starts_with(b"\x89PNG")
        || b.starts_with(&[0xFF, 0xD8, 0xFF])
        || b.starts_with(b"GIF8")
        || (b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP")
        || b.starts_with(b"II*\0")
        || b.starts_with(b"MM\0*")
        || b.starts_with(b"BM")
}

/// A decoded picture: straight-alpha RGBA8, top row first.
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Whether any pixel is not fully opaque.
    pub has_alpha: bool,
}

/// Decode a still image file (`name` picks Targa, which has no magic number; the content picks
/// every other format).
pub fn decode_raster(name: &str, bytes: &[u8]) -> Result<Raster> {
    if crate::dpx::sniff(bytes) {
        return crate::dpx::decode(name, bytes);
    }
    if crate::tga::sniff(name, bytes) {
        return crate::tga::decode(name, bytes);
    }
    use image::ImageDecoder;
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| MediaError::Decode(format!("{name}: {e}")))?;
    let mut decoder = reader.into_decoder().map_err(|e| MediaError::Decode(format!("{name}: {e}")))?;
    let orientation = decoder.orientation().map_err(|e| MediaError::Decode(format!("{name}: {e}")))?;
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| MediaError::Decode(format!("{name}: {e}")))?;
    img.apply_orientation(orientation);
    let has_alpha = img.color().has_alpha();
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(Raster { width, height, rgba: rgba.into_raw(), has_alpha })
}

pub struct StillSource {
    info: MediaInfo,
    frame: Arc<VideoFrame>,
    analysis_supported: bool,
}

impl StillSource {
    pub fn decode(name: &str, bytes: &[u8]) -> Result<Self> {
        let r = decode_raster(name, bytes)?;
        let mut source = Self::from_rgba(name, r.width, r.height, r.rgba, r.has_alpha, bytes.len() as u64);
        source.analysis_supported = bytes.starts_with(b"\x89PNG") || bytes.starts_with(&[0xff, 0xd8, 0xff]);
        Ok(source)
    }

    pub fn from_rgba(name: &str, w: u32, h: u32, rgba: Vec<u8>, has_alpha: bool, file_size: u64) -> Self {
        let codec = name.rsplit('.').next().unwrap_or("image").to_ascii_uppercase();
        let info = MediaInfo {
            name: name.to_string(),
            kind: MediaKind::Still,
            duration: DEFAULT_STILL_DURATION,
            video: Some(VideoStreamInfo {
                width: w,
                height: h,
                frame_rate: FrameRate::FPS_29_97,
                par: (1, 1),
                codec,
                pixel_format: if has_alpha { "RGBA 8-bit".into() } else { "RGB 8-bit".into() },
                color: filmcraft_color::ColorInfo::SRGB_FULL,
                has_alpha,
                bitrate: None,
                hdr: None,
            }),
            audio_streams: Vec::new(),
            container: "Image".into(),
            start_timecode: None,
            file_size: Some(file_size),
        };
        Self { info, frame: Arc::new(VideoFrame::rgba8(w, h, rgba)), analysis_supported: true }
    }
}

impl MediaSource for StillSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, _req: FrameRequest) -> Result<Arc<VideoFrame>> {
        Ok(self.frame.clone())
    }
    fn analysis_support(&self) -> Result<()> {
        if self.analysis_supported { Ok(()) } else { Err(MediaError::Unsupported("analysis currently supports PNG and JPEG stills".into())) }
    }
    fn analysis_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>> {
        self.analysis_support()?;
        if req.time.0 < 0 {
            return Err(MediaError::Decode("negative analysis time".into()));
        }
        Ok(self.frame.clone())
    }
    fn audio(&self, _start: i64, _frames: usize, _sample_rate: u32) -> Result<filmcraft_frame::AudioBuffer> {
        Err(MediaError::NoStream("audio"))
    }
}

#[cfg(test)]
mod analysis_tests {
    use super::*;
    use image::ImageEncoder;
    #[test]
    fn analysis_static_png_keeps_alpha_and_zero_pts() {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes).write_image(&[255, 0, 0, 127], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
        let s = StillSource::decode("image.png", &bytes).unwrap();
        let frame = s.analysis_frame(FrameRequest::full(Tick(20 * TICKS_PER_SECOND))).unwrap();
        assert_eq!(frame.pts, Tick::ZERO);
        assert_eq!(frame.to_rgba8(), [255, 0, 0, 127]);
    }
    #[test]
    fn analysis_jpeg_exif_orientation_is_applied() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg).write_image(&[255, 0, 0].repeat(32 * 16), 32, 16, image::ExtendedColorType::Rgb8).unwrap();
        let mut exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0".to_vec();
        let mut oriented = jpeg[..2].to_vec();
        oriented.extend([0xff, 0xe1]);
        oriented.extend(((exif.len() + 2) as u16).to_be_bytes());
        oriented.append(&mut exif);
        oriented.extend(&jpeg[2..]);
        let source = StillSource::decode("portrait.jpg", &oriented).unwrap();
        assert_eq!((source.info().video.as_ref().unwrap().width, source.info().video.as_ref().unwrap().height), (16, 32));
        let frame = source.analysis_frame(FrameRequest::full(Tick::ZERO)).unwrap();
        assert_eq!((frame.width, frame.height), (16, 32));
    }
}
