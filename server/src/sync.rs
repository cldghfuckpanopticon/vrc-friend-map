//! Record sync (`POST /v1/sync`) and the per-account lease (`POST /v1/lease`).
//! See docs/design/server-source-of-truth.md.
//!
//! ```text
//! vfm-data/
//!   sync.sqlite                 records of every account (0600)
//!   backups/sync-v<N>-<date>.sqlite   copies made before each database migration (last 3)
//! ```
//!
//! The server stores records it mostly can't read (end-to-end sealed by the
//! account's devices). What it checks per collection class:
//! * API and log collections: only the device holding the account's lease writes.
//! * Game-log collections: a record can only be changed by the device that made it.
//! * The person's own data: the newest change by hybrid logical clock wins.

use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use vfm_share::records::{self, Class, LeaseRequest, LeaseResponse, Record, Rejected, SyncRequest, SyncResponse};

/// One entry per database version; `MIGRATIONS[i]` takes the database from version `i` to `i + 1`.
/// Never edit a released entry: add a new one.
const MIGRATIONS: &[&str] = &[
    // 1: records, accounts, what each device can read.
    "CREATE TABLE accounts (
        account    TEXT PRIMARY KEY,
        seq        INTEGER NOT NULL DEFAULT 0,
        purged_seq INTEGER NOT NULL DEFAULT 0,
        bytes      INTEGER NOT NULL DEFAULT 0
     ) WITHOUT ROWID;
     CREATE TABLE records (
        account     TEXT NOT NULL,
        collection  TEXT NOT NULL,
        key         TEXT NOT NULL,
        seq         INTEGER NOT NULL,
        schema      INTEGER NOT NULL,
        source      TEXT NOT NULL,
        observed_at INTEGER NOT NULL,
        hlc         TEXT NOT NULL DEFAULT '',
        deleted     INTEGER NOT NULL DEFAULT 0,
        payload     BLOB,
        updated_at  INTEGER NOT NULL,
        PRIMARY KEY (account, collection, key)
     ) WITHOUT ROWID;
     CREATE INDEX records_by_seq ON records (account, seq);
     CREATE TABLE device_reads (
        account    TEXT NOT NULL,
        device     TEXT NOT NULL,
        collection TEXT NOT NULL,
        schema     INTEGER NOT NULL,
        seen_at    INTEGER NOT NULL,
        PRIMARY KEY (account, device, collection)
     ) WITHOUT ROWID;",
];

/// Largest single record payload.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;
/// Changes accepted per request.
pub const MAX_CHANGES: usize = 5000;
/// Stored payload bytes per account.
pub const MAX_ACCOUNT_BYTES: i64 = 128 * 1024 * 1024;
/// Records per response page, and their payload bytes.
const PAGE_RECORDS: usize = 2000;
const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Tombstones and device reports older than this are dropped.
const KEEP_MS: i64 = 90 * 24 * 3600 * 1000;
/// A lease runs out this long after the last renewal (apps renew every 30 s).
pub const LEASE_MS: i64 = 90_000;
const BACKUPS_KEPT: usize = 3;

struct Lease {
    device: String,
    token: String,
    priority: u8,
    expires: i64,
}

pub struct SyncDb {
    conn: Mutex<Connection>,
    leases: Mutex<HashMap<String, Lease>>,
}

impl SyncDb {
    /// Open (or create) `dir/sync.sqlite`, backing it up and migrating it if it's older.
    pub fn open(dir: &Path) -> Result<SyncDb, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join("sync.sqlite");
        let conn = Connection::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;").map_err(|e| e.to_string())?;
        migrate(&conn, dir, &path)?;
        Ok(SyncDb { conn: Mutex::new(conn), leases: Mutex::new(HashMap::new()) })
    }

    #[cfg(test)]
    fn memory() -> SyncDb {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn, Path::new("."), Path::new(":memory:")).unwrap();
        SyncDb { conn: Mutex::new(conn), leases: Mutex::new(HashMap::new()) }
    }

    // ---------- lease ----------

    pub fn lease(&self, account: &str, req: &LeaseRequest, now: i64) -> LeaseResponse {
        let mut leases = self.leases.lock().unwrap();
        if leases.get(account).is_some_and(|l| l.expires <= now) {
            leases.remove(account);
        }
        let ours = |l: &Lease| l.device == req.device && req.token.as_deref().map_or(true, |t| t == l.token);
        if req.action == "release" {
            if leases.get(account).is_some_and(|l| ours(l)) {
                leases.remove(account);
            }
            return LeaseResponse { held: false, token: None, holder: None, expires: 0 };
        }
        let grant = match leases.get(account) {
            None => true,
            Some(l) if ours(l) => true,
            // A device that matters more (desktop over phone; the agent, later, over both) takes over.
            Some(l) => req.priority > l.priority,
        };
        if grant {
            let token = match leases.get(account) {
                Some(l) if ours(l) => l.token.clone(),
                _ => crate::config::random_token(24),
            };
            let l = Lease { device: req.device.clone(), token: token.clone(), priority: req.priority, expires: now + LEASE_MS };
            let expires = l.expires;
            leases.insert(account.to_string(), l);
            LeaseResponse { held: true, token: Some(token), holder: Some(req.device.clone()), expires }
        } else {
            let l = &leases[account];
            LeaseResponse { held: false, token: None, holder: Some(l.device.clone()), expires: l.expires }
        }
    }

    fn lease_ok(&self, account: &str, device: &str, token: Option<&str>, now: i64) -> bool {
        let leases = self.leases.lock().unwrap();
        matches!((leases.get(account), token), (Some(l), Some(t)) if l.expires > now && l.device == device && l.token == t)
    }

    // ---------- sync ----------

    /// Apply a device's changes and answer with everything it hasn't seen.
    /// Returns the response and whether anything was stored (to notify the account's other devices).
    pub fn sync(&self, account: &str, req: &SyncRequest, now: i64) -> Result<(SyncResponse, bool), String> {
        if !records::valid_source(&req.device) {
            return Err("bad device ID".into());
        }
        if req.changes.len() > MAX_CHANGES {
            return Err(format!("at most {MAX_CHANGES} changes per request"));
        }
        let leased = self.lease_ok(account, &req.device, req.lease.as_deref(), now);
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT OR IGNORE INTO accounts (account) VALUES (?1)", params![account]).map_err(|e| e.to_string())?;
        let (mut seq, purged_seq, mut bytes): (u64, u64, i64) = tx
            .query_row("SELECT seq, purged_seq, bytes FROM accounts WHERE account = ?1", params![account], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(|e| e.to_string())?;

        let mut rejected = Vec::new();
        let mut written: HashSet<u64> = HashSet::new();
        // Stored versions a device tried to overwrite with an older one: sent back so it can merge.
        let mut send_back: Vec<(String, String)> = Vec::new();
        for rec in &req.changes {
            let reject = |why: &str| Rejected { c: rec.c.clone(), k: rec.k.clone(), why: why.into() };
            if !records::valid_collection(&rec.c) || !records::valid_key(&rec.k) || rec.source != req.device {
                rejected.push(reject("invalid"));
                continue;
            }
            if rec.payload.len() > MAX_RECORD_BYTES {
                rejected.push(reject("too-big"));
                continue;
            }
            let class = records::class_of(&rec.c);
            if class == Class::Yours && records::Hlc::parse(&rec.hlc).is_none() {
                rejected.push(reject("invalid"));
                continue;
            }
            let existing: Option<(String, String, i64)> = tx
                .query_row(
                    "SELECT source, hlc, length(coalesce(payload, x'')) FROM records WHERE account = ?1 AND collection = ?2 AND key = ?3",
                    params![account, rec.c, rec.k],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            let ok = match class {
                Class::Api | Class::Log => leased,
                Class::GameLog => existing.as_ref().map_or(true, |(src, _, _)| *src == rec.source),
                Class::Yours => existing.as_ref().map_or(true, |(_, hlc, _)| rec.hlc.as_str() > hlc.as_str()),
            };
            if !ok {
                let why = match class {
                    Class::Api | Class::Log => "lease",
                    Class::GameLog => "other-source",
                    Class::Yours => "stale",
                };
                if why == "stale" {
                    send_back.push((rec.c.clone(), rec.k.clone()));
                }
                rejected.push(reject(why));
                continue;
            }
            let old_len = existing.as_ref().map_or(0, |e| e.2);
            let new_len = if rec.deleted { 0 } else { rec.payload.len() as i64 };
            if bytes - old_len + new_len > MAX_ACCOUNT_BYTES {
                rejected.push(reject("quota"));
                continue;
            }
            seq += 1;
            bytes += new_len - old_len;
            let payload: Option<&[u8]> = if rec.deleted { None } else { Some(&rec.payload) };
            tx.execute(
                "INSERT INTO records (account, collection, key, seq, schema, source, observed_at, hlc, deleted, payload, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (account, collection, key) DO UPDATE SET
                   seq = excluded.seq, schema = excluded.schema, source = excluded.source,
                   observed_at = excluded.observed_at, hlc = excluded.hlc, deleted = excluded.deleted,
                   payload = excluded.payload, updated_at = excluded.updated_at",
                params![account, rec.c, rec.k, seq, rec.v, rec.source, rec.observed_at, rec.hlc, rec.deleted, payload, now],
            )
            .map_err(|e| e.to_string())?;
            written.insert(seq);
        }
        tx.execute("UPDATE accounts SET seq = ?2, bytes = ?3 WHERE account = ?1", params![account, seq, bytes]).map_err(|e| e.to_string())?;
        for (collection, schema) in &req.reads {
            if records::valid_collection(collection) {
                tx.execute(
                    "INSERT INTO device_reads (account, device, collection, schema, seen_at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (account, device, collection) DO UPDATE SET schema = excluded.schema, seen_at = excluded.seen_at",
                    params![account, req.device, collection, schema, now],
                )
                .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;

        let mut res = SyncResponse { seq, rejected, ..Default::default() };
        res.write_max = self.write_max(&conn, account, now)?;
        // Missed deletions were already cleaned up: start over.
        if req.since > 0 && req.since < purged_seq && !req.full {
            res.reset = true;
            res.seq = 0;
            return Ok((res, !written.is_empty()));
        }
        let skip_deleted = req.since == 0 || req.full;
        let mut stmt = conn
            .prepare_cached(
                "SELECT collection, key, schema, source, observed_at, hlc, deleted, payload, seq FROM records
                 WHERE account = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![account, req.since]).map_err(|e| e.to_string())?;
        let mut size = 0usize;
        let mut last = req.since;
        while let Some(r) = rows.next().map_err(|e| e.to_string())? {
            let rec = row_record(r).map_err(|e| e.to_string())?;
            if res.changes.len() >= PAGE_RECORDS || size >= PAGE_BYTES {
                res.more = true;
                break;
            }
            last = rec.seq;
            if written.contains(&rec.seq) || (skip_deleted && rec.deleted) {
                continue;
            }
            size += rec.payload.len() + 128;
            res.changes.push(rec);
        }
        drop(rows);
        drop(stmt);
        if res.more {
            res.seq = last;
        }
        for (c, k) in send_back {
            if res.changes.iter().any(|r| r.c == c && r.k == k) {
                continue;
            }
            let rec = conn
                .query_row(
                    "SELECT collection, key, schema, source, observed_at, hlc, deleted, payload, seq FROM records
                     WHERE account = ?1 AND collection = ?2 AND key = ?3",
                    params![account, c, k],
                    row_record,
                )
                .map_err(|e| e.to_string())?;
            res.changes.push(rec);
        }
        Ok((res, !written.is_empty()))
    }

    /// Per collection, the highest schema every device of the account (seen in the last 90 days) reads.
    fn write_max(&self, conn: &Connection, account: &str, now: i64) -> Result<BTreeMap<String, u32>, String> {
        let mut stmt = conn
            .prepare_cached("SELECT collection, min(schema) FROM device_reads WHERE account = ?1 AND seen_at > ?2 GROUP BY collection")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![account, now - KEEP_MS], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
    }

    /// Remove everything of an account (account deleted on the server).
    pub fn delete_account(&self, account: &str) {
        let conn = self.conn.lock().unwrap();
        for table in ["records", "device_reads", "accounts"] {
            let _ = conn.execute(&format!("DELETE FROM {table} WHERE account = ?1"), params![account]);
        }
        self.leases.lock().unwrap().remove(account);
    }

    /// Drop tombstones and device reports older than 90 days. Devices that
    /// were behind them get `reset` on their next sync.
    pub fn cleanup(&self, now: i64) -> Result<usize, String> {
        let cutoff = now - KEEP_MS;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE accounts SET purged_seq = max(purged_seq, (
                SELECT coalesce(max(seq), 0) FROM records r
                WHERE r.account = accounts.account AND r.deleted = 1 AND r.updated_at < ?1))",
            params![cutoff],
        )
        .map_err(|e| e.to_string())?;
        let n = tx.execute("DELETE FROM records WHERE deleted = 1 AND updated_at < ?1", params![cutoff]).map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM device_reads WHERE seen_at < ?1", params![cutoff]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(n)
    }

    /// Storage per account, for the admin page: (records, bytes).
    pub fn usage(&self, account: &str) -> (u64, i64) {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT (SELECT count(*) FROM records WHERE account = ?1 AND deleted = 0), coalesce((SELECT bytes FROM accounts WHERE account = ?1), 0)",
            params![account],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0))
    }
}

fn row_record(r: &rusqlite::Row) -> rusqlite::Result<Record> {
    Ok(Record {
        c: r.get(0)?,
        k: r.get(1)?,
        v: r.get(2)?,
        source: r.get(3)?,
        observed_at: r.get(4)?,
        hlc: r.get(5)?,
        deleted: r.get(6)?,
        payload: r.get::<_, Option<Vec<u8>>>(7)?.unwrap_or_default(),
        seq: r.get(8)?,
    })
}

/// Bring the database to the newest version, backing it up first if it has data.
fn migrate(conn: &Connection, dir: &Path, path: &Path) -> Result<(), String> {
    let latest = MIGRATIONS.len() as i64;
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    if current > latest {
        return Err(format!(
            "{} was written by a newer vfm-server (database version {current}, this one knows {latest}). Update vfm-server, or restore a backup from {}.",
            path.display(),
            dir.join("backups").display()
        ));
    }
    if current == latest {
        return Ok(());
    }
    if current > 0 {
        backup(conn, dir, current)?;
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = i as i64 + 1;
        conn.execute_batch(&format!("BEGIN; {sql}; PRAGMA user_version = {version}; COMMIT;"))
            .map_err(|e| format!("database migration to version {version} failed: {e}"))?;
    }
    Ok(())
}

/// `VACUUM INTO backups/sync-v<version>-<date>.sqlite`, keeping the newest few.
fn backup(conn: &Connection, dir: &Path, version: i64) -> Result<PathBuf, String> {
    let backups = dir.join("backups");
    std::fs::create_dir_all(&backups).map_err(|e| e.to_string())?;
    let stamp = crate::store::now();
    let file = backups.join(format!("sync-v{version}-{stamp}.sqlite"));
    conn.execute("VACUUM INTO ?1", params![file.to_string_lossy()]).map_err(|e| format!("backup before migrating failed: {e}"))?;
    let mut old: Vec<PathBuf> = std::fs::read_dir(&backups)
        .map(|it| it.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("sync-v"))).collect())
        .unwrap_or_default();
    old.sort();
    while old.len() > BACKUPS_KEPT {
        let _ = std::fs::remove_file(old.remove(0));
    }
    println!("Database backed up to {} before updating it", file.display());
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfm_share::records::Hlc;

    fn rec(c: &str, k: &str, source: &str, payload: &[u8]) -> Record {
        Record { c: c.into(), k: k.into(), v: 1, source: source.into(), observed_at: 1, hlc: String::new(), deleted: false, payload: payload.to_vec(), seq: 0 }
    }

    fn req(device: &str, since: u64, changes: Vec<Record>) -> SyncRequest {
        SyncRequest { since, device: device.into(), changes, ..Default::default() }
    }

    fn take(db: &SyncDb, acc: &str, dev: &str, prio: u8, now: i64) -> LeaseResponse {
        db.lease(acc, &LeaseRequest { device: dev.into(), action: "take".into(), priority: prio, token: None }, now)
    }

    #[test]
    fn api_collections_need_the_lease() {
        let db = SyncDb::memory();
        let (r, stored) = db.sync("usr_a", &req("pc", 0, vec![rec("friends", "k1", "pc", b"x")]), 1000).unwrap();
        assert!(!stored);
        assert_eq!(r.rejected[0].why, "lease");
        let l = take(&db, "usr_a", "pc", 20, 1000);
        assert!(l.held);
        let mut q = req("pc", 0, vec![rec("friends", "k1", "pc", b"x")]);
        q.lease = l.token.clone();
        let (r, stored) = db.sync("usr_a", &q, 1000).unwrap();
        assert!(stored && r.rejected.is_empty());
        assert_eq!(r.seq, 1);
        assert!(r.changes.is_empty(), "its own write isn't sent back");
        // The phone sees it.
        let (r, _) = db.sync("usr_a", &req("phone", 0, vec![]), 1001).unwrap();
        assert_eq!(r.changes.len(), 1);
        assert_eq!(r.changes[0].payload, b"x");
        // Another account doesn't.
        let (r, _) = db.sync("usr_b", &req("phone", 0, vec![]), 1001).unwrap();
        assert!(r.changes.is_empty());
    }

    #[test]
    fn lease_priority_and_expiry() {
        let db = SyncDb::memory();
        assert!(take(&db, "usr_a", "phone", 10, 0).held);
        let pc = take(&db, "usr_a", "pc", 20, 1);
        assert!(pc.held, "desktop takes over from the phone");
        let phone = take(&db, "usr_a", "phone", 10, 2);
        assert!(!phone.held);
        assert_eq!(phone.holder.as_deref(), Some("pc"));
        let renewed = db.lease("usr_a", &LeaseRequest { device: "pc".into(), action: "take".into(), priority: 20, token: pc.token.clone() }, 3);
        assert_eq!(renewed.token, pc.token, "renewing keeps the token");
        assert!(take(&db, "usr_a", "phone", 10, 3 + LEASE_MS).held, "after it runs out, the phone gets it");
        // The old token no longer writes.
        let mut q = req("pc", 0, vec![rec("friends", "k", "pc", b"x")]);
        q.lease = pc.token.clone();
        assert_eq!(db.sync("usr_a", &q, 4 + LEASE_MS).unwrap().0.rejected[0].why, "lease");
    }

    #[test]
    fn your_data_newest_wins() {
        let db = SyncDb::memory();
        let mut a = rec("circles", "c1", "pc", b"old");
        a.hlc = Hlc { ms: 2000, counter: 0, device: "pc".into() }.to_string();
        db.sync("usr_a", &req("pc", 0, vec![a.clone()]), 1).unwrap();
        let mut b = rec("circles", "c1", "phone", b"older");
        b.hlc = Hlc { ms: 1000, counter: 0, device: "phone".into() }.to_string();
        let (r, stored) = db.sync("usr_a", &req("phone", 1, vec![b]), 2).unwrap();
        assert!(!stored);
        assert_eq!(r.rejected[0].why, "stale");
        assert_eq!(r.changes.len(), 1, "the stored version comes back for merging");
        assert_eq!(r.changes[0].payload, b"old");
        let mut c = rec("circles", "c1", "phone", b"new");
        c.hlc = Hlc { ms: 3000, counter: 0, device: "phone".into() }.to_string();
        assert!(db.sync("usr_a", &req("phone", 1, vec![c]), 3).unwrap().1);
    }

    #[test]
    fn game_log_records_belong_to_their_client() {
        let db = SyncDb::memory();
        assert!(db.sync("usr_a", &req("quest", 0, vec![rec("visits", "v1", "quest", b"q")]), 1).unwrap().1);
        let (r, stored) = db.sync("usr_a", &req("pc", 0, vec![rec("visits", "v1", "pc", b"p")]), 2).unwrap();
        assert!(!stored);
        assert_eq!(r.rejected[0].why, "other-source");
        assert!(db.sync("usr_a", &req("pc", 0, vec![rec("visits", "v2", "pc", b"p")]), 3).unwrap().1);
    }

    #[test]
    fn source_must_be_the_sender() {
        let db = SyncDb::memory();
        let (r, _) = db.sync("usr_a", &req("pc", 0, vec![rec("visits", "v1", "quest", b"q")]), 1).unwrap();
        assert_eq!(r.rejected[0].why, "invalid");
    }

    #[test]
    fn paging_and_cursor() {
        let db = SyncDb::memory();
        let changes: Vec<Record> = (0..PAGE_RECORDS + 10).map(|i| rec("visits", &format!("v{i}"), "pc", b"x")).collect();
        for chunk in changes.chunks(MAX_CHANGES) {
            db.sync("usr_a", &req("pc", 0, chunk.to_vec()), 1).unwrap();
        }
        let (p1, _) = db.sync("usr_a", &req("phone", 0, vec![]), 2).unwrap();
        assert!(p1.more);
        assert_eq!(p1.changes.len(), PAGE_RECORDS);
        let (p2, _) = db.sync("usr_a", &SyncRequest { since: p1.seq, device: "phone".into(), full: true, ..Default::default() }, 3).unwrap();
        assert!(!p2.more);
        assert_eq!(p2.changes.len(), 10);
        assert_eq!(p2.seq, (PAGE_RECORDS + 10) as u64);
        let (p3, _) = db.sync("usr_a", &req("phone", p2.seq, vec![]), 4).unwrap();
        assert!(p3.changes.is_empty());
    }

    #[test]
    fn deletions_and_reset() {
        let db = SyncDb::memory();
        db.sync("usr_a", &req("pc", 0, vec![rec("visits", "v1", "pc", b"x"), rec("visits", "v2", "pc", b"y")]), 1).unwrap();
        let (phone, _) = db.sync("usr_a", &req("phone", 0, vec![]), 1).unwrap();
        let mut del = rec("visits", "v1", "pc", b"");
        del.deleted = true;
        db.sync("usr_a", &req("pc", 2, vec![del]), 2).unwrap();
        // A device that's current gets the tombstone.
        let (r, _) = db.sync("usr_a", &req("phone", phone.seq, vec![]), 3).unwrap();
        assert!(r.changes[0].deleted);
        // A new device doesn't need it.
        let (r, _) = db.sync("usr_a", &req("laptop", 0, vec![]), 3).unwrap();
        assert_eq!(r.changes.len(), 1);
        assert_eq!(db.usage("usr_a"), (1, 1));
        // 90 days later the tombstone is gone, and a device behind it starts over.
        assert_eq!(db.cleanup(3 + KEEP_MS).unwrap(), 1);
        let (r, _) = db.sync("usr_a", &req("phone", phone.seq, vec![]), 4 + KEEP_MS).unwrap();
        assert!(r.reset);
    }

    #[test]
    fn write_max_is_what_every_device_reads() {
        let db = SyncDb::memory();
        let mut q = req("pc", 0, vec![]);
        q.reads = [("profiles".to_string(), 3)].into();
        db.sync("usr_a", &q, 1).unwrap();
        let mut q = req("phone", 0, vec![]);
        q.reads = [("profiles".to_string(), 2)].into();
        let (r, _) = db.sync("usr_a", &q, 2).unwrap();
        assert_eq!(r.write_max.get("profiles"), Some(&2));
        // The old phone hasn't been seen for 90 days: the PC may write 3.
        let mut q = req("pc", 0, vec![]);
        q.reads = [("profiles".to_string(), 3)].into();
        let (r, _) = db.sync("usr_a", &q, 3 + KEEP_MS).unwrap();
        assert_eq!(r.write_max.get("profiles"), Some(&3));
    }

    #[test]
    fn quota_and_size() {
        let db = SyncDb::memory();
        let big = vec![0u8; MAX_RECORD_BYTES + 1];
        let (r, _) = db.sync("usr_a", &req("pc", 0, vec![rec("visits", "v", "pc", &big)]), 1).unwrap();
        assert_eq!(r.rejected[0].why, "too-big");
    }

    #[test]
    fn database_versions() {
        let dir = std::env::temp_dir().join(format!("vfm-sync-test-{}", crate::config::random_token(6)));
        {
            let db = SyncDb::open(&dir).unwrap();
            db.sync("usr_a", &req("pc", 0, vec![rec("visits", "v", "pc", b"x")]), 1).unwrap();
        }
        // Reopening at the same version: no backup.
        drop(SyncDb::open(&dir).unwrap());
        assert!(!dir.join("backups").exists());
        // A database from a newer server is refused, not guessed at.
        {
            let conn = Connection::open(dir.join("sync.sqlite")).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {}", MIGRATIONS.len() + 1)).unwrap();
        }
        let err = SyncDb::open(&dir).err().unwrap();
        assert!(err.contains("newer vfm-server"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_before_migrating() {
        let dir = std::env::temp_dir().join(format!("vfm-sync-test-{}", crate::config::random_token(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join("sync.sqlite")).unwrap();
        // A database at version 1 with data, then pretend there's a version 2 to migrate to.
        migrate(&conn, &dir, &dir.join("sync.sqlite")).unwrap();
        conn.execute("INSERT INTO accounts (account) VALUES ('usr_a')", []).unwrap();
        let file = backup(&conn, &dir, 1).unwrap();
        let copy = Connection::open(&file).unwrap();
        let n: i64 = copy.query_row("SELECT count(*) FROM accounts", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
