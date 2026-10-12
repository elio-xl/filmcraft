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
        let params = if command == "edit.plan.result" { json!({"job":job,"session_id":s.edit_plans.session_id()}) } else { json!({"job":job}) };
        let page = s.execute(command, params).unwrap();
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
    json!({"session_id":s.edit_plans.session_id(),"request_id":"test-plan","expected_revision":s.revision.to_string(),
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

#[test]
fn plan_commits_one_undo_step() {
    let mut s = demo();
    let mut p = plan(&mut s);
    p["steps"][0]["params"]["delta"] = json!((-TPS / 25).to_string());
    p["steps"] = json!(vec![p["steps"][0].clone(); 3]);
    let original = serde_json::to_value(&*s.project).unwrap();
    let undo = s.history.undo.len();
    let rev = s.revision;
    let events = s.events.len();
    let result = run(&mut s, "edit.plan.apply", p);
    assert_eq!(result["committed"], true, "{result}");
    assert_eq!(s.history.undo.len(), undo + 1);
    assert_eq!(s.revision, rev + 1);
    assert_eq!(s.events[events..].iter().filter(|e| matches!(e, crate::Event::ProjectChanged { .. })).count(), 1);
    let edited = serde_json::to_value(&*s.project).unwrap();
    assert_ne!(edited, original);
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(serde_json::to_value(&*s.project).unwrap(), original);
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(serde_json::to_value(&*s.project).unwrap(), edited);
}
#[test]
fn plan_failure_leaves_live_state_unchanged() {
    let mut s = demo();
    let mut p = plan(&mut s);
    let before = snapshot(&s);
    let mut bad = p["steps"][0].clone();
    bad["params"]["clip"] = json!(u64::MAX);
    p["steps"] = json!([p["steps"][0].clone(), bad]);
    let result = run(&mut s, "edit.plan.apply", p);
    assert_eq!(result["status"], "error");
    assert!(result["error"]["message"].as_str().unwrap().contains("step 1"));
    assert_eq!(snapshot(&s), before);
}
fn ready(s: &Session, job: &Value) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let page = s.edit_plans.result(job.as_u64().unwrap()).unwrap();
        if page["status"] == "ready" {
            break;
        }
        assert_ne!(page["finished"], true, "{page}");
        assert!(Instant::now() < deadline, "{page}");
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn concurrent_user_edit_wins() {
    let mut s = demo();
    let p = plan(&mut s);
    assert_eq!(run(&mut s, "edit.plan.validate", p.clone())["status"], "complete");
    let mut p = p;
    p["request_id"] = json!("apply-after-validate");
    let ack = s.execute("edit.plan.apply", p).unwrap();
    ready(&s, &ack["job"]);
    s.execute("captions.add", json!({"text":"user edit"})).unwrap();
    let before = snapshot(&s);
    let result = wait(&mut s, "edit.plan.result", &ack["job"]);
    assert_eq!(result["error"]["code"], "REVISION_MISMATCH", "{result}");
    assert_eq!(snapshot(&s), before);
}
#[test]
fn cancelled_ready_plan_does_not_commit() {
    let mut s = demo();
    let p = plan(&mut s);
    let before = snapshot(&s);
    let ack = s.execute("edit.plan.apply", p).unwrap();
    ready(&s, &ack["job"]);
    s.execute("jobs.cancel", json!({"job":ack["job"]})).unwrap();
    assert_eq!(wait(&mut s, "edit.plan.result", &ack["job"])["status"], "cancelled");
    assert_eq!(snapshot(&s), before);
}
#[test]
fn changed_project_reference_discards_ready_plan() {
    let mut s = demo();
    let p = plan(&mut s);
    let ack = s.execute("edit.plan.apply", p).unwrap();
    ready(&s, &ack["job"]);
    s.project = std::sync::Arc::new((*s.project).clone());
    let before = snapshot(&s);
    let result = wait(&mut s, "edit.plan.result", &ack["job"]);
    assert_eq!(result["error"]["code"], "PROJECT_CHANGED", "{result}");
    assert_eq!(snapshot(&s), before);
}
#[test]
fn source_replaced_before_apply_is_rejected() {
    let mut s = demo();
    s.services = std::sync::Arc::new(crate::FsServices);
    let clip = s.active_sequence().unwrap().video_tracks[0].items[0].clone();
    let (root, _, _) = s.project.resolve_media(clip.item).unwrap();
    let dir = crate::temp_dir().join(format!("filmcraft-plan-source-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("source.png");
    let make = |color: &[u8]| {
        let mut out = std::io::Cursor::new(Vec::new());
        image::write_buffer_with_format(&mut out, &color.repeat(32), 8, 4, image::ExtendedColorType::Rgba8, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    std::fs::write(&path, make(&[255, 0, 0, 255])).unwrap();
    if let filmcraft_project::ItemKind::Media(m) = &mut std::sync::Arc::make_mut(&mut s.project).items.get_mut(&root).unwrap().kind {
        m.media = filmcraft_project::MediaRef::File { path: path.to_str().unwrap().into() };
        m.info.kind = filmcraft_media::MediaKind::Still;
    } else {
        panic!("demo media missing");
    }
    let p = plan(&mut s);
    let before = snapshot(&s);
    let ack = s.execute("edit.plan.apply", p).unwrap();
    ready(&s, &ack["job"]);
    let replacement = dir.join("replacement.png");
    std::fs::write(&replacement, make(&[0, 255, 0, 255])).unwrap();
    std::fs::rename(replacement, &path).unwrap();
    let result = wait(&mut s, "edit.plan.result", &ack["job"]);
    assert_eq!(result["error"]["code"], "SOURCE_CHANGED", "{result}");
    assert_eq!(snapshot(&s), before);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn plan_graphic_edits_keep_shared_instances_synchronized() {
    let mut s = demo();
    s.execute("graphics.newText", json!({"text":"original","newClip":true,"time":600*TPS})).unwrap();
    let clip = s.state.selection[0];
    let item = s.execute("graphics.upgradeToSourceGraphic", json!({"clip":clip.0})).unwrap()["item"].as_u64().unwrap();
    let track = s.active_sequence().unwrap().find_item(clip).unwrap().0;
    s.execute("timeline.move", json!({"moves":[{"clip":clip.0,"track":track.0,"time":610*TPS}],"copy":true})).unwrap();
    let p = json!({"session_id":s.edit_plans.session_id(),"request_id":"graphic-plan","expected_revision":s.revision.to_string(),"source_versions":[],"steps":[{"id":"graphics.set","params":{"clip":clip.0,"layer":0,"time":(600*TPS).to_string(),"props":{"text":"shared replacement"}},"reason":"shared title"}]});
    let result = run(&mut s, "edit.plan.apply", p);
    assert_eq!(result["committed"], true, "{result}");
    let layers = &s.project.source_graphics[&filmcraft_project::ItemId(item)].layers;
    let copies = s.active_sequence().unwrap().all_tracks().flat_map(|t| &t.items).filter(|c| c.item.0 == item).collect::<Vec<_>>();
    assert_eq!(copies.len(), 2);
    for c in copies {
        assert_eq!(&c.effects.iter().filter(|e| filmcraft_project::graphic::is_layer(e)).cloned().collect::<Vec<_>>(), layers);
    }
    assert!(serde_json::to_string(layers).unwrap().contains("shared replacement"));
}

#[test]
fn isolated_step_panic_never_reaches_live_state() {
    let mut s = demo();
    let mut p = plan(&mut s);
    p["steps"] = json!(vec![p["steps"][0].clone(); 3]);
    let before = snapshot(&s);
    s.edit_plans.inject_panic(1);
    let result = run(&mut s, "edit.plan.apply", p);
    assert_eq!(result["error"]["code"], "WORKER_PANIC", "{result}");
    assert_eq!(snapshot(&s), before);
}
#[test]
fn apply_preserves_new_playhead_and_rejects_overflow() {
    let mut s = demo();
    let p = plan(&mut s);
    let ack = s.execute("edit.plan.apply", p).unwrap();
    ready(&s, &ack["job"]);
    s.execute("playhead.set", json!({"seconds":123})).unwrap();
    let playhead = s.playhead();
    let result = wait(&mut s, "edit.plan.result", &ack["job"]);
    assert_eq!(result["committed"], true, "{result}");
    assert_eq!(s.playhead(), playhead);
    s.revision = u64::MAX;
    let mut p = plan(&mut s);
    p["request_id"] = json!("overflow");
    let before = snapshot(&s);
    assert_eq!(run(&mut s, "edit.plan.apply", p)["error"]["code"], "RESOURCE_LIMIT");
    assert_eq!(snapshot(&s), before);
}
#[test]
fn committed_plan_uses_existing_recovery_snapshot() {
    let dir = crate::temp_dir().join(format!("filmcraft-plan-recovery-{}", std::process::id()));
    let mut s = demo();
    let mut cfg = crate::autosave::AutosaveConfig::new(&dir);
    cfg.journal_debounce = Duration::from_millis(10);
    s.start_autosave(cfg.clone()).unwrap();
    let p = plan(&mut s);
    assert_eq!(run(&mut s, "edit.plan.apply", p)["committed"], true);
    let edited = (*s.project).clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        s.poll_persistence();
        if s.persistence.as_ref().unwrap().status.journaled_revision == s.revision {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    s.persistence.take().unwrap().simulate_crash();
    let mut recovered = Session::default();
    recovered.start_autosave(cfg).unwrap();
    recovered.execute("file.recover", json!({})).unwrap();
    assert_eq!(*recovered.project, edited);
    recovered.shutdown();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lost_reply_does_not_apply_twice() {
    let mut s = demo();
    let p = plan(&mut s);
    let ack = s.execute("edit.plan.apply", p.clone()).unwrap();
    assert_eq!(s.execute("edit.plan.apply", p.clone()).unwrap()["job"], ack["job"]);
    let original = wait(&mut s, "edit.plan.result", &ack["job"]);
    assert_eq!(original["committed"], true);
    // Expire all ordinary result pages. A commit receipt must survive.
    for index in 0..40 {
        let mut other = plan(&mut s);
        other["request_id"] = json!(format!("read-only-{index}"));
        assert_eq!(run(&mut s, "edit.plan.validate", other)["status"], "complete");
    }
    let before = snapshot(&s);
    let retry = s.execute("edit.plan.apply", p).unwrap();
    assert_eq!(retry["job"], ack["job"]);
    assert_eq!(wait(&mut s, "edit.plan.result", &retry["job"]), original);
    assert_eq!(snapshot(&s), before);
}
#[test]
fn request_id_conflict_is_rejected() {
    let mut s = demo();
    let mut p = plan(&mut s);
    assert_eq!(run(&mut s, "edit.plan.apply", p.clone())["committed"], true);
    let before = snapshot(&s);
    p["steps"][0]["reason"] = json!("same step count, different full parameters");
    assert!(s.execute("edit.plan.apply", p).unwrap_err().to_string().contains("REQUEST_ID_CONFLICT"));
    assert_eq!(snapshot(&s), before);
}
#[test]
fn old_session_request_is_explicitly_expired() {
    let mut s = demo();
    let p = plan(&mut s);
    let id = s.execute("edit.plan.session", json!({})).unwrap()["session_id"].clone();
    let mut restarted = demo();
    let before = snapshot(&restarted);
    let mut old = p;
    old["session_id"] = id.clone();
    assert!(restarted.execute("edit.plan.apply", old).unwrap_err().to_string().contains("SESSION_EXPIRED"));
    assert!(restarted.execute("edit.plan.result", json!({"job":1,"session_id":id})).unwrap_err().to_string().contains("SESSION_EXPIRED"));
    assert_eq!(snapshot(&restarted), before);
}
#[test]
fn dedup_capacity_does_not_forget_commits() {
    let mut s = demo();
    let caption = s.execute("captions.add", json!({"text":"original"})).unwrap()["caption"].clone();
    let session = s.execute("edit.plan.session", json!({})).unwrap()["session_id"].clone();
    let mut first = Value::Null;
    let mut original = Value::Null;
    for index in 0..1024 {
        let p = json!({"session_id":session,"request_id":format!("committed-{index}"),"expected_revision":s.revision.to_string(),"source_versions":[],"steps":[{"id":"captions.setText","params":{"caption":caption,"text":format!("caption {index}")},"reason":"capacity"}]});
        let page = run(&mut s, "edit.plan.apply", p.clone());
        assert_eq!(page["committed"], true, "{index}: {page}");
        if index == 0 {
            first = p;
            original = page;
        }
    }
    let before = snapshot(&s);
    let mut extra = first.clone();
    extra["request_id"] = json!("one-too-many");
    extra["expected_revision"] = json!(s.revision.to_string());
    let page = run(&mut s, "edit.plan.apply", extra);
    assert_eq!(page["error"]["code"], "DEDUP_CAPACITY", "{page}");
    assert_eq!(snapshot(&s), before);
    let retry = s.execute("edit.plan.apply", first).unwrap();
    assert_eq!(wait(&mut s, "edit.plan.result", &retry["job"]), original);
    assert_eq!(snapshot(&s), before);
}
#[test]
fn committed_retry_survives_new_interactive_trim() {
    let mut s = demo();
    let p = plan(&mut s);
    let original = run(&mut s, "edit.plan.apply", p.clone());
    assert_eq!(original["committed"], true);
    crate::trim::play_around(&mut s, &json!({"clock":0})).unwrap();
    assert!(s.trim_play.active());
    let before = snapshot(&s);
    let ack = s.execute("edit.plan.apply", p).unwrap();
    assert_eq!(wait(&mut s, "edit.plan.result", &ack["job"]), original);
    assert_eq!(snapshot(&s), before);
    assert!(s.trim_play.active());
}
