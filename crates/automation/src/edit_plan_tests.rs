use crate::{BridgeClient, FilmcraftMcp};
use filmcraft_engine::Session;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
fn fixture() -> (Session, Value) {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let caption = s.execute("captions.add", json!({"text":"original"})).unwrap()["caption"].clone();
    let p = json!({"expected_revision":s.revision.to_string(),"source_versions":[],"steps":[{"id":"captions.setText","params":{"caption":caption,"text":"committed caption"},"reason":"retry test"}]});
    (s, p)
}
#[tokio::test]
async fn edit_plan_client_fills_identity_and_polls() {
    let (s, p) = fixture();
    let mcp = FilmcraftMcp::headless(s);
    let result = mcp.run("edit.plan.apply", p).await.unwrap();
    assert!(result["session_id"].is_string());
    assert!(result["request_id"].is_string());
    let page = loop {
        let page = mcp.run("edit.plan.result", json!({"job":result["job"],"session_id":result["session_id"]})).await.unwrap();
        if page["finished"] == true {
            break page;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    assert_eq!(page["committed"], true, "{page}");
}
#[tokio::test]
async fn edit_plan_bridge_reconnect_after_lost_commit_reply() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (s, p) = fixture();
    let before = s.revision;
    let undo = s.history.undo.len();
    let shared = Arc::new(Mutex::new(s));
    let server = shared.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let mut dropped = false;
        while let Ok((socket, _)) = listener.accept().await {
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let command = request["params"]["command"].as_str().unwrap();
                let params = request["params"]["params"].clone();
                if command == "edit.plan.apply" {
                    log.lock().unwrap().push(params.clone());
                }
                let result = server.lock().unwrap().execute(command, params);
                if command == "edit.plan.apply" && !dropped {
                    let ack = result.unwrap();
                    for _ in 0..1000 {
                        let page = server.lock().unwrap().execute("edit.plan.result", json!({"job":ack["job"],"session_id":ack["session_id"]})).unwrap();
                        if page["finished"] == true {
                            assert_eq!(page["committed"], true, "{page}");
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    dropped = true;
                    break;
                }
                let reply = match result {
                    Ok(v) => json!({"id":request["id"],"ok":true,"result":v}),
                    Err(e) => json!({"id":request["id"],"ok":false,"error":e.to_string()}),
                };
                if write.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });
    let bridge = BridgeClient::new(address).unwrap();
    let ack = tokio::time::timeout(Duration::from_secs(10), bridge.execute("edit.plan.apply", p)).await.unwrap().unwrap();
    let page = bridge.execute("edit.plan.result", json!({"job":ack["job"],"session_id":ack["session_id"]})).await.unwrap();
    assert_eq!(page["committed"], true, "{page}");
    let starts = seen.lock().unwrap();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0], starts[1]);
    drop(starts);
    let s = shared.lock().unwrap();
    assert_eq!(s.revision, before + 1);
    assert_eq!(s.history.undo.len(), undo + 1);
    drop(s);
    task.abort();
}
#[tokio::test]
async fn edit_plan_explicit_retry_never_gets_a_fresh_session() {
    let (s, mut p) = fixture();
    let old = s.edit_plans.session_id().to_owned();
    let (new, _) = fixture();
    let mcp = FilmcraftMcp::headless(new);
    p["request_id"] = json!("old-request");
    assert!(mcp.run("edit.plan.apply", p.clone()).await.unwrap_err().to_string().contains("original session_id"));
    p["session_id"] = json!(old);
    assert!(mcp.run("edit.plan.apply", p).await.unwrap_err().to_string().contains("SESSION_EXPIRED"));
}
