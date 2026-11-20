//! Record sync with a vfm-server (`sync1` / `lease1`, see
//! docs/design/server-source-of-truth.md). The interface hands over plain
//! records `{c, key, value}`; here they're sealed with keys derived from your app
//! key (the server sees neither the key nor the content), sent, and what comes
//! back is opened again. The sync position and the lease token per server stay
//! on this side.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use vfm_share::records::{self, LeaseRequest, LeaseResponse, Record, RecordKeys, SyncRequest, SyncResponse};
use vfm_share::{auth, Identity};

use super::{normalize_url, now, ShareState};

/// A change from the interface.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecChange {
    pub c: String,
    pub key: String,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub deleted: bool,
    /// For your own data (newest wins); empty for VRChat and game-log data.
    #[serde(default)]
    pub hlc: String,
    pub observed_at: i64,
}

/// A record for the interface, opened.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecOut {
    pub c: String,
    pub key: String,
    pub value: Value,
    pub deleted: bool,
    pub source: String,
    pub observed_at: i64,
    pub hlc: String,
    pub seq: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecSyncResult {
    pub changes: Vec<RecOut>,
    /// Why some of the sent changes weren't taken (with `c` and the key as sent).
    pub rejected: Vec<RecRejected>,
    /// The server lost track of deletions this device missed: everything was
    /// fetched again; drop local copies of records not in `changes`.
    pub reset: bool,
    pub write_max: BTreeMap<String, u32>,
    pub seq: u64,
    /// Records that couldn't be opened (another key, or damaged).
    pub unreadable: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecRejected {
    pub c: String,
    pub key: String,
    pub why: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Cursors {
    /// "url|user" -> highest sequence this device has.
    #[serde(default)]
    since: BTreeMap<String, u64>,
}

/// At most this many pages per sync call (2000 records each); the rest comes next time.
const MAX_PAGES: usize = 200;

fn slot(url: &str, user: &str) -> String {
    format!("{url}|{user}")
}

impl ShareState {
    /// Take, renew or give up this account's lease on a server. The token stays here.
    pub async fn records_lease(&self, raw_url: &str, user: &str, device: &str, priority: u8, release: bool) -> Result<LeaseResponse, String> {
        let url = normalize_url(raw_url)?;
        let id = self.identity(user).await?;
        let key = slot(&url, user);
        let token = self.leases.lock().unwrap().get(&key).cloned();
        let req = LeaseRequest { device: device.into(), action: if release { "release" } else { "take" }.into(), priority, token };
        let body = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        let v = self.send(&url, reqwest::Method::POST, "/v1/lease", Some(body), Some((&id, user))).await?;
        let mut res: LeaseResponse = serde_json::from_value(v).map_err(|e| e.to_string())?;
        let mut leases = self.leases.lock().unwrap();
        match res.token.take() {
            Some(t) if res.held => {
                leases.insert(key, t);
            }
            _ => {
                leases.remove(&key);
            }
        }
        Ok(res)
    }

    /// Send `changes` and fetch everything this device hasn't seen (all pages).
    /// `full`: start from scratch (a new device, or after dropping local copies).
    pub async fn records_sync(
        &self,
        raw_url: &str,
        user: &str,
        device: &str,
        full: bool,
        reads: BTreeMap<String, u32>,
        changes: Vec<RecChange>,
    ) -> Result<RecSyncResult, String> {
        let url = normalize_url(raw_url)?;
        if !records::valid_source(device) {
            return Err("bad device ID".into());
        }
        let id = self.identity(user).await?;
        let keys = RecordKeys::from_identity(&id);
        let key = slot(&url, user);
        let mut cursors: Cursors = self.read("records.json");
        let mut since = if full { 0 } else { cursors.since.get(&key).copied().unwrap_or(0) };
        let mut full = full || since == 0;
        // The real keys of what we send, to report rejections by them.
        let mut sent_keys: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut out_changes = Vec::with_capacity(changes.len());
        for ch in changes {
            if !records::valid_collection(&ch.c) {
                return Err(format!("bad collection {:?}", ch.c));
            }
            let kid = keys.key_id(&ch.c, &ch.key);
            let schema = records::collection(&ch.c).map(|d| d.schema).unwrap_or(1);
            let payload = if ch.deleted {
                Vec::new()
            } else {
                let plain = records::to_cbor(&records::Content { k: ch.key.clone(), v: ch.value });
                keys.seal(&ch.c, &kid, schema, &plain)
            };
            sent_keys.insert((ch.c.clone(), kid.clone()), ch.key);
            out_changes.push(Record {
                c: ch.c,
                k: kid,
                v: schema,
                source: device.into(),
                observed_at: ch.observed_at,
                hlc: ch.hlc,
                deleted: ch.deleted,
                payload,
                seq: 0,
            });
        }
        let lease = self.leases.lock().unwrap().get(&key).cloned();
        let mut result = RecSyncResult::default();
        let mut pending = Some(out_changes);
        for _ in 0..MAX_PAGES {
            let req = SyncRequest {
                since,
                device: device.into(),
                full,
                lease: lease.clone(),
                reads: reads.clone(),
                changes: pending.take().unwrap_or_default(),
            };
            let res = self.post_sync(&url, &id, user, &req).await?;
            for r in res.rejected {
                let real = sent_keys.get(&(r.c.clone(), r.k.clone())).cloned().unwrap_or(r.k);
                result.rejected.push(RecRejected { c: r.c, key: real, why: r.why });
            }
            result.write_max = res.write_max;
            if res.reset {
                // Deletions we missed are gone from the server: start over.
                result.reset = true;
                result.changes.clear();
                since = 0;
                full = true;
                continue;
            }
            for rec in res.changes {
                match open(&keys, rec) {
                    Some(o) => result.changes.push(o),
                    None => result.unreadable += 1,
                }
            }
            since = res.seq;
            if !res.more {
                break;
            }
        }
        result.seq = since;
        cursors.since.insert(key, since);
        let _g = self.lock.lock().await;
        self.write("records.json", &cursors)?;
        Ok(result)
    }

    /// Forget the sync position (and lease) for a server, e.g. when it's removed.
    pub async fn records_forget(&self, raw_url: &str, user: &str) -> Result<(), String> {
        let url = normalize_url(raw_url)?;
        let key = slot(&url, user);
        self.leases.lock().unwrap().remove(&key);
        let _g = self.lock.lock().await;
        let mut cursors: Cursors = self.read("records.json");
        cursors.since.remove(&key);
        self.write("records.json", &cursors)
    }

    async fn post_sync(&self, url: &str, id: &Identity, user: &str, req: &SyncRequest) -> Result<SyncResponse, String> {
        let body = records::encode(req);
        let path = "/v1/sync";
        let res = self
            .http
            .post(format!("{url}{path}"))
            .header("authorization", auth::sign_request(id, user, "POST", path, &body, now()))
            .header("content-type", records::CONTENT_TYPE)
            .header("accept", records::CONTENT_TYPE)
            .body(body)
            .send()
            .await
            .map_err(|e| if e.is_connect() || e.is_timeout() { "can't reach the server".to_string() } else { e.to_string() })?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("this server can't sync records yet (update vfm-server)".into());
        }
        let bytes = res.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            let msg = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| format!("server answered {status}"));
            return Err(msg);
        }
        records::decode(&bytes, 256 << 20)
    }
}

fn open(keys: &RecordKeys, rec: Record) -> Option<RecOut> {
    let (key, value) = if rec.deleted {
        // Tombstones carry no content: the interface finds the record by its key id.
        (format!("#{}", rec.k), Value::Null)
    } else {
        let plain = keys.open(&rec.c, &rec.k, rec.v, &rec.payload)?;
        let content: records::Content<Value> = records::from_cbor(&plain).ok()?;
        // The sealed content names its own key; it must match the key id it was stored under.
        if keys.key_id(&rec.c, &content.k) != rec.k {
            return None;
        }
        (content.k, content.v)
    };
    Some(RecOut { c: rec.c, key, value, deleted: rec.deleted, source: rec.source, observed_at: rec.observed_at, hlc: rec.hlc, seq: rec.seq })
}

/// The key id the server knows a record by, for matching tombstones (`#<id>`).
impl ShareState {
    pub async fn records_key_ids(&self, user: &str, items: Vec<(String, String)>) -> Result<Vec<String>, String> {
        let keys = RecordKeys::from_identity(&self.identity(user).await?);
        Ok(items.into_iter().map(|(c, k)| format!("#{}", keys.key_id(&c, &k))).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_checks_the_inner_key() {
        let id = Identity::generate();
        let keys = RecordKeys::from_identity(&id);
        let kid = keys.key_id("friends", "usr_a");
        let plain = records::to_cbor(&records::Content { k: "usr_a".to_string(), v: serde_json::json!({ "n": 1 }) });
        let rec = Record { c: "friends".into(), k: kid.clone(), v: 1, source: "pc".into(), observed_at: 5, hlc: String::new(), deleted: false, payload: keys.seal("friends", &kid, 1, &plain), seq: 7 };
        let o = open(&keys, rec.clone()).unwrap();
        assert_eq!(o.key, "usr_a");
        assert_eq!(o.value["n"], 1);
        assert_eq!(o.seq, 7);
        // Content sealed for one key, stored under another key id: refused.
        let other = keys.key_id("friends", "usr_b");
        let moved = Record { k: other.clone(), payload: keys.seal("friends", &other, 1, &plain), ..rec.clone() };
        assert!(open(&keys, moved).is_none());
        // Tombstones come back as "#<key id>".
        let gone = Record { deleted: true, payload: vec![], ..rec };
        assert_eq!(open(&keys, gone).unwrap().key, format!("#{kid}"));
    }
}
