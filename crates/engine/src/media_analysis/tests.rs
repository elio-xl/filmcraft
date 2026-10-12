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
