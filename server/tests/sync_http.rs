//! End to end over HTTP: the real vfm-server binary, a registered account and two
//! of its devices syncing records through it.

use std::time::Duration;
use vfm_share::records::{self, LeaseRequest, LeaseResponse, Record, RecordKeys, SyncRequest, SyncResponse};
use vfm_share::{auth, Identity};

const USER: &str = "usr_11111111-2222-3333-4444-555555555555";

struct Server {
    child: std::process::Child,
    base: String,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("vfm-sync-http-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let port = free_port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_vfm-server"))
        .arg("run")
        .env("VFM_CONFIG", dir.join("vfm-server.toml"))
        .env("VFM_LISTEN", format!("127.0.0.1:{port}"))
        .env("VFM_REGISTRATION", "open")
        .env("VFM_LOGGER", "false")
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("start vfm-server");
    let base = format!("http://127.0.0.1:{port}");
    let server = Server { child, base, dir };
    for _ in 0..100 {
        if reqwest::get(format!("{}/v1/info", server.base)).await.is_ok() {
            return server;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("vfm-server didn't come up");
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

async fn signed(id: &Identity, method: reqwest::Method, base: &str, path: &str, ct: &str, body: Vec<u8>) -> reqwest::Response {
    let header = auth::sign_request(id, USER, method.as_str(), path, &body, now());
    reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .header("authorization", header)
        .header("content-type", ct)
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn sync(id: &Identity, base: &str, req: &SyncRequest) -> SyncResponse {
    let res = signed(id, reqwest::Method::POST, base, "/v1/sync", records::CONTENT_TYPE, records::encode(req)).await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap_or_default());
    assert_eq!(res.headers()["content-type"], records::CONTENT_TYPE);
    records::decode(&res.bytes().await.unwrap(), 64 << 20).unwrap()
}

#[tokio::test]
async fn two_devices_sync_through_the_server() {
    let server = start().await;
    let base = server.base.clone();

    let info: serde_json::Value = reqwest::get(format!("{base}/v1/info")).await.unwrap().json().await.unwrap();
    let features: Vec<String> = serde_json::from_value(info["features"].clone()).unwrap();
    assert!(features.contains(&"sync1".to_string()) && features.contains(&"lease1".to_string()));

    // One account; the phone has the same key (copied from the PC, as the app does).
    let pc = Identity::generate();
    let phone = Identity::from_secret_string(&pc.to_secret_string()).unwrap();
    let body = serde_json::to_vec(&serde_json::json!({ "identity": pc.public(USER) })).unwrap();
    let res = signed(&pc, reqwest::Method::POST, &base, "/v1/register", "application/json", body).await;
    assert!(res.status().is_success(), "register: {}", res.text().await.unwrap());

    // The phone listens for live changes.
    let header = auth::sign_request(&phone, USER, "GET", "/v1/events", b"", now());
    let mut events = reqwest::Client::new().get(format!("{base}/v1/events")).header("authorization", header).send().await.unwrap();
    assert_eq!(events.status(), 200);
    let hello = events.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&hello).contains("open"));

    // The PC takes the lease and uploads a friend, sealed.
    let lease_body = serde_json::to_vec(&LeaseRequest { device: "pc".into(), action: "take".into(), priority: 20, token: None }).unwrap();
    let res = signed(&pc, reqwest::Method::POST, &base, "/v1/lease", "application/json", lease_body).await;
    let lease: LeaseResponse = res.json().await.unwrap();
    assert!(lease.held);

    let keys = RecordKeys::from_identity(&pc);
    let kid = keys.key_id("friends", "usr_friend");
    let plain = br#"{"displayName":"EchoMoth"}"#;
    let record = Record {
        c: "friends".into(),
        k: kid.clone(),
        v: 1,
        source: "pc".into(),
        observed_at: now() * 1000,
        hlc: String::new(),
        deleted: false,
        payload: keys.seal("friends", &kid, 1, plain),
        seq: 0,
    };
    let up = sync(&pc, &base, &SyncRequest { since: 0, device: "pc".into(), lease: lease.token.clone(), changes: vec![record], ..Default::default() }).await;
    assert!(up.rejected.is_empty(), "{:?}", up.rejected);
    assert_eq!(up.seq, 1);

    // The phone hears about it (skipping keep-alive comments)…
    let ev = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let chunk = events.chunk().await.unwrap().unwrap();
            let text = String::from_utf8_lossy(&chunk).to_string();
            if text.contains("event:") {
                return text;
            }
        }
    })
    .await
    .expect("live event");
    assert!(ev.contains("event: sync") && ev.contains("\"seq\":1"), "{ev}");

    // …fetches it and can open it.
    let down = sync(&phone, &base, &SyncRequest { since: 0, device: "phone".into(), ..Default::default() }).await;
    assert_eq!(down.changes.len(), 1);
    let got = &down.changes[0];
    assert_eq!(got.source, "pc");
    let phone_keys = RecordKeys::from_identity(&phone);
    assert_eq!(phone_keys.open(&got.c, &got.k, got.v, &got.payload).as_deref(), Some(&plain[..]));

    // Without the lease the phone can't write VRChat data.
    let bad = Record { source: "phone".into(), ..got.clone() };
    let r = sync(&phone, &base, &SyncRequest { since: down.seq, device: "phone".into(), changes: vec![bad], ..Default::default() }).await;
    assert_eq!(r.rejected[0].why, "lease");

    // The server's database holds no plaintext and no user IDs.
    let db = std::fs::read(server.dir.join("vfm-data").join("sync.sqlite")).unwrap_or_default();
    let wal = std::fs::read(server.dir.join("vfm-data").join("sync.sqlite-wal")).unwrap_or_default();
    let all = [db, wal].concat();
    assert!(!all.is_empty());
    let has = |needle: &[u8]| all.windows(needle.len()).any(|w| w == needle);
    assert!(!has(b"EchoMoth") && !has(b"usr_friend"));
}
