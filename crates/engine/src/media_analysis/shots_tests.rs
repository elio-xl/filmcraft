use crate::{Services, Session};
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
        panic!("analysis wrote project services")
    }
    fn reader(&self, _: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        Some(Ok(Arc::new(filmcraft_media::reader::MemReader(Arc::from(&b"SHOT"[..])))))
    }
}
struct Source {
    info: MediaInfo,
    name: String,
}
impl MediaSource for Source {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn analysis_support(&self) -> filmcraft_media::Result<()> {
        Ok(())
    }
    fn video_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        self.analysis_frame(req)
    }
    fn analysis_frame(&self, req: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        let frame = self.info.frame_rate().frame_at(req.time);
        if self.name.starts_with("broken") && frame == 30 {
            return Err(filmcraft_media::MediaError::Decode("synthetic bad frame".into()));
        }
        let color = if self.name.starts_with("flash") {
            if frame == 20 { [255, 255, 255, 255] } else { [220, 20, 20, 255] }
        } else if self.name.starts_with("many") {
            if frame / 6 % 2 == 0 { [220, 20, 20, 255] } else { [20, 220, 20, 255] }
        } else if frame < 20 {
            [220, 20, 20, 255]
        } else if frame < 40 {
            [20, 220, 20, 255]
        } else {
            [20, 20, 220, 255]
        };
        Ok(Arc::new(VideoFrame::rgba8(32, 18, color.repeat(32 * 18)).with_pts(self.info.frame_rate().tick_of(frame))))
    }
    fn audio(&self, _: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        Ok(AudioBuffer::silence(rate, 1, frames))
    }
}
fn open(name: &str, bytes: Arc<[u8]>) -> Option<filmcraft_media::Result<SharedSource>> {
    if !name.ends_with(".shots") || bytes.as_ref() != b"SHOT" {
        return None;
    }
    let rate = if name.starts_with("many") { FrameRate::new(20, 1) } else { FrameRate::new(10, 1) };
    let duration = if name.starts_with("long") || name.starts_with("many") { 601 * TPS } else { 6 * TPS };
    let source = filmcraft_media::generators::GeneratorSource::new(filmcraft_media::Generator::BarsAndTone, 32, 18, rate, Tick(duration));
    let mut info = source.info().clone();
    info.kind = filmcraft_media::MediaKind::Movie;
    Some(Ok(Arc::new(Source { info, name: name.into() })))
}
fn session(name: &str) -> (Session, u64) {
    let mut s = Session::new(Arc::new(Host));
    s.media.register_opener(open);
    let id = crate::commands::import_bytes(&mut s, name, Arc::from(&b"SHOT"[..]), None).unwrap();
    (s, id.0)
}
fn run(s: &mut Session, params: Value) -> Value {
    let ack = s.execute("media.analysis.shots", params).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let page = s.execute("media.analysis.result", json!({"job":ack["job"]})).unwrap();
        if page["finished"] == true {
            return page;
        }
        assert!(Instant::now() < deadline, "shot job timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn tick(v: &Value) -> i64 {
    v.as_str().unwrap().parse().unwrap()
}
#[test]
fn source_shots_do_not_edit_project() {
    let (mut s, item) = session("hard.shots");
    let before = (serde_json::to_value(&*s.project).unwrap(), serde_json::to_value(&s.state).unwrap(), s.revision, s.history.undo.len());
    let page = run(&mut s, json!({"item":item}));
    assert_eq!(page["status"], "complete", "{page}");
    assert_eq!(page["candidate_only"], true);
    let shots = page["shots"].as_array().unwrap();
    assert_eq!(shots.len(), 3, "{page}");
    for (i, shot) in shots.iter().enumerate() {
        assert_eq!(tick(&shot["start_ticks"]), i as i64 * 2 * TPS);
        assert_eq!(tick(&shot["end_ticks"]), (i as i64 + 1) * 2 * TPS);
        assert_eq!(tick(&shot["representative_ticks"]), (i as i64 * 2 + 1) * TPS);
    }
    assert_eq!((serde_json::to_value(&*s.project).unwrap(), serde_json::to_value(&s.state).unwrap(), s.revision, s.history.undo.len()), before);
}
#[test]
fn flash_does_not_claim_semantic_scene_change() {
    let (mut s, item) = session("flash.shots");
    let page = run(&mut s, json!({"item":item,"sensitivity":50,"min_shot_frames":6}));
    assert_eq!(page["status"], "complete", "{page}");
    assert_eq!(page["candidate_only"], true);
    assert_eq!(page["parameters"]["sensitivity"], 50);
    assert_eq!(page["parameters"]["min_shot_frames"], 6);
    let shots = page["shots"].as_array().unwrap();
    assert!(shots.len() < 3);
    for shot in shots {
        assert!(shot.get("scene_name").is_none() && shot.get("summary").is_none());
    }
}
#[test]
fn corrupt_samples_split_coverage_without_inventing_continuity() {
    let (mut s, item) = session("broken.shots");
    let page = run(&mut s, json!({"item":item}));
    assert_eq!(page["status"], "partial", "{page}");
    let missing = page["missing_ranges"].as_array().unwrap();
    assert!(!missing.is_empty());
    assert!(missing.iter().any(|range| tick(&range["start_ticks"]) <= 3 * TPS && tick(&range["end_ticks"]) > 3 * TPS));
    for shot in page["shots"].as_array().unwrap() {
        assert!(!(tick(&shot["start_ticks"]) < 3 * TPS && tick(&shot["end_ticks"]) > 3 * TPS), "shot crosses failed evidence gap: {shot}");
    }
}
#[test]
fn shot_budgets_report_uncovered_ranges_and_reject_hostile_settings() {
    let (mut s, item) = session("long.shots");
    for extra in [
        json!({"sensitivity":-1}),
        json!({"sensitivity":101}),
        json!({"min_shot_frames":0}),
        json!({"min_shot_frames":1e30}),
        json!({"start_ticks":"-1","end_ticks":"1"}),
        json!({"start_ticks":"0"}),
        json!({"applyCuts":true}),
    ] {
        let mut params = json!({"item":item});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(s.execute("media.analysis.shots", params).is_err());
    }
    let page = run(&mut s, json!({"item":item}));
    assert_eq!(page["status"], "partial", "{page}");
    assert_eq!(tick(&page["remaining_ranges"][0]["start_ticks"]), 600 * TPS);
    assert_eq!(tick(&page["remaining_ranges"][0]["end_ticks"]), 601 * TPS);
    let (mut s, item) = session("many.shots");
    let page = run(&mut s, json!({"item":item,"min_shot_frames":1}));
    assert_eq!(page["status"], "partial", "{page}");
    assert_eq!(page["shots"].as_array().unwrap().len(), 1000);
    assert!(!page["remaining_ranges"].as_array().unwrap().is_empty());
}
