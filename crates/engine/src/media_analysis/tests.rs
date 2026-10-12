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
