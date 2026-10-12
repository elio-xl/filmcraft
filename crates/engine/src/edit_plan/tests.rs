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
    assert_eq!(result["steps"][0]["affected_clip_count"], 2, "{result}");
    assert_eq!(result["diff"]["entity_count"], 2, "{result}");
    for c in copies {
        assert_eq!(&c.effects.iter().filter(|e| filmcraft_project::graphic::is_layer(e)).cloned().collect::<Vec<_>>(), layers);
    }
    assert!(serde_json::to_string(layers).unwrap().contains("shared replacement"));
}

fn report_fixture() -> Session {
    use filmcraft_time::Tick;
    let mut s = demo();
    let mut project = (*s.project).clone();
    let ids = [project.alloc_id(), project.alloc_id(), project.alloc_id()];
    let q = project.sequence_mut(s.state.active_sequence.unwrap()).unwrap();
    let template = q.video_tracks[0].items[0].clone();
    for track in q.all_tracks_mut() {
        track.items.clear();
        track.transitions.clear();
    }
    q.markers.clear();
    q.caption_tracks.clear();
    for (index, id) in ids.into_iter().enumerate() {
        let mut clip = template.clone();
        clip.id = filmcraft_project::ClipId(id);
        clip.start = Tick(index as i64 * 20 * TPS);
        clip.duration = Tick(10 * TPS);
        clip.source_in = Tick::ZERO;
        clip.speed = 1.0;
        clip.link = None;
        q.video_tracks[0].items.push(clip);
    }
    s.project = std::sync::Arc::new(project);
    s
}
fn candidate_report(s: &Session, steps: Value) -> super::validation::Candidate {
    let p = json!({"session_id":s.edit_plans.session_id(),"request_id":"report","expected_revision":s.revision.to_string(),"source_versions":[],"steps":steps});
    let parsed = super::validation::parse(&p).unwrap();
    let verified = s.project.items.keys().filter_map(|id| s.project.resolve_media(*id).map(|(root, m, _)| (root, m.info.duration))).collect();
    super::validation::compute(&super::validation::Snapshot { project: s.project.clone(), state: s.state.clone(), revision: s.revision }, &parsed, &verified)
        .unwrap()
}
fn reported_entity<'a>(diff: &'a Value, kind: &str, id: u64) -> &'a Value {
    diff["entities"].as_array().unwrap().iter().find(|v| v["kind"] == kind && v["id"] == id).unwrap_or_else(|| panic!("missing {kind} {id} in {diff}"))
}
#[test]
fn edit_plan_report_shows_overwrite_split_and_copy() {
    let s = report_fixture();
    let q = s.active_sequence().unwrap();
    let track = &q.video_tracks[0];
    let a = track.items[0].id.0;
    let b = track.items[1].id.0;
    let before = snapshot(&s);
    for copy in [false, true] {
        let candidate = candidate_report(
            &s,
            json!([
                {"id":"timeline.trim","params":{"clip":a,"edge":"out","delta":(-6*TPS).to_string()},"reason":"four seconds"},
                {"id":"timeline.move","params":{"moves":[{"clip":a,"track":track.id.0,"time":(23*TPS).to_string()}],"copy":copy},"reason":"overwrites middle of B"}
            ]),
        );
        let diff = &candidate.report["steps"][1]["diff"];
        let b_change = reported_entity(diff, "clip", b);
        assert_eq!(b_change["before"]["end"], (30 * TPS).to_string());
        assert_eq!(b_change["after"]["end"], (23 * TPS).to_string());
        assert_eq!(diff["entities"].as_array().unwrap().iter().filter(|v| v["change"] == "created").count(), if copy { 2 } else { 1 });
        assert_eq!(snapshot(&s), before);
    }
    let candidate = candidate_report(
        &s,
        json!([{"id":"timeline.move","params":{"moves":[{"clip":a,"track":track.id.0,"time":(20*TPS).to_string()}]},"reason":"replace all of B"}]),
    );
    let removed = reported_entity(&candidate.report["diff"], "clip", b);
    assert_eq!(removed["change"], "deleted");
    assert!(removed["after"].is_null());
}
#[test]
fn edit_plan_report_shows_ripple_caption_and_clamped_boundary() {
    let mut s = report_fixture();
    let caption = s.execute("captions.add", json!({"time":45*TPS,"durationSeconds":1,"text":"before"})).unwrap()["caption"].as_u64().unwrap();
    let q = s.active_sequence().unwrap();
    let a = q.video_tracks[0].items[0].id.0;
    let b = q.video_tracks[0].items[1].id.0;
    let track = q.video_tracks[0].id.0;
    let caption_start = q.find_caption(filmcraft_project::ClipId(caption)).unwrap().1.start.0;
    let candidate = candidate_report(
        &s,
        json!([
            {"id":"timeline.trim","params":{"clip":a,"edge":"out","mode":"ripple","delta":(-TPS).to_string()},"reason":"ripple"},
            {"id":"timeline.move","params":{"moves":[{"clip":a,"track":track,"time":(20*TPS).to_string()}],"insert":true,"copy":true},"reason":"insert shifts captions"},
            {"id":"captions.setText","params":{"caption":caption,"text":"after"},"reason":"caption"}
        ]),
    );
    let diff = &candidate.report["steps"][0]["diff"];
    assert_eq!(reported_entity(diff, "clip", b)["after"]["start"], (19 * TPS).to_string());
    assert_eq!(reported_entity(&candidate.report["steps"][1]["diff"], "caption", caption)["after"]["start"], (caption_start + 9 * TPS).to_string());
    let caption_change = reported_entity(&candidate.report["steps"][2]["diff"], "caption", caption);
    assert_eq!(caption_change["before"]["text"], "before");
    assert_eq!(caption_change["after"]["text"], "after");
    assert_eq!(candidate.report["steps"][2]["affected_clip_count"], 1);
    // Place the neighbor before the demo source's 18-second handle limit so this exercises
    // timeline clamping, independently of the source-duration clamp.
    let mut project = (*s.project).clone();
    project.sequence_mut(s.state.active_sequence.unwrap()).unwrap().video_tracks[0].items[1].start = filmcraft_time::Tick(15 * TPS);
    s.project = std::sync::Arc::new(project);
    let candidate =
        candidate_report(&s, json!([{"id":"timeline.trim","params":{"clip":a,"edge":"out","delta":(30*TPS).to_string()},"reason":"clamped to neighbor"}]));
    let changed = reported_entity(&candidate.report["diff"], "clip", a);
    assert_eq!(changed["after"]["end"], (15 * TPS).to_string());
    assert_eq!(changed["before"]["end"], (10 * TPS).to_string());
}
#[test]
fn edit_plan_report_bounds_rows_and_unicode_text() {
    let mut s = report_fixture();
    let mut project = (*s.project).clone();
    let ids = (0..600).map(|_| project.alloc_id()).collect::<Vec<_>>();
    let q = project.sequence_mut(s.state.active_sequence.unwrap()).unwrap();
    let template = q.video_tracks[0].items[0].clone();
    q.video_tracks[0].items.clear();
    for (index, id) in ids.into_iter().enumerate() {
        let mut c = template.clone();
        c.id = filmcraft_project::ClipId(id);
        c.start = filmcraft_time::Tick(index as i64 * 20 * TPS);
        q.video_tracks[0].items.push(c);
    }
    s.project = std::sync::Arc::new(project);
    let caption = s.execute("captions.add", json!({"time":13000*TPS,"text":"字".repeat(2000)})).unwrap()["caption"].clone();
    let a = s.active_sequence().unwrap().video_tracks[0].items[0].id.0;
    let candidate = candidate_report(
        &s,
        json!([
            {"id":"timeline.trim","params":{"clip":a,"edge":"out","mode":"ripple","delta":(-TPS).to_string()},"reason":"many collateral edits"},
            {"id":"captions.setText","params":{"caption":caption,"text":"新".repeat(2000)},"reason":"large Unicode caption"}
        ]),
    );
    let diff = &candidate.report["diff"];
    let shown = diff["entities"].as_array().unwrap().len();
    assert!(shown <= 256);
    assert_eq!(diff["entity_count"], 601);
    assert_eq!(diff["omitted_entity_count"], 601 - shown);
    assert!(serde_json::to_vec(&candidate.report).unwrap().len() <= 1024 * 1024);
    let caption_only = candidate_report(&s, json!([{"id":"captions.setText","params":{"caption":caption,"text":"新".repeat(2000)},"reason":"text bound"}]));
    let entity = &caption_only.report["diff"]["entities"][0];
    assert_eq!(entity["after"]["text_truncated"], true);
    assert!(entity["after"]["text"].as_str().unwrap().chars().count() <= 256);
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
