//! Record sync (protocol feature `sync1`): every piece of an account's data is a
//! record `(collection, key) → payload`, versioned on the server by one change
//! sequence per account. See docs/design/server-source-of-truth.md.
//!
//! * [`COLLECTIONS`] says which collections exist, who may write them and their
//!   current schema version.
//! * [`Hlc`] orders changes to the person's own data across devices.
//! * [`RecordKeys`] seals device-written records end to end and hides record
//!   keys from the server (an HMAC instead of a user ID).
//! * [`SyncRequest`] / [`SyncResponse`] are the bodies of `POST /v1/sync`,
//!   sent as CBOR compressed with zstd ([`encode`] / [`decode`]).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;

use crate::Identity;

/// Feature names in `/v1/info` `features`.
pub const FEATURE_SYNC: &str = "sync1";
pub const FEATURE_LEASE: &str = "lease1";

/// Media type of `/v1/sync` bodies: CBOR compressed with zstd. Its own type rather
/// than `Content-Encoding: zstd`, so proxies on the way leave the bytes alone.
pub const CONTENT_TYPE: &str = "application/vnd.vfm-sync";

/// Who may write a collection, which decides how the server checks writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    /// From VRChat's API: only the holder of the account's lease writes.
    Api,
    /// From a VRChat client's game log / OSC: each client writes only its own records.
    GameLog,
    /// The person's own data: any device; the newest change (by [`Hlc`]) wins per record.
    Yours,
    /// Append-only logs from the API producer (lease holder).
    Log,
}

pub struct CollectionDef {
    pub name: &'static str,
    pub class: Class,
    /// The schema version current apps write.
    pub schema: u32,
}

const fn c(name: &'static str, class: Class) -> CollectionDef {
    CollectionDef { name, class, schema: 1 }
}

/// Every collection this protocol version knows. A server accepts unknown
/// collections from newer apps as [`Class::Yours`] (stored, never interpreted).
pub const COLLECTIONS: &[CollectionDef] = &[
    // From VRChat's API (the lease holder; later the agent).
    c("me", Class::Api),
    c("friends", Class::Api),
    c("presence", Class::Api),
    c("profiles", Class::Api),
    c("mutuals", Class::Api),
    c("favorites", Class::Api),
    c("groups", Class::Api),
    c("worlds", Class::Api),
    c("others", Class::Api),
    c("moderations", Class::Api),
    c("userNotes", Class::Api),
    c("notifications", Class::Api),
    c("alts", Class::Api),
    // From VRChat clients' game logs.
    c("visits", Class::GameLog),
    c("live", Class::GameLog),
    // The person's own data.
    c("circles", Class::Yours),
    c("imports", Class::Yours),
    c("nicknames", Class::Yours),
    c("starred", Class::Yours),
    c("timezones", Class::Yours),
    c("worldNotes", Class::Yours),
    c("inviteTemplates", Class::Yours),
    c("blockReasons", Class::Yours),
    c("shareNotes", Class::Yours),
    c("sharedCircles", Class::Yours),
    c("dismissedGroups", Class::Yours),
    c("slotWatch", Class::Yours),
    c("pushTargets", Class::Yours),
    c("settings", Class::Yours),
    // Logs.
    c("friendLog", Class::Log),
    c("activity", Class::Log),
];

pub fn collection(name: &str) -> Option<&'static CollectionDef> {
    COLLECTIONS.iter().find(|d| d.name == name)
}

/// The class a server applies: known collections by definition, unknown ones as [`Class::Yours`].
pub fn class_of(name: &str) -> Class {
    collection(name).map(|d| d.class).unwrap_or(Class::Yours)
}

/// Collection names: short, ASCII, so they're safe as identifiers anywhere.
pub fn valid_collection(name: &str) -> bool {
    (1..=40).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Record keys as sent to the server: a [`RecordKeys::key_id`] (64 hex) or, for
/// unencrypted records, any short printable string.
pub fn valid_key(key: &str) -> bool {
    (1..=128).contains(&key.len()) && key.bytes().all(|b| (0x21..0x7f).contains(&b))
}

/// Device IDs (`source`): the app's random install ID, or `agent`.
pub fn valid_source(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------- hybrid logical clock ----------

/// Hybrid logical clock: wall-clock milliseconds, a counter for changes within
/// the same millisecond (or when a device's clock is behind one it has seen),
/// and the device as tie-breaker. Its string form sorts in the same order, so a
/// server can compare without parsing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hlc {
    pub ms: u64,
    pub counter: u32,
    pub device: String,
}

impl Hlc {
    /// The next timestamp for a change made on `device` now, after `last` (the
    /// newest this device has made or seen).
    pub fn next(last: Option<&Hlc>, now_ms: u64, device: &str) -> Hlc {
        match last {
            Some(l) if l.ms >= now_ms => Hlc { ms: l.ms, counter: l.counter + 1, device: device.into() },
            _ => Hlc { ms: now_ms, counter: 0, device: device.into() },
        }
    }

    pub fn parse(s: &str) -> Option<Hlc> {
        let mut it = s.splitn(3, '-');
        let ms = it.next()?;
        let counter = it.next()?;
        let device = it.next()?;
        if ms.len() != 13 || counter.len() != 6 || !valid_source(device) {
            return None;
        }
        Some(Hlc { ms: ms.parse().ok()?, counter: counter.parse().ok()?, device: device.into() })
    }
}

impl std::fmt::Display for Hlc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:013}-{:06}-{}", self.ms, self.counter, self.device)
    }
}

// ---------- end-to-end keys for records ----------

/// Keys for an account's own records, derived from its app key (shared by all
/// of the account's devices). The server never has them.
pub struct RecordKeys {
    enc: [u8; 32],
    mac: [u8; 32],
}

impl RecordKeys {
    pub fn from_identity(id: &Identity) -> RecordKeys {
        let secret = id.boxk.to_bytes();
        let hk = Hkdf::<Sha256>::new(Some(b"vfm1-records"), &secret);
        let mut enc = [0u8; 32];
        let mut mac = [0u8; 32];
        hk.expand(b"seal", &mut enc).expect("32 bytes");
        hk.expand(b"key-id", &mut mac).expect("32 bytes");
        RecordKeys { enc, mac }
    }

    /// What the server sees instead of the real key (`usr_…`): an HMAC over
    /// collection and key, hex. Same input, same output, so updates find the record.
    pub fn key_id(&self, collection: &str, key: &str) -> String {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.mac).expect("any key length");
        m.update(collection.as_bytes());
        m.update(&[0]);
        m.update(key.as_bytes());
        hex::encode(m.finalize().into_bytes())
    }

    /// Seal a record's content (CBOR of `{key, value}`, so the real key travels
    /// inside). Bound to collection, key id and schema, so the server can't move
    /// it to another record or pass it off as another version.
    pub fn seal(&self, collection: &str, key_id: &str, schema: u32, plain: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 24];
        OsRng.fill_bytes(&mut nonce);
        let ad = Self::ad(collection, key_id, schema);
        let ct = XChaCha20Poly1305::new((&self.enc).into())
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad: &ad })
            .expect("encryption can't fail");
        let mut out = Vec::with_capacity(24 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn open(&self, collection: &str, key_id: &str, schema: u32, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < 24 + 16 {
            return None;
        }
        let ad = Self::ad(collection, key_id, schema);
        XChaCha20Poly1305::new((&self.enc).into())
            .decrypt(XNonce::from_slice(&sealed[..24]), Payload { msg: &sealed[24..], aad: &ad })
            .ok()
    }

    fn ad(collection: &str, key_id: &str, schema: u32) -> Vec<u8> {
        format!("vfm1-record/{collection}/{key_id}/{schema}").into_bytes()
    }
}

// ---------- wire format ----------

/// One record change, from a device to the server or back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Collection.
    pub c: String,
    /// Key (a [`RecordKeys::key_id`] for sealed records).
    pub k: String,
    /// Schema version of the payload.
    pub v: u32,
    /// Who produced it: a device ID, or `agent`.
    pub source: String,
    /// When the producer observed it (unix ms).
    pub observed_at: i64,
    /// [`Hlc`] as a string, for [`Class::Yours`]; empty otherwise.
    #[serde(default)]
    pub hlc: String,
    #[serde(default)]
    pub deleted: bool,
    /// Sealed content; empty for tombstones.
    #[serde(default, with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Set by the server: the account sequence of this version.
    #[serde(default)]
    pub seq: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncRequest {
    /// The highest sequence this device has (0: everything).
    pub since: u64,
    /// This install's ID.
    pub device: String,
    /// The lease token, needed to write [`Class::Api`] / [`Class::Log`] collections.
    #[serde(default)]
    pub lease: Option<String>,
    /// This device is fetching everything from scratch (it started at `since = 0`
    /// or after a `reset`); pages of that fetch never answer with `reset`.
    #[serde(default)]
    pub full: bool,
    /// Highest schema version this device reads, per collection it knows.
    #[serde(default)]
    pub reads: BTreeMap<String, u32>,
    #[serde(default)]
    pub changes: Vec<Record>,
}

/// Why a change wasn't taken.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rejected {
    pub c: String,
    pub k: String,
    /// `"stale"` (a newer version is stored; it's in `changes`), `"lease"`,
    /// `"other-source"`, `"invalid"`, `"too-big"`, `"quota"`.
    pub why: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SyncResponse {
    /// The sequence to send as `since` next time.
    pub seq: u64,
    /// Changes after `since`, except the ones this request just made.
    pub changes: Vec<Record>,
    /// More changes are waiting: ask again with `since = seq`.
    pub more: bool,
    /// The device was away too long (deletions it missed are gone): drop the
    /// local copy and fetch everything again (`since = 0`, `full = true`).
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub rejected: Vec<Rejected>,
    /// Highest schema version to write, per collection: what every device of the
    /// account seen in the last 90 days can read.
    #[serde(default)]
    pub write_max: BTreeMap<String, u32>,
}

/// `POST /v1/lease` body.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LeaseRequest {
    pub device: String,
    /// `"take"` (or renew), `"release"`.
    pub action: String,
    /// Higher wins a running lease: desktop 20, phone 10 (agent 100, later).
    #[serde(default)]
    pub priority: u8,
    /// The current token, when renewing or releasing.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LeaseResponse {
    /// This device holds the lease; use `token` for writes and renewals.
    pub held: bool,
    #[serde(default)]
    pub token: Option<String>,
    /// Who holds it (device ID), if anyone.
    #[serde(default)]
    pub holder: Option<String>,
    /// Unix ms when it runs out unless renewed.
    #[serde(default)]
    pub expires: i64,
}

/// The plaintext inside a sealed record: the real key and the value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Content<V> {
    pub k: String,
    pub v: V,
}

/// CBOR of a value (record contents before sealing).
pub fn to_cbor<T: Serialize>(v: &T) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(v, &mut out).expect("serializing to memory can't fail");
    out
}

pub fn from_cbor<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    ciborium::from_reader(bytes).map_err(|e| format!("bad CBOR: {e}"))
}

/// CBOR, then zstd.
pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    let mut cbor = Vec::new();
    ciborium::into_writer(v, &mut cbor).expect("serializing to memory can't fail");
    zstd::bulk::compress(&cbor, 3).expect("compressing in memory can't fail")
}

/// The other way; `max` limits the decompressed size.
pub fn decode<T: DeserializeOwned>(bytes: &[u8], max: usize) -> Result<T, String> {
    let cbor = zstd::bulk::decompress(bytes, max).map_err(|e| format!("not zstd, or bigger than {max} bytes: {e}"))?;
    ciborium::from_reader(cbor.as_slice()).map_err(|e| format!("bad CBOR: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hlc_orders_and_round_trips() {
        let a = Hlc::next(None, 1_700_000_000_000, "dev-a");
        let b = Hlc::next(Some(&a), 1_699_999_999_000, "dev-b"); // clock behind: counter goes up
        assert!(b > a);
        assert!(b.to_string() > a.to_string(), "string order matches");
        assert_eq!(Hlc::parse(&b.to_string()), Some(b.clone()));
        let c = Hlc::next(Some(&b), 1_700_000_000_500, "dev-a");
        assert_eq!(c.counter, 0);
        assert!(c > b);
        assert!(Hlc::parse("nonsense").is_none());
    }

    #[test]
    fn records_seal_and_hide_keys() {
        let id = Identity::generate();
        let k = RecordKeys::from_identity(&id);
        let kid = k.key_id("friends", "usr_123");
        assert_eq!(kid, k.key_id("friends", "usr_123"));
        assert_ne!(kid, k.key_id("profiles", "usr_123"));
        assert!(!kid.contains("usr_"));
        let sealed = k.seal("friends", &kid, 1, b"hello");
        assert_eq!(k.open("friends", &kid, 1, &sealed).as_deref(), Some(&b"hello"[..]));
        assert!(k.open("friends", &kid, 2, &sealed).is_none(), "bound to the schema");
        assert!(k.open("profiles", &kid, 1, &sealed).is_none(), "bound to the collection");
        let other = RecordKeys::from_identity(&Identity::generate());
        assert!(other.open("friends", &kid, 1, &sealed).is_none());
        // Same identity on another device: same keys.
        let again = RecordKeys::from_identity(&Identity::from_secret_string(&id.to_secret_string()).unwrap());
        assert_eq!(again.open("friends", &kid, 1, &sealed).as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn wire_round_trip() {
        let req = SyncRequest {
            since: 5,
            full: false,
            device: "dev-a".into(),
            lease: None,
            reads: [("friends".to_string(), 1)].into(),
            changes: vec![Record {
                c: "friends".into(),
                k: "ab".into(),
                v: 1,
                source: "dev-a".into(),
                observed_at: 1,
                hlc: String::new(),
                deleted: false,
                payload: vec![1, 2, 3],
                seq: 0,
            }],
        };
        let bytes = encode(&req);
        let back: SyncRequest = decode(&bytes, 1 << 20).unwrap();
        assert_eq!(back.changes, req.changes);
        assert!(decode::<SyncRequest>(&bytes, 4).is_err(), "size limit");
    }

    #[test]
    fn classes() {
        assert_eq!(class_of("friends"), Class::Api);
        assert_eq!(class_of("visits"), Class::GameLog);
        assert_eq!(class_of("somethingNew"), Class::Yours);
        let mut names: Vec<_> = COLLECTIONS.iter().map(|d| d.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), COLLECTIONS.len(), "no duplicates");
        assert!(COLLECTIONS.iter().all(|d| valid_collection(d.name)));
    }
}
