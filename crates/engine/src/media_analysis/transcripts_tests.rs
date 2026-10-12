use crate::{Services, Session};
use filmcraft_frame::{AudioBuffer, VideoFrame};
use filmcraft_media::{FrameRequest, MediaInfo, MediaSource, SharedSource};
use filmcraft_project::{ItemId, ItemKind, Label, Transcript, Word};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND as TPS, Tick, TimeRange};
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
        panic!("analysis wrote file")
    }
    fn reader(&self, _: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        Some(Ok(Arc::new(filmcraft_media::reader::MemReader(Arc::from(&b"AUDIO"[..])))))
    }
}
struct Source {
    info: MediaInfo,
    broken: bool,
}
impl MediaSource for Source {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, _: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
        panic!("audio query decoded video")
    }
    fn audio(&self, start: i64, frames: usize, rate: u32) -> filmcraft_media::Result<AudioBuffer> {
        assert!(frames <= 4096, "unbounded audio read");
        let mut b = AudioBuffer::silence(rate, 1, frames);
        for (i, v) in b.channels[0].iter_mut().enumerate() {
            let sample = start + i as i64;
            *v = if (24001..72001).contains(&sample) {
                0.0
            } else if self.broken {
                f32::NAN
            } else {
                if sample % 2 == 0 { 0.2 } else { -0.2 }
            };
        }
        Ok(b)
    }
}
fn open(name: &str, bytes: Arc<[u8]>) -> Option<filmcraft_media::Result<SharedSource>> {
    if !name.ends_with(".sound") || bytes.as_ref() != b"AUDIO" {
        return None;
    }
    let src = filmcraft_media::generators::GeneratorSource::new(filmcraft_media::Generator::BarsAndTone, 32, 18, FrameRate::new(25, 1), Tick(2 * TPS));
    let mut info = src.info().clone();
    if name.starts_with("long") {
        info.duration = Tick(601 * TPS);
    }
    info.kind = filmcraft_media::MediaKind::AudioOnly;
    info.video = None;
    info.audio_streams[0].channels = 1;
    info.audio_streams[0].sample_rate = if name.starts_with("long") { 8000 } else { 48000 };
    Some(Ok(Arc::new(Source { info, broken: name.starts_with("bad") })))
}
fn session(name: &str) -> (Session, ItemId) {
    let mut s = Session::new(Arc::new(Host));
    s.media.register_opener(open);
    let item = crate::commands::import_bytes(&mut s, name, Arc::from(&b"AUDIO"[..]), None).unwrap();
    (s, item)
}
fn run(s: &mut Session, command: &str, params: Value) -> Value {
    let ack = s.execute(command, params).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let page = s.execute("media.analysis.result", json!({"job":ack["job"]})).unwrap();
        if page["finished"] == true {
            return page;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn stored(s: &mut Session, item: ItemId, text: &str) {
    Arc::make_mut(&mut s.project).transcripts.insert(
        item,
        Arc::new(Transcript {
            source: "manual".into(),
            language: "zh".into(),
            words: vec![Word::new(text, Tick(TPS / 2), Tick(3 * TPS / 2))],
            ..Default::default()
        }),
    );
}
#[test]
fn stored_transcript_works_without_whisper() {
    let (mut s, item) = session("ok.sound");
    let unavailable = run(&mut s, "media.analysis.transcript", json!({"item":item.0}));
    assert_eq!(unavailable["availability"], "unavailable", "{unavailable}");
    assert_eq!(unavailable["actions"]["import"]["command"], "transcript.set");
    assert_eq!(unavailable["actions"]["generate"]["command"], "transcript.generate");
    stored(&mut s, item, "hello");
    let before = (s.revision, s.history.undo.len(), serde_json::to_value(&s.state).unwrap());
    let page = run(&mut s, "media.analysis.transcript", json!({"item":item.0}));
    assert_eq!(page["words"][0]["text"], "hello", "{page}");
    assert_eq!(page["words"][0]["start_ticks"], (TPS / 2).to_string());
    stored(&mut s, item, "changed");
    let changed = run(&mut s, "media.analysis.transcript", json!({"item":item.0}));
    assert_eq!(changed["words"][0]["text"], "changed", "stale source-version cache");
    assert_eq!((s.revision, s.history.undo.len(), serde_json::to_value(&s.state).unwrap()), before);
}
#[test]
fn subclip_words_use_root_time() {
    let (mut s, item) = session("ok.sound");
    stored(&mut s, item, "whole word");
    let child = Arc::make_mut(&mut s.project).add_item(
        "sub",
        Label::Iris,
        ItemKind::Subclip { parent: item, range: TimeRange::new(Tick(TPS), Tick(TPS)), restrict_trims: true },
        None,
    );
    let page = run(&mut s, "media.analysis.transcript", json!({"item":child.0}));
    assert_eq!(page["root_item"], item.0);
    let word = &page["words"][0];
    assert_eq!(word["start_ticks"], (TPS / 2).to_string());
    assert_eq!(word["end_ticks"], (3 * TPS / 2).to_string());
    assert_eq!(word["overlap"]["start_ticks"], TPS.to_string());
    assert_eq!(word["overlap"]["end_ticks"], (3 * TPS / 2).to_string());
}
#[test]
fn audio_only_media_can_be_analysed() {
    let (mut s, item) = session("ok.sound");
    let before = (s.revision, s.history.undo.len());
    let page = run(&mut s, "media.analysis.audio", json!({"item":item.0,"threshold_db":-40,"min_duration_ticks":(TPS/2).to_string()}));
    assert_eq!(page["status"], "complete", "{page}");
    let ranges = page["silence_ranges"].as_array().unwrap();
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0]["start_sample"], "24001");
    assert_eq!(ranges[0]["end_sample"], "72001");
    assert_eq!(ranges[0]["start_ticks"], Tick::from_units(24001, 48000).0.to_string());
    assert_eq!(page["rhythm"]["availability"], "unavailable");
    assert_eq!(page["loudness"]["availability"], "available");
    assert!(page["loudness"]["integrated_lufs"].is_number());
    assert_eq!((s.revision, s.history.undo.len()), before);
}
#[test]
fn hostile_audio_parameters_and_samples_fail_safely() {
    let (mut s, item) = session("bad.sound");
    for extra in
        [json!({"threshold_db":10}), json!({"threshold_db":-1e30}), json!({"min_duration_ticks":"0"}), json!({"start_ticks":"0"}), json!({"download":true})]
    {
        let mut p = json!({"item":item.0});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(s.execute("media.analysis.audio", p).is_err());
    }
    let page = run(&mut s, "media.analysis.audio", json!({"item":item.0}));
    assert_eq!(page["status"], "error", "{page}");
    assert_eq!(page["error"]["code"], "INVALID_AUDIO");
}

#[test]
fn long_audio_reports_unscanned_range() {
    let (mut s, item) = session("long.sound");
    let page = run(&mut s, "media.analysis.audio", json!({"item":item.0}));
    assert_eq!(page["status"], "partial", "{page}");
    assert_eq!(page["remaining_ranges"][0]["start_ticks"], (600 * TPS).to_string());
    assert_eq!(page["remaining_ranges"][0]["end_ticks"], (601 * TPS).to_string());
}
