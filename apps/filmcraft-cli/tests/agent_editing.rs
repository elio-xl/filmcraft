//! Deterministic end-to-end agent editing, through the real stdio MCP binary.
use base64::Engine as _;
use filmcraft_media::{FrameRequest, MediaSource};
use filmcraft_time::{FrameRate, TICKS_PER_SECOND as TPS, Tick};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

struct Mcp {
    process: Child,
    input: ChildStdin,
    output: Receiver<Value>,
    next: u64,
    transcript: Vec<Value>,
}
impl Mcp {
    fn start(extra: &[&str]) -> Self {
        let mut process = Command::new(env!("CARGO_BIN_EXE_filmcraft-cli"))
            .arg("mcp")
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = process.stdin.take().unwrap();
        let stdout = process.stdout.take().unwrap();
        let (tx, output) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut client = Self { process, input, output, next: 1, transcript: Vec::new() };
        let reply = client
            .rpc("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"filmcraft-agent-acceptance","version":"1"}}));
        assert_eq!(reply["result"]["serverInfo"]["name"], "filmcraft");
        writeln!(client.input, "{}", json!({"jsonrpc":"2.0","method":"notifications/initialized"})).unwrap();
        client
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.transcript.push(request.clone());
        writeln!(self.input, "{request}").unwrap();
        self.input.flush().unwrap();
        loop {
            let reply = self.output.recv_timeout(Duration::from_secs(300)).expect("MCP reply before timeout");
            if reply["id"] == id {
                self.transcript.push(reply.clone());
                return reply;
            }
        }
    }
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.rpc("tools/call", json!({"name":name,"arguments":arguments}));
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }
    fn good(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self.call(name, arguments);
        assert_ne!(reply["isError"], true, "{reply}");
        reply
    }
    fn analysis(&mut self, id: &str, params: Value) -> Value {
        let ack = self.run(id, params);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let page = metadata(&self.good("media_analysis_result", json!({"job":ack["job"]})));
            if page["finished"] == true {
                return page;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn run(&mut self, id: &str, params: Value) -> Value {
        metadata(&self.good("command_run", json!({"id":id,"params":params})))
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
fn metadata(reply: &Value) -> Value {
    reply.get("structuredContent").cloned().unwrap_or_else(|| serde_json::from_str(reply["content"][0]["text"].as_str().unwrap()).unwrap())
}
fn image(reply: &Value, index: usize) -> image::RgbaImage {
    let content = &reply["content"][index];
    assert_eq!(content["mimeType"], "image/png");
    image::load_from_memory(&base64::engine::general_purpose::STANDARD.decode(content["data"].as_str().unwrap()).unwrap()).unwrap().to_rgba8()
}
fn root(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/fixtures/agent-editing").join(format!("{name}-{}", std::process::id()))
}

fn generate_fixtures(dir: &Path) -> PathBuf {
    use filmcraft_isobmff::{Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
    use filmcraft_prores::{ChromaFormat, Encoder, Frame, Profile};
    std::fs::create_dir_all(dir).unwrap();
    let mut videos = Vec::new();
    for clip in 0..3usize {
        let path = dir.join(format!("source-{clip}.mov"));
        let mut writer = Mp4Writer::new(std::fs::File::create(&path).unwrap(), WriterOptions::new(Brand::Mov)).unwrap();
        let track = writer.add_track(TrackConfig::new(SampleEntry::prores(FourCc(*b"apco"), 1280, 720), 25)).unwrap();
        let mut encoder = Encoder::new(Profile::Proxy, 1280, 720);
        for number in 0..250usize {
            let scene = (clip + number / 100) % 3;
            let mut frame = Frame::new(1280, 720, ChromaFormat::Yuv422, 10, false);
            let (y, cb, cr) = [(250, 410, 950), (700, 170, 110), (127, 950, 470)][scene];
            frame.y.fill(y);
            frame.cb.fill(cb);
            frame.cr.fill(cr);
            let mut gray = |x: usize, y: usize, value: u16| {
                frame.y[y * 1280 + x] = value;
                frame.cb[y * 640 + x / 2] = 512;
                frame.cr[y * 640 + x / 2] = 512;
            };
            for y in 200..500usize {
                for x in 400..800usize {
                    let dx = x as i32 - 600;
                    let dy = y as i32 - 350;
                    let inside = match scene {
                        0 => dx * dx + dy * dy < 140 * 140,
                        1 => dy >= -140 && dx.abs() < (dy + 150) / 2,
                        _ => dx.abs() < 140 && dy.abs() < 140,
                    };
                    if inside {
                        gray(x, y, 940);
                    }
                }
            }
            // Original seven-segment digits: clip id then exact zero-based source frame number.
            const DIGITS: [u8; 10] = [0b1111110, 0b0110000, 0b1101101, 0b1111001, 0b0110011, 0b1011011, 0b1011111, 0b1110000, 0b1111111, 0b1111011];
            for (position, digit) in [clip + 1, number / 100, (number / 10) % 10, number % 10].into_iter().enumerate() {
                let left = 100 + position * 110;
                for (segment, (x, y, w, h)) in
                    [(12, 0, 60, 12), (72, 12, 12, 48), (72, 72, 12, 48), (12, 120, 60, 12), (0, 72, 12, 48), (0, 12, 12, 48), (12, 60, 60, 12)]
                        .into_iter()
                        .enumerate()
                {
                    if DIGITS[digit] & (1 << (6 - segment)) != 0 {
                        for yy in y..y + h {
                            for xx in x..x + w {
                                gray(left + xx, 30 + yy, 940);
                            }
                        }
                    }
                }
            }
            // Two-pixel detail intentionally finer than the analysis thumbnails can represent.
            for y in 520..600usize {
                for x in 1040..1200usize {
                    gray(x, y, if (x / 2 + y / 2) % 2 == 0 { 940 } else { 64 });
                }
            }
            let data = encoder.encode(&frame).unwrap();
            writer.write_sample(track, WriteSample { data: &data, duration: 1, composition_offset: 0, is_sync: true }).unwrap();
        }
        writer.finish().unwrap();
        videos.push(
            json!({"path":path,"width":1280,"height":720,"fps_num":25,"fps_den":1,"frames":250,"scene_order":(0..3).map(|n|(clip+n)%3).collect::<Vec<_>>() }),
        );
    }
    let image = dir.join("closing.png");
    std::fs::write(&image, filmcraft_automation::png_rgba(1280, 720, [255, 0, 255, 255].repeat(1280 * 720), 0).unwrap()).unwrap();
    let audio = dir.join("music.wav");
    let samples: Vec<f32> = (0..30 * 48_000).map(|i| 0.15 * (i as f32 * (440. + 20. * (i / 48_000) as f32) * std::f32::consts::TAU / 48_000.).sin()).collect();
    std::fs::write(&audio, filmcraft_media::wav::write_wav16(&samples, 1, 48_000)).unwrap();
    for name in ["source-0.mov", "source-1.mov", "source-2.mov", "closing.png", "music.wav"] {
        std::fs::write(dir.join(format!("{name}.attribution")),format!("asset: {name}\ntitle: Agent editing deterministic fixture\nauthor: FilmCraft contributors\nsource: original work\nlicense: MIT OR Apache-2.0\nlicense-file: ../../../../LICENSE-MIT\nadded: 2026-10-12 by FilmCraft contributors\nnotes: Generated by apps/filmcraft-cli/tests/agent_editing.rs; no external assets.\n")).unwrap();
    }
    let manifest = dir.join("manifest.json");
    std::fs::write(&manifest,serde_json::to_vec_pretty(&json!({"license":"MIT OR Apache-2.0","source":"original procedural test artwork; no external assets","videos":videos,"image":image,"audio":audio,"labels":"clip id, zero-based frame number","scenes":["red circle","green triangle","blue square"]})).unwrap()).unwrap();
    manifest
}

#[test]
fn agent_editing_round_trip() {
    let dir = root("roundtrip");
    let manifest = generate_fixtures(&dir);
    let fixtures: Value = serde_json::from_slice(&std::fs::read(manifest).expect("generated fixture manifest")).unwrap();
    let bridge = std::env::var("FILMCRAFT_AGENT_TEST_BRIDGE").ok();
    let mut client = match &bridge {
        Some(address) => Mcp::start(&["--bridge", address]),
        None => Mcp::start(&[]),
    };
    client.run("file.newProject", json!({"name":"Agent acceptance"}));
    let paths: Vec<_> =
        fixtures["videos"].as_array().unwrap().iter().map(|v| v["path"].clone()).chain([fixtures["image"].clone(), fixtures["audio"].clone()]).collect();
    let imported = metadata(&client.good("media_import", json!({"paths":paths})));
    assert_eq!(imported["errors"], json!([]), "{imported}");
    let items = imported["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    let before = metadata(&client.good("project_inspect", json!({})));
    let mut analyses = Vec::new();
    for item in items.iter().take(4) {
        let inspected = metadata(&client.good("media_inspect", json!({"item":item})));
        assert_eq!(inspected["analysis_capabilities"]["frames"]["supported"], true);
        let started = Instant::now();
        let reply = client.good("media_frames", json!({"item":item,"max_side":512,"max_frames":3}));
        let first_ms = started.elapsed().as_secs_f64() * 1000.;
        let meta = metadata(&reply);
        assert_eq!(meta["status"], "complete");
        assert_eq!(meta["coordinate_space"], "source");
        for frame in meta["frames"].as_array().unwrap() {
            let pixels = image(&reply, frame["content_index"].as_u64().unwrap() as usize);
            assert!(pixels.width().max(pixels.height()) <= 512);
            assert!(frame["actual_ticks"].as_str().unwrap().parse::<i64>().is_ok());
        }
        let started = Instant::now();
        let hit = client.good("media_frames", json!({"item":item,"max_side":512,"max_frames":3}));
        let hit_ms = started.elapsed().as_secs_f64() * 1000.;
        assert_eq!(metadata(&hit)["cache_hit"], true);
        analyses.push(json!({"item":item,"first_ms":first_ms,"hit_ms":hit_ms,"response_bytes":serde_json::to_vec(&reply).unwrap().len(),"metadata":meta}));
    }
    assert_eq!(metadata(&client.good("project_inspect", json!({}))), before);
    client.run("file.newSequence", json!({"name":"Thirty seconds","width":1280,"height":720,"fps":25,"video":2,"audio":1}));
    let steps: Vec<_> = items
        .iter()
        .take(3)
        .enumerate()
        .map(|(index, item)| json!({"id":"timeline.place","params":{"item":item,"track":"V1","seconds":index*9,"sourceIn":TPS,"duration":9*TPS,"audio":false}}))
        .chain([
            json!({"id":"timeline.place","params":{"item":items[3],"track":"V1","seconds":27,"duration":3*TPS,"audio":false}}),
            json!({"id":"timeline.place","params":{"item":items[4],"track":"A1","seconds":0,"duration":30*TPS,"video":false}}),
            json!({"id":"graphics.newText","params":{"text":"Agent cut","newClip":true,"track":1,"time":0,"seconds":3,"position":[640,650],"size":60}}),
        ])
        .collect();
    let batch = metadata(&client.good("command_batch", json!({"steps":steps})));
    assert_eq!(batch["completed"], 6);
    let sequence = metadata(&client.good("sequence_inspect", json!({})));
    assert_eq!(sequence["duration"], 30 * TPS);
    assert_eq!(sequence["durationFrames"], 750);
    assert_eq!(sequence["settings"]["width"], 1280);
    assert_eq!(sequence["settings"]["height"], 720);
    let video = sequence["video"][0]["items"].as_array().unwrap();
    assert_eq!(video.len(), 4);
    for (index, clip) in video.iter().take(3).enumerate() {
        assert_eq!(clip["sourceIn"], TPS);
        assert_eq!(clip["sourceOut"], 10 * TPS);
        assert_eq!(clip["start"], index as i64 * 9 * TPS);
    }
    assert_eq!(sequence["audio"][0]["items"][0]["duration"], 30 * TPS);
    assert!(sequence["video"][1]["items"].to_string().contains("Agent cut"));
    client.run("playhead.set", json!({"seconds":5}));
    client.run("timeline.select", json!({"clips":[video[0]["clip"]]}));
    let state_before = metadata(&client.good("doc_inspect", json!({})));
    let history_before = client.run("history.list", json!({}));
    client.good("media_frames", json!({"item":items[0],"strategy":"timestamps","times_ticks":[(2*TPS).to_string()],"max_side":256}));
    assert_eq!(metadata(&client.good("doc_inspect", json!({}))), state_before, "analysis changed project, playhead or selection");
    assert_eq!(client.run("history.list", json!({})), history_before, "analysis changed undo/redo history");
    let preview = client.good("render_preview", json!({"seconds":1,"max_side":512}));
    let preview_image = image(&preview, 0);
    assert_eq!(preview_image.width(), 512);
    let title_pixels =
        preview_image.enumerate_pixels().filter(|(_, y, pixel)| (240..280).contains(y) && pixel.0[..3].iter().all(|&channel| channel > 230)).count();
    assert!(title_pixels > 80, "the title must be visible in the preview");
    let project = dir.join("edit.fcproj");
    client.run("file.saveAs", json!({"path":project}));
    let saved = std::fs::read(&project).unwrap();
    assert!(!saved.is_empty());
    // Reopen through the same public command in either backend; no UI state is injected.
    client.run("file.open", json!({"path":project}));
    let reopened = metadata(&client.good("sequence_inspect", json!({})));
    for field in ["settings", "video", "audio", "duration"] {
        assert_eq!(reopened[field], sequence[field], "reopened {field}");
    }
    let output = dir.join("edit.mov");
    client.run("file.exportMedia", json!({"path":output,"format":"prores","width":1280,"height":720,"audio":true,"range":"entire","wait":true}));
    let source =
        filmcraft_codecs::Mp4Source::open_reader("edit.mov", std::sync::Arc::new(filmcraft_media::reader::FileReader::open(&output).unwrap())).unwrap();
    let info = source.info();
    assert_eq!((info.video.as_ref().unwrap().width, info.video.as_ref().unwrap().height), (1280, 720));
    assert_eq!(info.duration, Tick(30 * TPS));
    assert!(!info.audio_streams.is_empty());
    for (seconds, source_index, source_frame) in [(4, 0, 125), (13, 1, 125), (22, 2, 125)] {
        let exported = source.analysis_frame(FrameRequest::full(Tick(seconds * TPS))).unwrap();
        assert_eq!(exported.pts, Tick(seconds * TPS));
        let original = filmcraft_codecs::Mp4Source::open_reader(
            "source.mov",
            std::sync::Arc::new(filmcraft_media::reader::FileReader::open(Path::new(fixtures["videos"][source_index]["path"].as_str().unwrap())).unwrap()),
        )
        .unwrap();
        let expected = original.analysis_frame(FrameRequest::full(FrameRate::FPS_25.tick_of(source_frame))).unwrap();
        let (a, b) = (exported.to_rgba8(), expected.to_rgba8());
        let mut sum = 0u64;
        let mut count = 0u64;
        for y in 80..600 {
            for x in 80..1200 {
                let index = (y * 1280 + x) * 4;
                for channel in 0..3 {
                    sum += u64::from(a[index + channel].abs_diff(b[index + channel]));
                    count += 1;
                }
            }
        }
        assert!((sum as f64 / count as f64) < 8., "export differs from full-resolution source at {seconds}s");
        // A two-pixel checker survives original-source export; a 512px thumbnail cannot preserve it.
        let luma = |x: usize| -> u16 {
            let i = (560 * 1280 + x) * 4;
            a[i..i + 3].iter().map(|v| u16::from(*v)).sum::<u16>() / 3
        };
        assert!(luma(1100).abs_diff(luma(1102)) > 80, "full-resolution detail was lost");
    }
    let ending = source.analysis_frame(FrameRequest::full(Tick(29 * TPS))).unwrap().to_rgba8();
    assert!(ending[0] > 180 && ending[2] > 180, "closing PNG is magenta");
    let audio = source.audio(48_000, 4800, 48_000).unwrap();
    assert!(audio.channels.iter().flatten().any(|sample| sample.abs() > 0.01));
    let crossings = audio.channels[0].windows(2).filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0).count();
    assert!((45..=47).contains(&crossings), "source second1 carries460Hz: {crossings} crossings in100ms");
    std::fs::write(dir.join("analysis-measurements.json"), serde_json::to_vec_pretty(&analyses).unwrap()).unwrap();
    std::fs::write(dir.join("mcp-transcript.json"), serde_json::to_vec_pretty(&client.transcript).unwrap()).unwrap();
    println!("acceptance artifacts: {}", dir.display());
}

#[test]
fn failed_batch_can_be_recovered() {
    let mut client = Mcp::start(&[]);
    client.run("file.newSequence", json!({"name":"Recovery","fps":25}));
    let before = metadata(&client.good("project_inspect", json!({})));
    let result=client.call("command_batch",json!({"steps":[{"id":"file.newBin","params":{"name":"First retained"}},{"id":"no.such.command"},{"id":"file.newBin","params":{"name":"Never created"}}]}));
    assert_eq!(result["isError"], true);
    let outcome = metadata(&result);
    assert_eq!(outcome["completed"], 1);
    assert_eq!(outcome["failed"], 1);
    assert_eq!(outcome["results"].as_array().unwrap().len(), 2);
    let after = metadata(&client.good("project_inspect", json!({})));
    assert!(after.to_string().contains("First retained"));
    assert!(!after.to_string().contains("Never created"));
    client.run("edit.undo", json!({}));
    let recovered = metadata(&client.good("project_inspect", json!({})));
    assert_eq!(recovered["root"], before["root"]);
    assert!(!recovered.to_string().contains("First retained"));
}

#[test]
fn source_frames_map_fractional_vfr_reuse_speed_and_reverse() {
    use filmcraft_isobmff::{Brand, FourCc, Mp4Writer, SampleEntry, TrackConfig, WriteSample, WriterOptions};
    use filmcraft_prores::{ChromaFormat, Encoder, Frame, Profile};
    let encoded = Encoder::new(Profile::Proxy, 64, 64).encode(&Frame::new(64, 64, ChromaFormat::Yuv422, 10, false)).unwrap();
    for (timescale, durations, queries, expected) in [
        (
            30000,
            [1001, 1001, 1001],
            vec![Tick(0), FrameRate::FPS_29_97.tick_of(1), FrameRate::FPS_29_97.tick_of(2) - Tick(1)],
            vec![Tick(0), FrameRate::FPS_29_97.tick_of(1), FrameRate::FPS_29_97.tick_of(1)],
        ),
        (1000, [40, 80, 40], vec![Tick(0), Tick(TPS / 10), Tick(12 * TPS / 100)], vec![Tick(0), Tick(4 * TPS / 100), Tick(12 * TPS / 100)]),
    ] {
        let mut writer = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mov)).unwrap();
        let track = writer.add_track(TrackConfig::new(SampleEntry::prores(FourCc(*b"apco"), 64, 64), timescale)).unwrap();
        for duration in durations {
            writer.write_sample(track, WriteSample { data: &encoded, duration, composition_offset: 0, is_sync: true }).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let source = filmcraft_codecs::Mp4Source::open("mapping.mov", bytes.into()).unwrap();
        for (query, expected) in queries.into_iter().zip(expected) {
            assert_eq!(source.analysis_frame(FrameRequest::full(query)).unwrap().pts, expected);
        }
    }
    let mut session = filmcraft_engine::Session::default();
    session.execute("file.openDemoProject", json!({})).unwrap();
    let mut clip = session.active_sequence().unwrap().video_tracks[0].items[0].clone();
    let rate = FrameRate::FPS_29_97;
    clip.start = rate.tick_of(100);
    clip.source_in = rate.tick_of(10);
    clip.duration = rate.tick_of(20);
    clip.speed = 2.;
    clip.reverse = false;
    let forward = clip.source_time_at(clip.start + rate.tick_of(5));
    assert_eq!(forward, rate.tick_of(20));
    let mut reuse = clip.clone();
    reuse.start = rate.tick_of(500);
    assert_eq!(reuse.source_time_at(reuse.start + rate.tick_of(5)), forward);
    clip.reverse = true;
    assert_eq!(clip.source_time_at(clip.start), rate.tick_of(50) - Tick(1));
    assert_eq!(clip.source_time_at(clip.start + rate.tick_of(5)), rate.tick_of(40) - Tick(1));
    clip.reverse = false;
    clip.speed = 0.5;
    assert_eq!(clip.source_time_at(clip.start + rate.tick_of(6)), rate.tick_of(13));
}

#[test]
fn source_preview_video_and_png_fallback_round_trip() {
    let dir = root("preview");
    let manifest = generate_fixtures(&dir);
    let fixtures: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let bridge = std::env::var("FILMCRAFT_AGENT_TEST_BRIDGE").ok();
    let mut client = match &bridge {
        Some(address) => Mcp::start(&["--bridge", address]),
        None => Mcp::start(&[]),
    };
    client.run("file.newProject", json!({"name":"Source preview acceptance"}));
    let imported = metadata(&client.good("media_import", json!({"paths":[fixtures["videos"][0]["path"]]})));
    let item = imported["items"][0].as_u64().unwrap();
    let absent = metadata(&client.good("media_transcript", json!({"item":item})));
    assert_eq!(absent["availability"], "unavailable");
    client.run("transcript.set", json!({"item":item,"transcript":{"source":"manual","language":"en","words":[{"text":"circle","start":TPS,"end":3*TPS}]}}));
    let sound = metadata(&client.good("media_import", json!({"paths":[fixtures["audio"]]})))["items"][0].as_u64().unwrap();
    let before = (client.good("doc_inspect", json!({})), client.run("history.list", json!({})));
    let words = metadata(&client.good("media_transcript", json!({"item":item,"start_ticks":(2*TPS).to_string(),"end_ticks":(4*TPS).to_string()})));
    assert_eq!(words["words"][0]["text"], "circle");
    assert_eq!(words["words"][0]["start_ticks"], TPS.to_string());
    assert_eq!(words["words"][0]["overlap"]["start_ticks"], (2 * TPS).to_string());
    let audio_job = client.run("media.analysis.audio", json!({"item":sound,"start_ticks":"0","end_ticks":(2*TPS).to_string()}));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let audio = metadata(&client.good("media_analysis_result", json!({"job":audio_job["job"]})));
        if audio["finished"] == true {
            assert_eq!(audio["loudness"]["availability"], "available");
            assert!(audio["silence_ranges"].as_array().unwrap().is_empty());
            assert_eq!(audio["rhythm"]["availability"], "unavailable");
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = Instant::now();
    let fallback = client.good("media_preview_clip", json!({"item":item,"start_ticks":(2*TPS).to_string(),"end_ticks":(6*TPS).to_string(),"max_side":160}));
    let meta = metadata(&fallback);
    assert_eq!(meta["delivery"], "frames");
    let frames = meta["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 8);
    for frame in frames {
        let picture = image(&fallback, frame["content_index"].as_u64().unwrap() as usize);
        assert_eq!(picture.dimensions(), (160, 90));
        let time = frame["actual_ticks"].as_str().unwrap().parse::<i64>().unwrap();
        assert!((2 * TPS..6 * TPS).contains(&time));
    }
    let png_ms = started.elapsed().as_millis();
    let claim=client.analysis("media.analysis.summary.set",json!({"item":item,"source_version":meta["source_version"],"start_ticks":(2*TPS).to_string(),"end_ticks":(4*TPS).to_string(),"summary":"red circle with source frame labels","tags":["red","circle"],"evidence_frame_ids":[frames[0]["frame_id"]],"model_label":"acceptance client"}));
    assert_eq!(claim["summary"]["origin"], "model");
    let search = client.analysis("media.analysis.summary.search", json!({"query":"RED circle"}));
    assert_eq!(search["results"].as_array().unwrap().len(), 1);
    assert_eq!(search["results"][0]["evidence_frame_ids"][0], frames[0]["frame_id"]);
    assert_eq!(search["results"][0]["model_label"], "acceptance client");

    let shots =
        metadata(&client.good(
            "media_shots",
            json!({"item":item,"expected_version":meta["source_version"],"start_ticks":(2*TPS).to_string(),"end_ticks":(6*TPS).to_string()}),
        ));
    assert_eq!(shots["candidate_only"], true);
    let candidates = shots["shots"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["start_ticks"], (2 * TPS).to_string());
    assert_eq!(candidates[0]["end_ticks"], (4 * TPS).to_string());
    assert_eq!(candidates[1]["end_ticks"], (6 * TPS).to_string());

    let started = Instant::now();
    let movie = client.good("media_preview_clip", json!({"item":item,"expected_version":meta["source_version"],"start_ticks":(2*TPS).to_string(),"end_ticks":(6*TPS).to_string(),"max_side":160,"audio":false,"video_capability":"verified"}));
    let video = metadata(&movie);
    let uri = video["preview"]["resource_uri"].clone();
    let resource = client.rpc("resources/read", json!({"uri":uri}));
    let data = base64::engine::general_purpose::STANDARD.decode(resource["result"]["contents"][0]["blob"].as_str().unwrap()).unwrap();
    assert!(data.len() <= 24 * 1024 * 1024);
    let source = filmcraft_codecs::Mp4Source::open("preview.mp4", data.clone().into()).unwrap();
    assert_eq!(source.info().frame_rate(), FrameRate::new(12, 1));
    assert_eq!(source.info().duration, Tick(4 * TPS));
    for (time, green) in [(TPS / 2, false), (5 * TPS / 2, true)] {
        let frame = source.video_frame(FrameRequest::full(Tick(time))).unwrap();
        let rgba = frame.to_rgba8();
        assert_eq!(rgba[1] > rgba[0], green, "video source offset or speed changed");
    }
    assert_eq!((client.good("doc_inspect", json!({})), client.run("history.list", json!({}))), before);
    std::fs::write(dir.join("preview.mp4"), &data).unwrap();
    std::fs::write(dir.join("preview.mp4.attribution"), "asset: preview.mp4\ntitle: Source preview acceptance\nauthor: FilmCraft contributors\nsource: original work\nlicense: MIT OR Apache-2.0\nadded: 2026-10-12 by FilmCraft contributors\nnotes: Generated from procedural FilmCraft test footage.\n").unwrap();
    std::fs::write(dir.join("preview-measurements.json"), serde_json::to_vec_pretty(&json!({"png_wall_ms":png_ms,"video_wall_ms":started.elapsed().as_millis(),"video_bytes":data.len(),"fallback_frames":frames.len(),"client":"Rust stdio acceptance client","model_video_consumption":"not tested"})).unwrap()).unwrap();
    std::fs::write(dir.join("mcp-transcript.json"), serde_json::to_vec(&client.transcript).unwrap()).unwrap();
    println!("P1 preview artifacts: {}", dir.display());
}
