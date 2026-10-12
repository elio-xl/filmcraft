//! Analysis boundaries: reject hostile requests before allocating or decoding.

use super::request::{parse, sample_times};
use filmcraft_time::{Tick, TimeRange};
use serde_json::{Value, json};

fn available() -> TimeRange {
    TimeRange::new(Tick::ZERO, Tick(254_016_000_000))
}

#[test]
fn analysis_request_defaults_produce_eight_bounded_samples() {
    let r = parse(&json!({"item": 1})).unwrap();
    assert_eq!(r.limits.max_side, 512);
    assert_eq!(r.limits.max_frames, 8);
    assert_eq!(
        sample_times(&r, available()).unwrap(),
        vec![
            Tick(0),
            Tick(31_752_000_000),
            Tick(63_504_000_000),
            Tick(95_256_000_000),
            Tick(127_008_000_000),
            Tick(158_760_000_000),
            Tick(190_512_000_000),
            Tick(222_264_000_000),
        ]
    );
}

#[test]
fn analysis_request_limits_reject_before_sampling() {
    for (key, values) in [
        ("max_side", vec![json!(0), json!(-1), json!(1921), json!(1.5), json!("512"), Value::Null]),
        ("max_frames", vec![json!(0), json!(-1), json!(17), json!(1.5), json!(u64::MAX), Value::Null]),
        ("item", vec![json!(0), json!(-1), json!(1.5), json!("1"), Value::Null]),
    ] {
        for value in values {
            let mut p = json!({"item": 1});
            p[key] = value;
            assert!(parse(&p).is_err(), "accepted {p}");
        }
    }
    let r = parse(&json!({"item": 1, "max_side": 1920, "max_frames": 16})).unwrap();
    assert_eq!(r.limits.max_side, 1920);
    assert_eq!(sample_times(&r, available()).unwrap().len(), 16);
    for p in [json!({}), Value::Null, json!([]), json!({"item":1,"max_sdie":512})] {
        assert!(parse(&p).is_err(), "accepted {p}");
    }
}

#[test]
fn uniform_sampling_is_half_open() {
    let r = parse(&json!({"item": 1, "max_frames": 2})).unwrap();
    assert_eq!(sample_times(&r, available()).unwrap(), vec![Tick(0), Tick(127_008_000_000)]);
    let r = parse(&json!({"item":1,"start_ticks":"10","end_ticks":"15","max_frames":2})).unwrap();
    assert_eq!(sample_times(&r, available()).unwrap(), vec![Tick(10), Tick(12)]);
    let r = parse(&json!({"item":1,"start_ticks":"10","end_ticks":"11","max_frames":16})).unwrap();
    assert_eq!(sample_times(&r, available()).unwrap(), vec![Tick(10)]);
}

#[test]
fn analysis_request_requires_exact_nonnegative_time_strings() {
    for value in [
        json!(-1),
        json!(0),
        json!(1.5),
        json!("-1"),
        json!("+1"),
        json!(" 1"),
        json!("1.0"),
        json!("NaN"),
        json!("inf"),
        json!("9223372036854775808"),
        json!(""),
        Value::Null,
    ] {
        let p = json!({"item":1,"start_ticks":value,"end_ticks":"100"});
        assert!(parse(&p).is_err(), "accepted {p}");
    }
    let r = parse(&json!({"item":1,"start_ticks":"9007199254740993","end_ticks":"9007199254740995","max_frames":2})).unwrap();
    assert_eq!(sample_times(&r, TimeRange::new(Tick::ZERO, Tick(i64::MAX))).unwrap(), vec![Tick(9_007_199_254_740_993), Tick(9_007_199_254_740_994)]);
}

#[test]
fn analysis_request_checks_interval_and_available_range() {
    for p in [
        json!({"item":1,"start_ticks":"10"}),
        json!({"item":1,"end_ticks":"10"}),
        json!({"item":1,"start_ticks":"10","end_ticks":"10"}),
        json!({"item":1,"start_ticks":"11","end_ticks":"10"}),
    ] {
        assert!(parse(&p).is_err(), "accepted {p}");
    }
    let r = parse(&json!({"item":1,"start_ticks":"9","end_ticks":"20"})).unwrap();
    assert!(sample_times(&r, TimeRange::new(Tick(10), Tick(10))).is_err());
    let r = parse(&json!({"item":1})).unwrap();
    for range in
        [TimeRange::new(Tick(0), Tick(0)), TimeRange::new(Tick(-1), Tick(10)), TimeRange::new(Tick(i64::MAX), Tick(1)), TimeRange::new(Tick(10), Tick(-1))]
    {
        assert!(sample_times(&r, range).is_err(), "accepted {range:?}");
    }
}

#[test]
fn analysis_request_timestamp_sampling_keeps_request_order() {
    let r = parse(&json!({"item":1,"strategy":"timestamps","times_ticks":["20","0","20"]})).unwrap();
    assert_eq!(sample_times(&r, available()).unwrap(), vec![Tick(20), Tick(0), Tick(20)]);
    for p in [
        json!({"item":1,"strategy":"timestamps","times_ticks":[]}),
        json!({"item":1,"strategy":"timestamps","times_ticks":vec!["0";17]}),
        json!({"item":1,"strategy":"timestamps"}),
        json!({"item":1,"strategy":"uniform","times_ticks":["0"]}),
        json!({"item":1,"strategy":"timestamps","times_ticks":["0"],"start_ticks":"0","end_ticks":"1"}),
        json!({"item":1,"strategy":"timestamps","times_ticks":["0"],"max_frames":8}),
        json!({"item":1,"strategy":"shots"}),
    ] {
        assert!(parse(&p).is_err(), "accepted {p}");
    }
    let r = parse(&json!({"item":1,"strategy":"timestamps","times_ticks":["254016000000"]})).unwrap();
    assert!(sample_times(&r, available()).is_err());
}

#[test]
fn analysis_request_bounds_identifiers() {
    assert!(parse(&json!({"item":1,"request_id":"a".repeat(128)})).is_ok());
    for p in [
        json!({"item":1,"request_id":"a".repeat(129)}),
        json!({"item":1,"request_id":""}),
        json!({"item":1,"request_id":12}),
        json!({"item":1,"expected_version":""}),
        json!({"item":1,"expected_version":"a".repeat(257)}),
    ] {
        assert!(parse(&p).is_err(), "accepted {p}");
    }
}

#[test]
fn analysis_request_hostile_values_never_panic() {
    for value in [Value::Null, json!(false), json!(-1), json!(u64::MAX), json!([]), json!({}), json!("x".repeat(1024))] {
        for key in ["item", "start_ticks", "end_ticks", "max_side", "max_frames", "times_ticks", "strategy", "request_id", "expected_version"] {
            let mut p = json!({"item":1});
            p[key] = value.clone();
            assert!(
                std::panic::catch_unwind(|| {
                    if let Ok(r) = parse(&p) {
                        let _ = sample_times(&r, available());
                    }
                })
                .is_ok(),
                "panicked for {p}"
            );
        }
    }
}

#[test]
fn analysis_request_large_intervals_do_not_overflow() {
    let r = parse(&json!({"item":1,"max_frames":16})).unwrap();
    let times = sample_times(&r, TimeRange::new(Tick::ZERO, Tick(i64::MAX))).unwrap();
    assert_eq!(times.len(), 16);
    assert_eq!(times.first(), Some(&Tick::ZERO));
    assert_eq!(times.last(), Some(&Tick(8_646_911_284_551_352_319)));
    assert!(times.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn analysis_request_strategy_is_explicit_and_identifiers_are_bounded() {
    for p in
        [json!({"item":1,"times_ticks":["0"]}), json!({"item":1,"strategy":null}), json!({"item":1,"strategy":0}), json!({"item":1,"request_id":"\u{0000}"})]
    {
        assert!(parse(&p).is_err(), "accepted {p}");
    }
    assert!(parse(&json!({"item":1,"request_id":"剪".repeat(128)})).is_ok());
    assert!(parse(&json!({"item":1,"request_id":"剪".repeat(129)})).is_err());
}

#[test]
fn analysis_request_inspect_accepts_only_common_fields() {
    use super::request::parse_inspect;
    let request = parse_inspect(&json!({"item":3,"request_id":"inspect-1","expected_version":"sha256:known"})).unwrap();
    assert_eq!(request.item, filmcraft_project::ItemId(3));
    for p in [
        json!({"item":3,"max_side":512}),
        json!({"item":3,"max_frames":8}),
        json!({"item":3,"start_ticks":"0","end_ticks":"10"}),
        json!({"item":3,"wait":true}),
        json!({"item":0}),
        json!({"item":3,"expected_version":null}),
    ] {
        assert!(parse_inspect(&p).is_err(), "accepted {p}");
    }
}

use super::source;
use crate::{Services, Session};
use filmcraft_project::{ItemId, ItemKind, Label};
use std::sync::{Arc, Mutex};

struct AnalysisHost {
    bytes: Mutex<Vec<u8>>,
}
impl Services for AnalysisHost {
    fn read_file(&self, _path: &str) -> std::io::Result<Vec<u8>> {
        Ok(self.bytes.lock().unwrap().clone())
    }
    fn write_file(&self, _path: &str, _bytes: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("read-only test host"))
    }
    fn reader(&self, _path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
        Some(Ok(Arc::new(filmcraft_media::reader::MemReader(self.bytes.lock().unwrap().clone().into()))))
    }
}

fn analysis_image_session() -> (Session, ItemId, Arc<AnalysisHost>) {
    let mut png = std::io::Cursor::new(Vec::new());
    image::write_buffer_with_format(&mut png, &[255; 8 * 4 * 4], 8, 4, image::ExtendedColorType::Rgba8, image::ImageFormat::Png).unwrap();
    let bytes = png.into_inner();
    let host = Arc::new(AnalysisHost { bytes: Mutex::new(bytes.clone()) });
    let mut session = Session::new(host.clone());
    let item = crate::commands::import_bytes(&mut session, "source.png", bytes.into(), None).unwrap();
    (session, item, host)
}

#[test]
fn unplaced_media_is_inspectable() {
    let (session, item, _) = analysis_image_session();
    assert!(session.state.active_sequence.is_none());
    let project = serde_json::to_value(&*session.project).unwrap();
    let state = serde_json::to_value(&session.state).unwrap();
    let revision = (session.revision, session.saved_revision);
    let history = (session.history.undo.len(), session.history.redo.len());
    let input = source::resolve(&session, item).unwrap();
    let info = source::inspect(&input).unwrap();
    assert_eq!(info["item"], item.0);
    assert_eq!(info["root_item"], item.0);
    assert_eq!(info["coordinate_space"], "source");
    assert_eq!(info["video"]["width"], 8);
    assert_eq!(info["video"]["height"], 4);
    assert_eq!(serde_json::to_value(&*session.project).unwrap(), project);
    assert_eq!(serde_json::to_value(&session.state).unwrap(), state);
    assert_eq!((session.revision, session.saved_revision), revision);
    assert_eq!((session.history.undo.len(), session.history.redo.len()), history);
}

#[test]
fn offline_slate_is_never_analysis_content() {
    let (mut session, item, host) = analysis_image_session();
    let ItemKind::Media(clip) = &mut Arc::make_mut(&mut session.project).item_mut(item).unwrap().kind else { panic!() };
    clip.offline = true;
    assert!(source::resolve(&session, item).is_err());
    let ItemKind::Media(clip) = &mut Arc::make_mut(&mut session.project).item_mut(item).unwrap().kind else { panic!() };
    clip.offline = false;
    *host.bytes.lock().unwrap() = b"corrupt or unsupported source".to_vec();
    assert!(source::resolve(&session, item).is_err(), "must reopen raw data instead of cached display source");
    assert!(source::resolve(&session, ItemId(u64::MAX)).is_err());
}

#[test]
fn subclip_analysis_has_root_coordinates() {
    let (mut session, root, _) = analysis_image_session();
    let project = Arc::make_mut(&mut session.project);
    let parent =
        project.add_item("parent", Label::Iris, ItemKind::Subclip { parent: root, range: TimeRange::new(Tick(10), Tick(100)), restrict_trims: true }, None);
    let child = project.add_item("child", Label::Iris, ItemKind::Subclip { parent, range: TimeRange::new(Tick(40), Tick(20)), restrict_trims: true }, None);
    let result = source::resolve(&session, child).unwrap();
    assert_eq!(result.root_item, root);
    assert_eq!(result.available, TimeRange::new(Tick(40), Tick(20)));
    let info = source::inspect(&result).unwrap();
    assert_eq!(info["source_range"]["start_ticks"], "40");
    assert_eq!(info["source_range"]["end_ticks"], "60");
}

#[test]
fn cyclic_deep_and_invalid_ranges_fail_without_panicking() {
    let (mut session, root, _) = analysis_image_session();
    let mut parent = root;
    for depth in 1..=16 {
        parent = Arc::make_mut(&mut session.project).add_item(
            "nested",
            Label::Iris,
            ItemKind::Subclip { parent, range: TimeRange::new(Tick::ZERO, Tick(100)), restrict_trims: true },
            None,
        );
        if depth == 15 {
            assert!(source::resolve(&session, parent).is_ok());
        }
        if depth == 16 {
            assert!(source::resolve(&session, parent).is_err());
        }
    }
    let ItemKind::Subclip { parent: reference, .. } = &mut Arc::make_mut(&mut session.project).item_mut(parent).unwrap().kind else { panic!() };
    *reference = parent;
    assert!(source::resolve(&session, parent).is_err());
    for range in [TimeRange::new(Tick(-1), Tick(1)), TimeRange::new(Tick::ZERO, Tick::ZERO), TimeRange::new(Tick(i64::MAX), Tick(1))] {
        let id = Arc::make_mut(&mut session.project).add_item("bad range", Label::Iris, ItemKind::Subclip { parent: root, range, restrict_trims: true }, None);
        assert!(source::resolve(&session, id).is_err());
    }
}

#[test]
fn analysis_snapshot_does_not_read_before_worker_opens_it() {
    struct DenyReads;
    impl Services for DenyReads {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            panic!("capture must not read")
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("capture must not write")
        }
        fn reader(&self, _: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
            panic!("capture must not open a reader")
        }
    }
    let (mut session, item, _) = analysis_image_session();
    session.services = Arc::new(DenyReads);
    assert!(source::AnalysisInput::capture(&session, item).is_ok());
}

#[test]
fn hosts_without_readers_must_opt_into_bounded_reads() {
    struct BoundedHost(Vec<u8>);
    impl Services for BoundedHost {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            panic!("must use the bounded host contract")
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("analysis must not write")
        }
        fn read_file_bounded(&self, _: &str, cap: u64) -> std::io::Result<Vec<u8>> {
            assert_eq!(cap, 64 * 1024 * 1024);
            Ok(self.0.clone())
        }
    }
    struct UnboundedHost;
    impl Services for UnboundedHost {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            panic!("unbounded host reads are forbidden")
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("analysis must not write")
        }
    }
    let (mut session, item, host) = analysis_image_session();
    session.services = Arc::new(BoundedHost(host.bytes.lock().unwrap().clone()));
    assert!(source::resolve(&session, item).is_ok());
    session.services = Arc::new(UnboundedHost);
    assert!(source::resolve(&session, item).is_err());
}

#[test]
fn audio_inspection_reports_audio_without_a_fake_video_stream() {
    let bytes = filmcraft_media::wav::write_wav16(&[0.0; 480], 1, 48000);
    let host = Arc::new(AnalysisHost { bytes: Mutex::new(bytes.clone()) });
    let mut session = Session::new(host);
    let item = crate::commands::import_bytes(&mut session, "audio.wav", bytes.into(), None).unwrap();
    let source = source::resolve(&session, item).unwrap();
    let info = source::inspect(&source).unwrap();
    assert!(info["video"].is_null());
    assert_eq!(info["audio_streams"][0]["sample_rate"], 48000);
    assert_eq!(info["duration_ticks"], "2540160000");
}

#[test]
fn still_subclips_use_the_projects_configured_duration() {
    let (mut session, root, _) = analysis_image_session();
    let project = Arc::make_mut(&mut session.project);
    let ItemKind::Media(media) = &mut project.item_mut(root).unwrap().kind else { panic!() };
    media.info.duration = Tick(10 * filmcraft_time::TICKS_PER_SECOND);
    let span = TimeRange::new(Tick(6 * filmcraft_time::TICKS_PER_SECOND), Tick(2 * filmcraft_time::TICKS_PER_SECOND));
    let child = project.add_item("late still subclip", Label::Iris, ItemKind::Subclip { parent: root, range: span, restrict_trims: true }, None);
    let source = source::resolve(&session, child).unwrap();
    assert_eq!(source.available, span);
    let info = source::inspect(&source).unwrap();
    assert_eq!(info["duration_ticks"], "2540160000000");
    assert_eq!(info["duration_origin"], "project_still_duration");
}

#[test]
fn oversized_whole_files_fail_before_payload_allocation_or_reads() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct HugeReader(AtomicUsize);
    impl filmcraft_media::ByteReader for HugeReader {
        fn len(&self) -> u64 {
            source::MAX_WHOLE_BYTES + 1
        }
        fn read_at(&self, _: u64, bytes: &mut [u8]) -> std::io::Result<()> {
            self.0.fetch_add(bytes.len(), Ordering::SeqCst);
            bytes.fill(0);
            Ok(())
        }
    }
    struct HugeHost(Arc<HugeReader>);
    impl Services for HugeHost {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            panic!("unbounded read")
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("write")
        }
        fn reader(&self, _: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
            Some(Ok(self.0.clone()))
        }
    }
    let (mut session, item, _) = analysis_image_session();
    let reader = Arc::new(HugeReader(AtomicUsize::new(0)));
    session.services = Arc::new(HugeHost(reader.clone()));
    let error = source::resolve(&session, item).err().unwrap();
    assert!(matches!(error, crate::EngineError::Analysis(ref e) if e.code == "RESOURCE_LIMIT"));
    assert!(reader.0.load(Ordering::SeqCst) <= filmcraft_media::reader::HEAD_LEN);
}

mod frame_output {
    use super::super::frames;
    use super::*;
    use base64::Engine;
    use filmcraft_frame::{PixelData, VideoFrame};
    use filmcraft_media::{FrameRequest, MediaError, MediaInfo, MediaKind, MediaSource};

    struct TimedSource {
        info: MediaInfo,
        frames: Vec<Arc<VideoFrame>>,
        fail_at: Vec<Tick>,
    }
    impl MediaSource for TimedSource {
        fn info(&self) -> &MediaInfo {
            &self.info
        }
        fn video_frame(&self, _: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
            panic!("must use the verified analysis method")
        }
        fn analysis_frame(&self, request: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
            if self.fail_at.contains(&request.time) {
                return Err(MediaError::Decode("injected frame failure".into()));
            }
            self.frames
                .iter()
                .filter(|f| f.pts <= request.time)
                .max_by_key(|f| f.pts)
                .cloned()
                .ok_or_else(|| MediaError::Decode("no source frame at this time".into()))
        }
        fn audio(&self, _: i64, _: usize, _: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
            Err(MediaError::NoStream("audio"))
        }
    }
    fn fixture(frames: Vec<Arc<VideoFrame>>, par: (u32, u32), fail_at: Vec<Tick>) -> source::AnalysisSource {
        let prototype = filmcraft_media::still::StillSource::from_rgba("test.png", 1, 1, vec![0, 0, 0, 255], false, 0);
        let mut info = prototype.info().clone();
        info.kind = MediaKind::Movie;
        info.duration = available().duration;
        let video = info.video.as_mut().unwrap();
        video.width = frames[0].width;
        video.height = frames[0].height;
        video.par = par;
        source::AnalysisSource {
            item: ItemId(1),
            root_item: ItemId(1),
            name: "fixture".into(),
            source: Arc::new(TimedSource { info, frames, fail_at }),
            source_duration: available().duration,
            duration_origin: "source",
            available: available(),
            services: Arc::new(crate::FsServices),
            identity: source::SourceIdentity::File { path: "fixture.mov".into() },
        }
    }
    fn frame(w: u32, h: u32, pts: Tick) -> Arc<VideoFrame> {
        Arc::new(VideoFrame::rgba8(w, h, [255, 0, 0, 127].repeat((w * h) as usize)).with_pts(pts))
    }
    fn png(frame: &Value) -> image::RgbaImage {
        let bytes = base64::engine::general_purpose::STANDARD.decode(frame["png_base64"].as_str().unwrap()).unwrap();
        image::load_from_memory(&bytes).unwrap().to_rgba8()
    }

    #[test]
    fn analysis_uses_presentation_time() {
        let ms = filmcraft_time::TICKS_PER_SECOND / 1000;
        let source = fixture(vec![frame(2, 2, Tick(0)), frame(2, 2, Tick(40 * ms)), frame(2, 2, Tick(100 * ms))], (1, 1), vec![]);
        let request = parse(&json!({"item":1,"strategy":"timestamps","times_ticks":[(70*ms).to_string()]})).unwrap();
        let page = frames::decode(&source, &request).unwrap();
        assert_eq!(page["status"], "complete");
        assert_eq!(page["frames"][0]["actual_ticks"], (40 * ms).to_string());
        assert_eq!(page["frames"][0]["requested_ticks"], json!([(70 * ms).to_string()]));
        let source = fixture(vec![frame(2, 2, Tick(50 * ms)), frame(2, 2, Tick(100 * ms))], (1, 1), vec![]);
        assert_eq!(frames::decode(&source, &request).unwrap()["frames"][0]["actual_ticks"], (50 * ms).to_string());
    }

    #[test]
    fn duplicate_requests_keep_all_indices_and_sort_by_actual_pts() {
        let ms = filmcraft_time::TICKS_PER_SECOND / 1000;
        let source = fixture(vec![frame(2, 2, Tick(0)), frame(2, 2, Tick(40 * ms)), frame(2, 2, Tick(100 * ms))], (1, 1), vec![]);
        let request =
            parse(&json!({"item":1,"strategy":"timestamps","times_ticks":[(70*ms).to_string(),(40*ms).to_string(),(100*ms).to_string(),"0"]})).unwrap();
        let page = frames::decode(&source, &request).unwrap();
        let values = page["frames"].as_array().unwrap();
        assert_eq!(values.len(), 3);
        assert_eq!(values[0]["actual_ticks"], "0");
        assert_eq!(values[0]["request_indices"], json!([3]));
        assert_eq!(values[1]["request_indices"], json!([0, 1]));
        assert_eq!(values[1]["requested_ticks"], json!([(70 * ms).to_string(), (40 * ms).to_string()]));
        assert_eq!(values[2]["actual_ticks"], (100 * ms).to_string());
    }

    #[test]
    fn analysis_preserves_display_geometry_and_alpha() {
        let landscape = frame(1920, 1080, Tick::ZERO);
        let portrait = Arc::new(landscape.rotated(1));
        for (source, expected) in [
            (fixture(vec![portrait], (1, 1), vec![]), (288, 512)),
            (fixture(vec![frame(720, 576, Tick::ZERO)], (16, 15), vec![]), (512, 384)),
            (fixture(vec![frame(8, 4, Tick::ZERO)], (1, 1), vec![]), (8, 4)),
        ] {
            let request = parse(&json!({"item":1,"max_frames":1})).unwrap();
            let page = frames::decode(&source, &request).unwrap();
            let result = &page["frames"][0];
            assert_eq!(result["width"], expected.0);
            assert_eq!(result["height"], expected.1);
            assert_eq!(result["mime_type"], "image/png");
            let image = png(result);
            assert_eq!(image.dimensions(), expected);
            assert_eq!(image.get_pixel(0, 0).0[3], 127);
        }
    }

    #[test]
    fn analysis_partial_failure_keeps_other_frames() {
        let source = fixture(vec![frame(2, 2, Tick(0)), frame(2, 2, Tick(100))], (1, 1), vec![Tick(70)]);
        let request = parse(&json!({"item":1,"strategy":"timestamps","times_ticks":["0","70","100"]})).unwrap();
        let page = frames::decode(&source, &request).unwrap();
        assert_eq!(page["status"], "partial");
        assert_eq!(page["frames"].as_array().unwrap().len(), 2);
        assert_eq!(page["errors"][0]["request_index"], 1);
        assert_eq!(page["errors"][0]["code"], "DECODE_FAILED");
        let request = parse(&json!({"item":1,"strategy":"timestamps","times_ticks":["70"]})).unwrap();
        let page = frames::decode(&source, &request).unwrap();
        assert_eq!(page["status"], "error");
        assert!(page["frames"].as_array().unwrap().is_empty());
    }

    #[test]
    fn analysis_rejects_malformed_buffers_without_panicking() {
        for data in [
            PixelData::Rgba8(Arc::new(vec![0; 3])),
            PixelData::RgbaF32(Arc::new(vec![f32::NAN; 16])),
            PixelData::Yuv16 { planes: std::array::from_fn(|_| Arc::new(vec![0; 4])), chroma: filmcraft_frame::Chroma::C444, bits: 0, alpha: None },
        ] {
            let f = VideoFrame { width: 2, height: 2, data, color: filmcraft_color::ColorInfo::SRGB_FULL, par: (1, 1), pts: Tick::ZERO };
            let source = fixture(vec![Arc::new(f)], (1, 1), vec![]);
            let request = parse(&json!({"item":1,"max_frames":1})).unwrap();
            let page = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| frames::decode(&source, &request))).unwrap().unwrap();
            assert_eq!(page["status"], "error");
            assert_eq!(page["errors"][0]["code"], "INVALID_FRAME");
            assert!(page["frames"].as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn analysis_does_not_guess_pts_from_unverified_sources() {
        struct Unverified(MediaInfo);
        impl MediaSource for Unverified {
            fn info(&self) -> &MediaInfo {
                &self.0
            }
            fn video_frame(&self, _: FrameRequest) -> filmcraft_media::Result<Arc<VideoFrame>> {
                panic!("unverified frame must not be consumed")
            }
            fn audio(&self, _: i64, _: usize, _: u32) -> filmcraft_media::Result<filmcraft_frame::AudioBuffer> {
                Err(MediaError::NoStream("audio"))
            }
        }
        let mut source = fixture(vec![frame(2, 2, Tick(0))], (1, 1), vec![]);
        source.source = Arc::new(Unverified(source.source.info().clone()));
        let page = frames::decode(&source, &parse(&json!({"item":1,"max_frames":1})).unwrap()).unwrap();
        assert_eq!(page["status"], "error");
        assert_eq!(page["errors"][0]["code"], "UNSUPPORTED_MEDIA");
    }
    #[test]
    fn response_budget_keeps_complete_frames_and_remaining_times() {
        let mut noise = Vec::with_capacity(512 * 512 * 4);
        let mut seed = 37u32;
        for _ in 0..512 * 512 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            noise.extend_from_slice(&[seed as u8, (seed >> 8) as u8, (seed >> 16) as u8, 255]);
        }
        let images = (0..16).map(|i| Arc::new(VideoFrame::rgba8(512, 512, noise.clone()).with_pts(Tick(i)))).collect();
        let source = fixture(images, (1, 1), vec![]);
        let request = parse(&json!({"item":1,"strategy":"timestamps","times_ticks":(0..16).map(|i|i.to_string()).collect::<Vec<_>>()})).unwrap();
        let page = frames::decode(&source, &request).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 8 * 1024 * 1024);
        let images = page["frames"].as_array().unwrap();
        assert!(!images.is_empty() && images.len() < 16);
        for image in images {
            assert_eq!(png(image).dimensions(), (512, 512));
        }
        let remaining = page["remaining_times_ticks"].as_array().unwrap();
        assert_eq!(images.len() + remaining.len(), 16);
        assert_eq!(page["status"], "partial");
    }
}

#[test]
fn analysis_sequence_opens_bounded_source_without_display_cache() {
    struct Host(Arc<AnalysisHost>);
    impl Services for Host {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            panic!("unbounded read")
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("write")
        }
        fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
            self.0.reader(path)
        }
        fn list_dir_bounded(&self, _: &str, _: usize) -> std::io::Result<Vec<String>> {
            Ok(vec!["s001.png".into(), "s003.png".into()])
        }
    }
    let (mut session, item, host) = analysis_image_session();
    session.services = Arc::new(Host(host));
    let ItemKind::Media(media) = &mut Arc::make_mut(&mut session.project).item_mut(item).unwrap().kind else { panic!() };
    media.info.kind = filmcraft_media::MediaKind::ImageSequence;
    media.info.video.as_mut().unwrap().frame_rate = filmcraft_time::FrameRate::FPS_25;
    media.media = filmcraft_project::MediaRef::File { path: "s001.png".into() };
    let source = source::resolve(&session, item).unwrap();
    assert_eq!(source.source_duration, filmcraft_time::FrameRate::FPS_25.tick_of(3));
    let request = parse(&json!({"item":item.0,"strategy":"timestamps","times_ticks":[filmcraft_time::FrameRate::FPS_25.tick_of(1).0.to_string()]})).unwrap();
    let page = super::frames::decode(&source, &request).unwrap();
    assert_eq!(page["status"], "complete");
    assert_eq!(page["frames"][0]["actual_ticks"], "0");
}
mod queued_analysis {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use web_time::Instant;
    struct Host {
        source: Arc<AnalysisHost>,
        mode: AtomicUsize,
        entered: AtomicUsize,
    }
    impl Services for Host {
        fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
            panic!("unbounded read")
        }
        fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            panic!("write")
        }
        fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(5);
            while self.mode.load(Ordering::SeqCst) == 1 {
                if filmcraft_media::cancel::cancelled() {
                    return Some(Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")));
                }
                assert!(Instant::now() < until, "test host release timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_ne!(self.mode.load(Ordering::SeqCst), 2, "injected worker panic");
            self.source.reader(path)
        }
    }
    fn session(mode: usize) -> (Session, ItemId, Arc<Host>) {
        let (mut s, item, source) = analysis_image_session();
        let host = Arc::new(Host { source, mode: AtomicUsize::new(mode), entered: AtomicUsize::new(0) });
        s.services = host.clone();
        (s, item, host)
    }
    fn wait_for(mut f: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < until, "job did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn start(s: &mut Session, item: ItemId) -> u64 {
        s.execute("media.analysis.inspect", json!({"item":item.0})).unwrap()["job"].as_u64().unwrap()
    }
    fn result(s: &mut Session, id: u64) -> Value {
        s.execute("media.analysis.result", json!({"job":id})).unwrap()
    }
    fn finished(s: &mut Session, id: u64) -> Value {
        wait_for(|| result(s, id)["finished"] == true);
        result(s, id)
    }
    #[test]
    fn analysis_queue_is_bounded_and_ids_are_shared() {
        let (mut s, item, host) = session(1);
        let existing = s.allocate_job_id().unwrap();
        s.jobs.push(crate::Job { id: existing, label: "export fixture".into(), progress: Default::default(), result: Default::default() });
        let active = start(&mut s, item);
        wait_for(|| host.entered.load(Ordering::SeqCst) > 0);
        let queued: Vec<_> = (0..8).map(|_| start(&mut s, item)).collect();
        assert!(matches!(s.execute("media.analysis.inspect",json!({"item":item.0})),Err(crate::EngineError::Analysis(ref e)) if e.code=="ANALYSIS_BUSY"));
        assert_eq!(s.execute("jobs.list", json!({})).unwrap().as_array().unwrap().len(), 10);
        let next = s.allocate_job_id().unwrap();
        assert!(existing < active && active < queued[0] && queued[7] < next);
        host.mode.store(0, Ordering::SeqCst);
        assert_eq!(finished(&mut s, active)["status"], "complete");
        for id in queued {
            assert_eq!(finished(&mut s, id)["status"], "complete");
        }
    }
    #[test]
    fn cancel_and_panic_leave_terminal_jobs() {
        let (mut s, item, host) = session(1);
        let active = start(&mut s, item);
        wait_for(|| host.entered.load(Ordering::SeqCst) > 0);
        let queued = start(&mut s, item);
        s.execute("jobs.cancel", json!({"job":queued})).unwrap();
        assert_eq!(finished(&mut s, queued)["status"], "cancelled");
        s.execute("jobs.cancel", json!({"job":active})).unwrap();
        assert_eq!(finished(&mut s, active)["status"], "cancelled");
        assert_eq!(host.entered.load(Ordering::SeqCst), 1, "cancelled queued work must not open sources");
        host.mode.store(2, Ordering::SeqCst);
        let failed = start(&mut s, item);
        assert_eq!(finished(&mut s, failed)["error"]["code"], "WORKER_PANIC");
        host.mode.store(0, Ordering::SeqCst);
        let good = start(&mut s, item);
        assert_eq!(finished(&mut s, good)["status"], "complete");
    }
    #[test]
    fn analysis_does_not_hold_session_lock_or_mutate_editor() {
        let (s, item, host) = session(1);
        let project = serde_json::to_value(&*s.project).unwrap();
        let state = serde_json::to_value(&s.state).unwrap();
        let history = (s.history.undo.len(), s.history.redo.len());
        let session = Arc::new(Mutex::new(s));
        let id = start(&mut session.lock().unwrap(), item);
        wait_for(|| host.entered.load(Ordering::SeqCst) > 0);
        let mut s = session.try_lock().expect("worker holds live session lock");
        s.execute("project.inspect", json!({})).unwrap();
        s.execute("jobs.list", json!({})).unwrap();
        s.execute("jobs.cancel", json!({"job":id})).unwrap();
        assert_eq!(finished(&mut s, id)["status"], "cancelled");
        assert_eq!(serde_json::to_value(&*s.project).unwrap(), project);
        assert_eq!(serde_json::to_value(&s.state).unwrap(), state);
        assert_eq!((s.history.undo.len(), s.history.redo.len()), history);
    }
    #[test]
    fn analysis_frame_job_keeps_images_out_of_job_listing() {
        let (mut s, item, _) = session(0);
        let response = s.execute("media.analysis.frames", json!({"item":item.0,"max_frames":2})).unwrap();
        assert!(response.get("frames").is_none());
        let id = response["job"].as_u64().unwrap();
        let page = finished(&mut s, id);
        assert_eq!(page["status"], "complete");
        assert_eq!(page["frames"].as_array().unwrap().len(), 1);
        assert!(page["frames"][0]["png_base64"].as_str().unwrap().starts_with("iVBOR"));
        assert!(!s.execute("jobs.list", json!({})).unwrap().to_string().contains("png_base64"));
    }
}

#[test]
fn shared_job_allocator_does_not_reuse_or_wrap_ids() {
    let mut session = Session::default();
    assert_eq!(session.allocate_job_id().unwrap(), 1);
    assert_eq!(session.allocate_job_id().unwrap(), 2);
    session.jobs.push(crate::Job { id: 99, label: "external".into(), progress: Default::default(), result: Default::default() });
    assert_eq!(session.allocate_job_id().unwrap(), 100);
    session.jobs.clear();
    assert_eq!(session.allocate_job_id().unwrap(), 101);
    session.jobs.push(crate::Job { id: u64::MAX, label: "hostile".into(), progress: Default::default(), result: Default::default() });
    assert!(session.allocate_job_id().is_err());
}

#[test]
fn analysis_result_rejects_hostile_job_arguments() {
    let mut session = Session::default();
    for params in [json!({}), json!({"job":0}), json!({"job":-1}), json!({"job":1.5}), json!({"job":"1"}), json!({"job":1,"extra":true}), Value::Null] {
        assert!(session.execute("media.analysis.result", params).is_err());
    }
    assert!(session.execute("media.analysis.result", json!({"job":1})).is_err());
}

mod versioned_analysis {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use web_time::Instant;
    fn done(s: &mut Session, job: u64) -> Value {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let value = s.execute("media.analysis.result", json!({"job":job})).unwrap();
            if value["finished"] == true {
                return value;
            }
            assert!(Instant::now() < until, "analysis did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn frames(s: &mut Session, item: ItemId, extra: Value) -> Value {
        let mut params = json!({"item":item.0,"max_frames":1});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let job = s.execute("media.analysis.frames", params).unwrap()["job"].as_u64().unwrap();
        done(s, job)
    }
    #[test]
    fn middle_bytes_change_invalidates_cache_despite_restored_mtime() {
        let (session, item, host) = analysis_image_session();
        let mut source = source::resolve(&session, item).unwrap();
        let dir = crate::temp_dir().join(format!(
            "filmcraft-analysis-version-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.png");
        let mut bytes = host.bytes.lock().unwrap().clone();
        bytes.resize(3 * 1024 * 1024, 0);
        std::fs::write(&path, &bytes).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        source.services = Arc::new(crate::FsServices);
        source.identity = source::SourceIdentity::File { path: path.to_str().unwrap().into() };
        let cancel = AtomicBool::new(false);
        let first = super::super::cache::source_version(&source, &cancel).unwrap();
        bytes[1024 * 1024] = 37;
        std::fs::write(&path, &bytes).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        let second = super::super::cache::source_version(&source, &cancel).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with("sha256:"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn version_mismatch_rejects_request_without_returning_images() {
        let (mut s, item, _) = analysis_image_session();
        let page = frames(&mut s, item, json!({"expected_version":"sha256:obsolete"}));
        assert_eq!(page["status"], "error");
        assert_eq!(page["error"]["code"], "VERSION_MISMATCH");
        assert!(page.get("frames").is_none());
    }
    #[test]
    fn identical_content_reuses_cache_and_clear_expires_old_jobs() {
        let (mut s, item, host) = analysis_image_session();
        host.bytes.lock().unwrap().resize(3 * 1024 * 1024, 0);
        let first = frames(&mut s, item, json!({}));
        let second = frames(&mut s, item, json!({}));
        assert_eq!(first["cache_hit"], false);
        assert_eq!(second["cache_hit"], true);
        assert_eq!(first["source_version"], second["source_version"]);
        assert_eq!(first["frames"][0]["frame_id"], second["frames"][0]["frame_id"]);
        assert_ne!(first["job"], second["job"]);
        println!("3MiB source in memory: first={}, cached={}", first["timings_ms"], second["timings_ms"]);
        let cleared = s.execute("media.analysis.clearCache", json!({})).unwrap();
        assert_eq!(cleared["removed"], 1);
        assert!(matches!(s.execute("media.analysis.result",json!({"job":first["job"]})),Err(crate::EngineError::Analysis(ref e)) if e.code=="RESULT_EXPIRED"));
    }
    #[test]
    fn retry_receipts_reuse_jobs_and_reject_changed_parameters() {
        let (mut s, item, _) = analysis_image_session();
        let params = json!({"item":item.0,"max_frames":1,"request_id":"retry-1"});
        let first = s.execute("media.analysis.frames", params.clone()).unwrap();
        let second = s.execute("media.analysis.frames", params.clone()).unwrap();
        assert_eq!(first["job"], second["job"]);
        done(&mut s, first["job"].as_u64().unwrap());
        assert!(s.execute("media.analysis.frames", json!({"item":item.0,"max_frames":2,"request_id":"retry-1"})).is_err());
        s.execute("media.analysis.clearCache", json!({})).unwrap();
        assert!(matches!(s.execute("media.analysis.frames",params),Err(crate::EngineError::Analysis(ref e)) if e.code=="RESULT_EXPIRED"));
    }
    #[test]
    fn replacement_between_first_hash_and_open_discards_output() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct ChangingHost {
            first: Vec<u8>,
            later: Vec<u8>,
            calls: AtomicUsize,
        }
        impl Services for ChangingHost {
            fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
                panic!("unbounded read")
            }
            fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
                panic!("write")
            }
            fn reader(&self, _: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
                let bytes = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 { &self.first } else { &self.later };
                Some(Ok(Arc::new(filmcraft_media::reader::MemReader(bytes.clone().into()))))
            }
        }
        let (mut s, item, host) = analysis_image_session();
        let mut later = std::io::Cursor::new(Vec::new());
        image::write_buffer_with_format(&mut later, &[0, 0, 255, 255].repeat(8 * 4), 8, 4, image::ExtendedColorType::Rgba8, image::ImageFormat::Png).unwrap();
        s.services = Arc::new(ChangingHost { first: host.bytes.lock().unwrap().clone(), later: later.into_inner(), calls: AtomicUsize::new(0) });
        let page = frames(&mut s, item, json!({}));
        assert_eq!(page["status"], "error");
        assert_eq!(page["error"]["code"], "SOURCE_CHANGED");
        assert!(!page.to_string().contains("png_base64"));
    }
    #[test]
    fn cache_hit_skips_source_open_but_still_rehashes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counted(Arc<AnalysisHost>, AtomicUsize);
        impl Services for Counted {
            fn read_file(&self, _: &str) -> std::io::Result<Vec<u8>> {
                panic!("unbounded read")
            }
            fn write_file(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
                panic!("write")
            }
            fn reader(&self, path: &str) -> Option<std::io::Result<filmcraft_media::SharedReader>> {
                self.1.fetch_add(1, Ordering::SeqCst);
                self.0.reader(path)
            }
        }
        let (mut s, item, host) = analysis_image_session();
        let counted = Arc::new(Counted(host, AtomicUsize::new(0)));
        s.services = counted.clone();
        assert_eq!(frames(&mut s, item, json!({}))["cache_hit"], false);
        assert_eq!(counted.1.load(Ordering::SeqCst), 3, "hash/open/hash");
        assert_eq!(frames(&mut s, item, json!({}))["cache_hit"], true);
        assert_eq!(counted.1.load(Ordering::SeqCst), 5, "cache hit must hash twice without decoding the source again");
    }
}

#[test]
fn analysis_media_mutations_leave_session_queryable_and_saveable() {
    use std::time::Duration;
    let dir = std::env::temp_dir().join(format!("filmcraft-analysis-mutations-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("source.png");
    let mut png = std::io::Cursor::new(Vec::new());
    image::write_buffer_with_format(&mut png, &[255; 8 * 4 * 4], 8, 4, image::ExtendedColorType::Rgba8, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();
    std::fs::write(&path, &png).unwrap();
    let mut session = Session::default();
    let item = crate::commands::import_bytes(&mut session, path.to_str().unwrap(), png.clone().into(), None).unwrap();
    let original = serde_json::to_value(&*session.project).unwrap();
    let history = session.history.undo.len();
    let mut mutations = Vec::new();
    for length in (0..png.len()).step_by(7) {
        mutations.push(png[..length].to_vec());
    }
    for offset in (0..png.len()).step_by(5) {
        let mut changed = png.clone();
        changed[offset] ^= 0x80;
        mutations.push(changed);
    }
    let mut huge = png.clone();
    huge[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
    huge[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut crc = !0u32;
    for &byte in &huge[12..29] {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb88320 } else { crc >> 1 };
        }
    }
    huge[29..33].copy_from_slice(&(!crc).to_be_bytes());
    mutations.push(huge);
    for bytes in mutations {
        std::fs::write(&path, &bytes).unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let ack = session.execute("media.analysis.frames", json!({"item":item.0,"max_frames":1})).unwrap();
            let started = web_time::Instant::now();
            loop {
                let page = session.execute("media.analysis.result", json!({"job":ack["job"]})).unwrap();
                if page["finished"] == true {
                    break page;
                }
                assert!(started.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(1));
            }
        }));
        let page = outcome.expect("hostile media must not panic");
        assert_ne!(page["error"]["code"], "WORKER_PANIC");
        assert!(session.execute("project.inspect", json!({})).is_ok());
    }
    std::fs::write(&path, &png).unwrap();
    let ack = session.execute("media.analysis.frames", json!({"item":item.0,"max_frames":1})).unwrap();
    let started = web_time::Instant::now();
    loop {
        let page = session.execute("media.analysis.result", json!({"job":ack["job"]})).unwrap();
        if page["finished"] == true {
            assert_eq!(page["status"], "complete");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(serde_json::to_value(&*session.project).unwrap(), original);
    assert_eq!(session.history.undo.len(), history);
    let saved = dir.join("still-usable.fcproj");
    session.execute("file.saveAs", json!({"path":saved})).unwrap();
    assert!(saved.exists());
    std::fs::remove_dir_all(dir).unwrap();
}
