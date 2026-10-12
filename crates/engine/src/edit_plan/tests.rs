use crate::Session;
use filmcraft_time::TICKS_PER_SECOND as TPS;
use serde_json::{Value, json};
use std::time::Duration;
use web_time::Instant;

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s.state.linked_selection = false;
    s.state.selection.clear();
    s
}
fn snapshot(s: &Session) -> Value {
    json!({"project":&*s.project,"state":s.state,"revision":s.revision,
        "undo":s.history.undo.iter().map(|(label,p)|json!([label,&**p])).collect::<Vec<_>>(),
        "redo":s.history.redo.iter().map(|(label,p)|json!([label,&**p])).collect::<Vec<_>>(),
        "events":s.events,"journal":s.journal})
}
fn wait(s: &mut Session, command: &str, job: &Value) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let page = s.execute(command, json!({"job":job})).unwrap();
        if page["finished"] == true {
            return page;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn version(s: &mut Session, item: u64) -> String {
    let ack = s.execute("media.analysis.inspect", json!({"item":item})).unwrap();
    wait(s, "media.analysis.result", &ack["job"])["source_version"].as_str().unwrap().into()
}
fn plan(s: &mut Session) -> Value {
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].clone();
    let v = version(s, clip.item.0);
    json!({"request_id":"test-plan","expected_revision":s.revision.to_string(),
        "source_versions":[{"item":clip.item.0,"source_version":v}],
        "steps":[{"id":"timeline.trim","params":{"clip":clip.id.0,"edge":"out","mode":"regular","delta":(-TPS).to_string()},"reason":"shorten the first shot"}]})
}
fn run(s: &mut Session, id: &str, p: Value) -> Value {
    match s.execute(id, p) {
        Err(e) => json!({"status":"error","error":{"message":e.to_string()}}),
        Ok(ack) => wait(s, "edit.plan.result", &ack["job"]),
    }
}
#[test]
fn edit_plan_allowlist_is_exact() {
    for id in ["timeline.trim", "timeline.move", "edit.clear", "captions.setText", "graphics.set"] {
        assert!(super::validation::allowed_command(id), "{id}");
    }
    for id in ["timeline.place", "timeline.trim.extra", "graphics.newText", "edit.undo", "file.exportMedia", "command_batch"] {
        assert!(!super::validation::allowed_command(id), "{id}");
    }
}
#[test]
fn edit_plan_rejects_external_effects() {
    let mut s = demo();
    let mut p = plan(&mut s);
    let before = snapshot(&s);
    for id in ["file.exportMedia", "file.import", "transcript.downloadModel", "media.analysis.frames", "command_batch", "edit.plan.apply", "ui.click"] {
        p["steps"] = json!([{"id":id,"params":{},"reason":"must reject"}]);
        let page = run(&mut s, "edit.plan.validate", p.clone());
        assert_eq!(page["status"], "error", "{page}");
        assert!(page["error"]["message"].as_str().unwrap().contains("step 0"), "{page}");
        assert_eq!(snapshot(&s), before);
    }
}
#[test]
fn edit_plan_requires_explicit_targets() {
    let mut s = demo();
    let mut p = plan(&mut s);
    let clip = p["steps"][0]["params"]["clip"].clone();
    for (index, (id, params)) in [
        ("timeline.trim", json!({"edge":"out","delta":"-1"})),
        ("timeline.trim", json!({"clip":clip,"edge":"out","delta":"0"})),
        ("timeline.move", json!({"moves":[{"clip":clip,"time":"0"}]})),
        ("timeline.move", json!({"moves":[{"clip":clip,"track":1,"time":"-1"}]})),
        ("edit.clear", json!({})),
        ("captions.setText", json!({"text":"caption"})),
        ("graphics.set", json!({"clip":clip,"props":{"text":"title"}})),
        ("graphics.set", json!({"clip":clip,"layer":0,"time":"0","props":{"text":"title"},"merge":true})),
        ("timeline.trim", json!({"clip":u64::MAX,"edge":"out","delta":"-1"})),
    ]
    .into_iter()
    .enumerate()
    {
        p["request_id"] = json!(format!("explicit-{index}"));
        p["steps"] = json!([{"id":id,"params":params,"reason":"explicit targets"}]);
        let page = run(&mut s, "edit.plan.validate", p.clone());
        assert_eq!(page["status"], "error", "{page}");
        assert!(page["error"]["message"].as_str().unwrap().contains("step 0"), "{page}");
    }
}
#[test]
fn edit_plan_rejects_stale_sources_and_preserves_state() {
    let mut s = demo();
    let p = plan(&mut s);
    let before = snapshot(&s);
    let good = run(&mut s, "edit.plan.validate", p.clone());
    assert_eq!(good["status"], "complete", "{good}");
    assert_eq!(good["committed"], false);
    assert_eq!(good["steps"][0]["status"], "valid");
    assert_eq!(snapshot(&s), before);
    let mut stale = p.clone();
    stale["request_id"] = json!("stale-source");
    stale["source_versions"][0]["source_version"] = json!(format!("sha256:{}", "0".repeat(64)));
    let page = run(&mut s, "edit.plan.validate", stale);
    assert_eq!(page["status"], "error", "{page}");
    assert_eq!(page["error"]["code"], "VERSION_MISMATCH", "{page}");
    assert_eq!(snapshot(&s), before);
    let mut stale = p.clone();
    stale["request_id"] = json!("stale-revision");
    stale["expected_revision"] = json!("0");
    assert_eq!(run(&mut s, "edit.plan.validate", stale)["status"], "error");
    let mut big = p;
    big["request_id"] = json!("too-many");
    big["steps"] = json!(vec![big["steps"][0].clone(); 101]);
    assert_eq!(run(&mut s, "edit.plan.validate", big)["status"], "error");
}

#[test]
fn isolated_executors_match_legacy_commands() {
    for id in super::validation::ALLOWED {
        let mut s = demo();
        let mut p = plan(&mut s);
        let clip = p["steps"][0]["params"]["clip"].clone();
        let track = s.active_sequence().unwrap().video_tracks[0].id.0;
        let (params, legacy) = match *id {
            "timeline.trim" => (json!({"clip":clip,"edge":"out","delta":(-TPS).to_string()}), json!({"clip":clip,"edge":"out","delta":-TPS})),
            "timeline.move" => {
                (json!({"moves":[{"clip":clip,"track":track,"time":(50*TPS).to_string()}]}), json!({"moves":[{"clip":clip,"track":track,"time":50*TPS}]}))
            }
            "edit.clear" => {
                let v = json!({"clips":[clip]});
                (v.clone(), v)
            }
            "captions.setText" => {
                let c = s.execute("captions.add", json!({"text":"original","time":0})).unwrap()["caption"].clone();
                let v = json!({"caption":c,"text":"replacement","speaker":"speaker"});
                (v.clone(), v)
            }
            "graphics.set" => {
                s.execute("graphics.newText", json!({"text":"title","newClip":true,"time":600*TPS})).unwrap();
                let c = s.state.selection[0].0;
                (
                    json!({"clip":c,"layer":0,"time":(600*TPS).to_string(),"props":{"text":"replacement","size":48}}),
                    json!({"clip":c,"layer":0,"time":600*TPS,"props":{"text":"replacement","size":48}}),
                )
            }
            _ => unreachable!(),
        };
        p["expected_revision"] = json!(s.revision.to_string());
        p["steps"] = json!([{"id":id,"params":params,"reason":"equivalence"}]);
        let parsed = super::validation::parse(&p).unwrap();
        let verified = s.project.items.keys().filter_map(|id| s.project.resolve_media(*id).map(|(root, m, _)| (root, m.info.duration))).collect();
        let before = snapshot(&s);
        let candidate = super::validation::compute(
            &super::validation::Snapshot { project: s.project.clone(), state: s.state.clone(), revision: s.revision },
            &parsed,
            &verified,
        )
        .unwrap();
        assert_eq!(snapshot(&s), before);
        s.execute(id, legacy).unwrap();
        assert_eq!(serde_json::to_value(candidate.project).unwrap(), serde_json::to_value(&*s.project).unwrap(), "{id}");
    }
}
#[test]
fn hostile_plan_json_is_bounded() {
    let mut s = demo();
    let p = plan(&mut s);
    let before = snapshot(&s);
    for value in [json!("x".repeat(1024 * 1024 + 1)), json!(vec![Value::Null; 10001])] {
        let mut v = p.clone();
        v["steps"][0]["params"] = value;
        assert!(super::validation::parse(&v).is_err());
    }
    let mut deep = Value::Null;
    for _ in 0..40 {
        deep = json!([deep]);
    }
    let mut v = p.clone();
    v["steps"][0]["params"] = deep;
    assert!(super::validation::parse(&v).is_err());
    assert_eq!(snapshot(&s), before);
}
