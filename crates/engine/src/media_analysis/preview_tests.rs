use crate::{Services, Session};
use base64::Engine;
use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::{FrameRequest, MediaInfo, MediaSource, SharedSource};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND as TPS, Tick};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

struct Host;
impl Services for Host {
    fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
        panic!("unbounded read")
    }
    fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
        panic!("analysis must not write through project services")
    }
    fn reader(&self, _: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        Some(Ok(Arc::new(filmcraft_media::reader::MemReader(Arc::from(&b"AP01"[..])))))
    }
}
struct MarkedSource {
    info: MediaInfo,
    broken: bool,
}
impl MediaSource for MarkedSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        self.analysis_frame(req)
    }
    fn analysis_support(&self) -> filmcraft_media::Result<()> {
        Ok(())
    }
    fn analysis_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        if self.broken {
            return Err(filmcraft_media::MediaError::Decode("synthetic bad frame".into()));
        }
        let rate = self.info.frame_rate();
        let pts = rate.tick_of(rate.frame_at(req.time));
        let rgb = if pts.0 < TPS * 5 / 2 { [220, 20, 20, 255] } else { [20, 220, 20, 255] };
        Ok(Arc::new(VideoFrame::rgba8(64, 32, rgb.repeat(64 * 32)).with_pts(pts)))
    }
    fn audio(&self, start: i64, frames: usize, sr: u32) -> filmcraft_media::Result<AudioBuffer> {
        let mut result = AudioBuffer::silence(sr, 2, frames);
        for channel in &mut result.channels {
            for (i, value) in channel.iter_mut().enumerate() {
                let time = (start + i as i64) as f64 / f64::from(sr);
                if time >= 2.5 {
                    *value = (time * 440.0 * std::f64::consts::TAU).sin() as f32 * 0.5;
                }
            }
        }
        Ok(result)
    }
}
fn open(name: &str, bytes: Arc<[u8]>) -> Option<filmcraft_media::Result<SharedSource>> {
    if !name.ends_with(".ap01") || bytes.as_ref() != b"AP01" {
        return None;
    }
    let g = filmcraft_media::generators::GeneratorSource::new(filmcraft_media::Generator::BarsAndTone, 64, 32, FrameRate::new(24000, 1001), Tick(31 * TPS));
    let mut info = g.info().clone();
    info.kind = filmcraft_media::MediaKind::Movie;
    if name.starts_with("anamorphic") {
        info.video.as_mut().unwrap().par = (2, 1);
    }
    if name.starts_with("low-fps") {
        info.video.as_mut().unwrap().frame_rate = FrameRate::new(5, 1);
    }
    Some(Ok(Arc::new(MarkedSource { info, broken: name.starts_with("broken") })))
}
fn session() -> (Session, u64) {
    named_session("source.ap01")
}
fn named_session(name: &str) -> (Session, u64) {
    let mut s = Session::new(Arc::new(Host));
    s.media.register_opener(open);
    let item = crate::commands::import_bytes(&mut s, name, Arc::from(&b"AP01"[..]), None).unwrap();
    (s, item.0)
}
fn done(s: &mut Session, job: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let v = s.execute("media.analysis.result", json!({"job":job})).unwrap();
        if v["finished"] == true {
            return v;
        }
        assert!(Instant::now() < deadline, "preview did not finish: {v}");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn request(s: &mut Session, params: Value) -> Value {
    let started = s.execute("media.analysis.preview", params).unwrap();
    done(s, started["job"].as_u64().unwrap())
}
#[test]
fn preview_preserves_duration_and_sync() {
    let (mut s, item) = session();
    let before = (serde_json::to_value(&*s.project).unwrap(), serde_json::to_value(&s.state).unwrap(), s.revision, s.history.undo.len());
    let page =
        request(&mut s, json!({"item":item,"start_ticks":(2*TPS).to_string(),"end_ticks":(4*TPS).to_string(),"max_side":64,"video_capability":"verified"}));
    assert_eq!(page["status"], "complete", "{page}");
    assert_eq!(page["preview"]["source_start_ticks"], (2 * TPS).to_string());
    assert_eq!(page["preview"]["duration_ticks"], (2 * TPS).to_string());
    assert_eq!(page["preview"]["frame_rate"], json!({"num":12,"den":1}));
    assert_eq!(page["preview"]["audio"], true);
    let resource = s.execute("media.analysis.preview.read", json!({"artifact":page["preview"]["artifact_id"]})).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(resource["data"].as_str().unwrap()).unwrap();
    assert!(bytes.len() <= 24 * 1024 * 1024);
    let movie = filmcraft_codecs::Mp4Source::open("preview.mp4", bytes.into()).unwrap();
    assert!((movie.info().duration.0 - 2 * TPS).abs() <= TPS / 12);
    assert_eq!((movie.info().video.as_ref().unwrap().width, movie.info().video.as_ref().unwrap().height), (64, 32));
    for (time, green) in [(TPS / 4, false), (3 * TPS / 4, true)] {
        let f = movie.video_frame(FrameRequest::full(Tick(time))).unwrap().to_rgba8();
        assert_eq!(f[1] > f[0], green, "source offset or speed changed");
        let audio = movie.audio(Tick(time).to_units_floor(48000), 2400, 48000).unwrap();
        let amplitude = audio.channels.iter().flatten().map(|v| v.abs()).fold(0.0f32, f32::max);
        assert_eq!(amplitude > 0.05, green, "audio marker not aligned with picture");
    }
    assert_eq!((serde_json::to_value(&*s.project).unwrap(), serde_json::to_value(&s.state).unwrap(), s.revision, s.history.undo.len()), before);
    assert!(s.execute("media.analysis.preview.read", json!({"artifact":"../../private-file"})).is_err());
}
#[test]
fn preview_limits_are_enforced() {
    let (mut s, item) = session();
    let accepted = s.execute("media.analysis.preview", json!({"item":item,"start_ticks":"0","end_ticks":(30*TPS).to_string(),"max_side":32})).unwrap();
    let page = done(&mut s, accepted["job"].as_u64().unwrap());
    assert_eq!(page["status"], "complete", "{page}");
    for extra in [
        json!({"end_ticks":(30*TPS+1).to_string()}),
        json!({"max_fps":13}),
        json!({"max_fps":0}),
        json!({"max_side":1921}),
        json!({"audio":null}),
        json!({"video_capability":"yes"}),
    ] {
        let mut p = json!({"item":item,"start_ticks":"0","end_ticks":(TPS).to_string()});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(s.execute("media.analysis.preview", p.clone()).is_err(), "accepted {p}");
    }
}
#[test]
fn video_unsupported_falls_back_to_frames() {
    let (mut s, item) = session();
    for capability in ["unknown", "unsupported"] {
        let p = request(&mut s, json!({"item":item,"start_ticks":TPS.to_string(),"end_ticks":(2*TPS).to_string(),"max_side":64,"video_capability":capability}));
        assert_eq!(p["status"], "complete", "{p}");
        assert_eq!(p["delivery"], "frames");
        assert_eq!(p["fallback_call"]["name"], "media_frames");
        assert!(!p["frames"].as_array().unwrap().is_empty());
        assert!(p["frames"].as_array().unwrap().iter().all(|f| f["png_base64"].as_str().is_some_and(|b| !b.is_empty())));
        assert!(p.get("preview").is_none());
    }
}

#[test]
fn preview_preserves_unaligned_duration_and_resource_expiration() {
    let (mut s, item) = session();
    let params = json!({"item":item,"request_id":"unaligned","start_ticks":"0","end_ticks":(TPS/7).to_string(),"max_side":32,"audio":false,"video_capability":"verified"});
    let page = request(&mut s, params.clone());
    assert_eq!(page["status"], "complete", "{page}");
    let id = page["preview"]["artifact_id"].clone();
    let resource = s.execute("media.analysis.preview.read", json!({"artifact":id})).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(resource["data"].as_str().unwrap()).unwrap();
    let movie = filmcraft_codecs::Mp4Source::open("preview.mp4", bytes.into()).unwrap();
    assert!(
        (movie.info().duration.0 - TPS / 7).abs() <= TPS / 1_000_000_000 + 1,
        "preview must not extend to a full output frame: {:?}",
        movie.info().duration
    );
    assert!(!movie.info().has_audio());
    s.execute("media.analysis.clearCache", json!({})).unwrap();
    assert!(s.execute("media.analysis.preview.read", json!({"artifact":id})).is_err());
    assert!(s.execute("media.analysis.result", json!({"job":page["job"]})).is_err());
    assert!(s.execute("media.analysis.preview", params).is_err(), "expired request receipts must not silently regenerate");
}

#[test]
fn preview_keeps_low_frame_rates_and_rejects_decode_fallback() {
    let (mut s, item) = named_session("low-fps.ap01");
    let page = request(&mut s, json!({"item":item,"start_ticks":"0","end_ticks":TPS.to_string(),"max_side":64,"audio":false,"video_capability":"verified"}));
    assert_eq!(page["status"], "complete", "{page}");
    assert_eq!(page["preview"]["frame_rate"], json!({"num":5,"den":1}));
    let resource = s.execute("media.analysis.preview.read", json!({"artifact":page["preview"]["artifact_id"]})).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(resource["data"].as_str().unwrap()).unwrap();
    let movie = filmcraft_codecs::Mp4Source::open("preview.mp4", bytes.into()).unwrap();
    assert_eq!(movie.info().frame_rate(), FrameRate::new(5, 1));
    let (mut s, item) = named_session("broken.ap01");
    let page = request(&mut s, json!({"item":item,"start_ticks":"0","end_ticks":TPS.to_string(),"max_side":64,"audio":false,"video_capability":"verified"}));
    assert_eq!(page["error"]["code"], "DECODE_FAILED", "{page}");
    assert_eq!(s.execute("media.analysis.clearCache", json!({})).unwrap()["artifacts_removed"], 0);
}
#[test]
fn cancelled_preview_publishes_no_artifact() {
    let (mut s, item) = session();
    let started = s.execute("media.analysis.preview", json!({"item":item,"max_side":64,"video_capability":"verified"})).unwrap();
    let job = started["job"].as_u64().unwrap();
    s.execute("jobs.cancel", json!({"job":job})).unwrap();
    assert_eq!(done(&mut s, job)["status"], "cancelled");
    assert_eq!(s.execute("media.analysis.clearCache", json!({})).unwrap()["artifacts_removed"], 0);
}

#[test]
fn preview_preserves_source_pixel_aspect_without_squeezing_or_bars() {
    let (mut s, item) = named_session("anamorphic.ap01");
    let page = request(&mut s, json!({"item":item,"start_ticks":"0","end_ticks":TPS.to_string(),"max_side":64,"audio":false,"video_capability":"verified"}));
    assert_eq!(page["status"], "complete", "{page}");
    let resource = s.execute("media.analysis.preview.read", json!({"artifact":page["preview"]["artifact_id"]})).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(resource["data"].as_str().unwrap()).unwrap();
    let movie = filmcraft_codecs::Mp4Source::open("preview.mp4", bytes.into()).unwrap();
    let frame = movie.video_frame(FrameRequest::full(Tick::ZERO)).unwrap();
    assert_eq!((frame.width, frame.height), (64, 16));
    let rgba = frame.to_rgba8();
    for x in [2usize, 32, 61] {
        assert!(rgba[(8 * 64 + x) * 4] > 150, "anamorphic picture must fill its displayed aspect, including both edges");
    }
}
