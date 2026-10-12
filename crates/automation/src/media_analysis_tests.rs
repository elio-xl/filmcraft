//! MCP source analysis: actual image blocks, strict schemas, and retry-safe Bridge calls.
use super::*;
use crate::media_analysis::to_tool_result;
use std::time::Duration;

fn png() -> Vec<u8> {
    crate::png_rgba(8, 4, [255, 0, 0, 127].repeat(32), 512).unwrap()
}
fn page() -> Value {
    json!({"schema_version":1,"status":"complete","finished":true,"item":1,"source_version":"sha256:test","coordinate_space":"source",
        "frames":[{"frame_id":"frame:one","request_indices":[0,1],"requested_ticks":["0","100"],"actual_ticks":"0","width":8,"height":4,"mime_type":"image/png","png_base64":base64::engine::general_purpose::STANDARD.encode(png())}],"errors":[]})
}
#[test]
fn mcp_analysis_returns_images_and_metadata() {
    let result = to_tool_result(page()).unwrap();
    assert_ne!(result.is_error, Some(true));
    let metadata = result.structured_content.as_ref().unwrap();
    let index = metadata["frames"][0]["content_index"].as_u64().unwrap() as usize;
    let image = result.content[index].as_image().unwrap();
    assert_eq!(image.mime_type, "image/png");
    let data = base64::engine::general_purpose::STANDARD.decode(&image.data).unwrap();
    let decoded = image::load_from_memory(&data).unwrap().to_rgba8();
    assert_eq!(decoded.dimensions(), (8, 4));
    assert_eq!(decoded.get_pixel(0, 0).0[3], 127);
    let text = result.content[0].as_text().unwrap();
    assert!(!text.text.contains("png_base64") && !text.text.contains(&image.data));
    assert_eq!(serde_json::from_str::<Value>(&text.text).unwrap(), *metadata);
    assert_eq!(metadata["frames"][0]["requested_ticks"], json!(["0", "100"]));
    let mut partial = page();
    partial["status"] = json!("partial");
    partial["errors"] = json!([{"request_index":2,"code":"DECODE_FAILED"}]);
    assert_eq!(to_tool_result(partial).unwrap().structured_content.unwrap()["status"], "partial");
    let mut empty = page();
    empty["frames"] = json!([]);
    assert_eq!(to_tool_result(empty).unwrap().is_error, Some(true));
}
#[test]
fn media_analysis_tool_schemas_are_read_only_and_strict() {
    let mcp = FilmcraftMcp::headless(Session::default());
    for name in ["media_inspect", "media_frames", "media_preview_clip", "media_shots", "media_transcript", "media_analysis_result"] {
        let tool = serde_json::to_value(mcp.tool_router.get(name).expect("tool registered")).unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert!(mcp.unknown_arg(name, json!({"unknown":true}).as_object()).is_some());
    }
}
pub(super) struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
pub(super) fn session() -> (Session, u64, Fixture) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path =
        std::env::temp_dir().join(format!("filmcraft-mcp-analysis-{}-{}.png", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let bytes = png();
    std::fs::write(&path, &bytes).unwrap();
    let mut session = Session::default();
    let item = filmcraft_engine::commands::import_bytes(&mut session, path.to_str().unwrap(), bytes.into(), None).unwrap();
    (session, item.0, Fixture(path))
}
async fn fake_app(session: Session, drop_first: bool) -> (String, Arc<Mutex<Session>>, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let session = Arc::new(Mutex::new(session));
    let shared = session.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let task = tokio::spawn(async move {
        let mut dropped = false;
        while let Ok((socket, _)) = listener.accept().await {
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                log.lock().unwrap().push(request.clone());
                let command = request["params"]["command"].as_str().unwrap();
                let result = shared.lock().unwrap().execute(command, request["params"]["params"].clone());
                if drop_first && !dropped && command == "media.analysis.frames" {
                    assert!(result.is_ok());
                    dropped = true;
                    break;
                }
                let reply = match result {
                    Ok(value) => json!({"id":request["id"],"ok":true,"result":value}),
                    Err(error) => json!({"id":request["id"],"ok":false,"error":error.to_string()}),
                };
                if write.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });
    (address, session, seen, task)
}
#[tokio::test]
async fn bridge_retry_reuses_analysis_request() {
    let (session, item, _fixture) = session();
    let (address, shared, seen, task) = fake_app(session, true).await;
    let bridge = BridgeClient::new(address).unwrap();
    let reply =
        tokio::time::timeout(Duration::from_secs(5), bridge.execute("media.analysis.frames", json!({"item":item,"max_frames":1}))).await.unwrap().unwrap();
    assert!(reply["job"].as_u64().is_some());
    let starts: Vec<_> = seen.lock().unwrap().iter().filter(|r| r["params"]["command"] == "media.analysis.frames").cloned().collect();
    assert_eq!(starts.len(), 2);
    let request_id = &starts[0]["params"]["params"]["request_id"];
    assert!(request_id.as_str().is_some());
    assert_eq!(*request_id, starts[1]["params"]["params"]["request_id"]);
    assert_eq!(shared.lock().unwrap().execute("jobs.list", json!({})).unwrap().as_array().unwrap().len(), 1);
    assert!(bridge.execute("media.analysis.frames", json!({"item":item,"max_frames":2,"request_id":request_id})).await.is_err());
    task.abort();
}

#[test]
fn media_analysis_budget_keeps_whole_pngs() {
    let mut seed = 7u32;
    let bytes: Vec<u8> = (0..1024 * 1024 * 4)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect();
    let png = crate::png_rgba(1024, 1024, bytes, 1024).unwrap();
    let mut p = page();
    p["frames"][0]["png_base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&png));
    p["frames"][0]["width"] = json!(1024);
    p["frames"][0]["height"] = json!(1024);
    let mut second = p["frames"][0].clone();
    second["requested_ticks"] = json!(["200", "300"]);
    p["frames"].as_array_mut().unwrap().push(second);
    let result = to_tool_result(p).unwrap();
    assert!(serde_json::to_vec(&result).unwrap().len() <= crate::media_analysis::MAX_RESPONSE_BYTES);
    let meta = result.structured_content.unwrap();
    assert_eq!(meta["status"], "partial");
    assert_eq!(meta["frames"].as_array().unwrap().len(), 1);
    assert_eq!(meta["remaining_times_ticks"], json!(["200", "300"]));
    let image = result.content[1].as_image().unwrap();
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(&image.data).unwrap(), png);
}

#[tokio::test]
async fn bridge_analysis_does_not_seek() {
    let (session, item, _fixture) = session();
    let (address, shared, seen, task) = fake_app(session, false).await;
    let bridge = FilmcraftMcp::bridge(&address).unwrap();
    let before = shared.lock().unwrap().execute("project.inspect", json!({})).unwrap();
    let bridged = bridge.run_analysis("media.analysis.frames", json!({"item":item,"max_frames":2}), None).await.unwrap();
    // The identical imported path ensures identical full-content source versions.
    let (mut headless, _, _unused) = self::session();
    headless.project = shared.lock().unwrap().project.clone();
    let headless = FilmcraftMcp::headless(headless);
    let direct = headless.run_analysis("media.analysis.frames", json!({"item":item,"max_frames":2}), None).await.unwrap();
    let normalize = |mut value: Value| {
        for key in ["job", "request_id", "timings_ms", "cache_hit"] {
            value.as_object_mut().unwrap().remove(key);
        }
        value
    };
    assert_eq!(normalize(bridged.structured_content.unwrap()), normalize(direct.structured_content.unwrap()));
    assert_eq!(shared.lock().unwrap().execute("project.inspect", json!({})).unwrap(), before);
    assert!(
        seen.lock().unwrap().iter().all(|request| matches!(request["params"]["command"].as_str(), Some("media.analysis.frames" | "media.analysis.result")))
    );
    task.abort();
}

#[tokio::test]
async fn media_analysis_async_poll_and_strict_wait() {
    let (session, item, _fixture) = session();
    let mcp = FilmcraftMcp::headless(session);
    for bad in [Value::Null, json!(1), json!("true")] {
        assert!(mcp.run_analysis("media.analysis.frames", json!({"item":item,"wait":bad}), None).await.is_err());
    }
    assert!(mcp.run_analysis("media.analysis.frames", json!({"item":item,"max_side":null}), None).await.is_err());
    let ack = mcp.run_analysis("media.analysis.frames", json!({"item":item,"wait":false}), None).await.unwrap().structured_content.unwrap();
    let job = ack["job"].as_u64().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = mcp.media_analysis_result(Parameters(AnalysisResultParams { job })).await.unwrap();
            if result.structured_content.as_ref().unwrap()["finished"] == true {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(result.content.iter().any(|c| c.as_image().is_some()));
    assert!(mcp.run("project.inspect", json!({})).await.is_ok());
}

#[tokio::test]
async fn media_analysis_inspect_advertises_verified_frame_support() {
    let (session, item, _fixture) = session();
    let mcp = FilmcraftMcp::headless(session);
    let meta = mcp.run_analysis("media.analysis.inspect", json!({"item":item}), None).await.unwrap().structured_content.unwrap();
    assert_eq!(meta["analysis_capabilities"]["frames"]["supported"], true);
    assert_eq!(meta["analysis_capabilities"]["preview_video"]["supported"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn media_analysis_cancel_is_scoped_and_releases_bridge_lock() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_, _, fixture) = session();
    let original = std::fs::read(&fixture.0).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (r, mut w) = socket.into_split();
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: Value = serde_json::from_str(&line).unwrap();
            log.lock().unwrap().push(request.clone());
            let result = match request["params"]["command"].as_str().unwrap() {
                "media.analysis.frames" => json!({"job":701,"finished":false,"status":"queued"}),
                "media.analysis.result" => json!({"job":701,"finished":false,"status":"running"}),
                "project.inspect" => json!({"name":"unchanged"}),
                "jobs.cancel" => json!({"cancelled":true}),
                command => panic!("unexpected command {command}"),
            };
            w.write_all(format!("{}\n", json!({"id":request["id"],"ok":true,"result":result})).as_bytes()).await.unwrap();
        }
    });
    let (mut tx, input) = tokio::io::duplex(65536);
    let (output, rx) = tokio::io::duplex(65536);
    let server = tokio::spawn(FilmcraftMcp::bridge(&address).unwrap().serve_io(input, output));
    let mut lines = BufReader::new(rx).lines();
    tx.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n").await.unwrap();
    lines.next_line().await.unwrap().unwrap();
    tx.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"media_frames\",\"arguments\":{\"item\":1}}}\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if seen.lock().unwrap().iter().any(|r| r["params"]["command"] == "media.analysis.result") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tx.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"project_inspect\",\"arguments\":{}}}\n").await.unwrap();
    let reply: Value = serde_json::from_str(&tokio::time::timeout(Duration::from_secs(2), lines.next_line()).await.unwrap().unwrap().unwrap()).unwrap();
    assert_eq!(reply["id"], 2, "analysis wait must release the connection");
    tx.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":1}}\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if seen.lock().unwrap().iter().any(|r| r["params"]["command"] == "jobs.cancel") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let requests = seen.lock().unwrap();
    let cancelled: Vec<_> = requests.iter().filter(|r| r["params"]["command"] == "jobs.cancel").collect();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0]["params"]["params"], json!({"job":701}));
    assert_eq!(std::fs::read(&fixture.0).unwrap(), original);
    server.abort();
    app.abort();
}

#[tokio::test]
async fn media_analysis_oversized_bridge_reply_drops_connection_without_replay() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (r, mut w) = socket.into_split();
        let mut lines = BufReader::new(r).lines();
        let request: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["params"]["command"], "media.analysis.result");
        let _ = w.write_all(&vec![b' '; 16 * 1024 * 1024 + 1]).await;
        drop(w);
        drop(lines);
        let (socket, _) = listener.accept().await.unwrap();
        let (r, mut w) = socket.into_split();
        let mut lines = BufReader::new(r).lines();
        let request: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["params"]["command"], "project.inspect", "oversize response must not trigger automatic replay");
        w.write_all(format!("{}\n", json!({"id":request["id"],"ok":true,"result":{"ok":true}})).as_bytes()).await.unwrap();
    });
    let bridge = BridgeClient::new(addr).unwrap();
    assert!(matches!(bridge.execute("media.analysis.result", json!({"job":1})).await, Err(AutomationError::ResponseLimit)));
    assert_eq!(bridge.execute("project.inspect", json!({})).await.unwrap(), json!({"ok":true}));
    task.await.unwrap();
}
