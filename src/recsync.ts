// Record sync with vfm-servers ("Sync all my data through this server", beta).
// See docs/design/server-source-of-truth.md.
//
// - A device logged in to VRChat takes the account's lease on the server (a
//   desktop takes it over from a phone). While it holds it, it uploads its VRChat
//   data (friends, profiles, …) as records: only the ones that changed, a few
//   seconds after they did. Sealing and opening happen on the Rust side, with your
//   key; the server can't read them.
// - A device without a VRChat login (serverOnly) reads those records into its
//   cache and shows them. It never takes the lease.
// - A logged-in device that doesn't get the lease (another of your devices has it)
//   follows: it stops its own full VRChat syncs and reads the holder's records. It
//   keeps VRChat's live connection for now (invites and notifications aren't records
//   yet). When the holder goes away it takes the lease and fetches again itself.

import { backend, isMobile, type RecOut, type RecSyncResult } from "./backend";
import { emptyCache, getState, setLogs, setState, subscribeState, updateCache, updateServer } from "./store";
import { syncAll } from "./sync";
import { refreshStats } from "./logs";
import { addServer } from "./share";
import type { Cache, ShareServer } from "./types";

/** VRChat-derived collections this version syncs, and the cache fields they map to. */
const MAPPED: [collection: string, field: keyof Cache][] = [
  ["friends", "friends"],
  ["presence", "presence"],
  ["profiles", "details"],
  ["mutuals", "mutuals"],
  ["favorites", "favorites"],
  ["worlds", "worlds"],
  ["groups", "groups"],
  ["others", "others"],
];
/** Collections whose cache field is `{ byUser, fetchedAt }`. */
const BY_USER: [collection: string, field: "moderations" | "userNotes"][] = [
  ["moderations", "moderations"],
  ["userNotes", "userNotes"],
];
export const API_COLLECTIONS = ["me", ...MAPPED.map(([c]) => c), ...BY_USER.map(([c]) => c)];
/** Schema versions this app reads (all 1 so far). */
const READS: Record<string, number> = Object.fromEntries(API_COLLECTIONS.map((c) => [c, 1]));
const META = "_meta";

export interface Rec {
  c: string;
  key: string;
  value: unknown;
}

const id = (c: string, key: string) => `${c}\u0000${key}`;

/** The cache as records, by `collection \0 key`. */
export function cacheRecords(cache: Cache): Map<string, Rec> {
  const out = new Map<string, Rec>();
  const put = (c: string, key: string, value: unknown) => {
    if (value !== undefined) out.set(id(c, key), { c, key, value });
  };
  if (cache.me) put("me", "me", cache.me);
  put("me", META, { lastFriendsSync: cache.lastFriendsSync || 0 });
  for (const [c, field] of MAPPED) {
    const m = cache[field] as Record<string, unknown> | undefined;
    for (const [k, v] of Object.entries(m ?? {})) put(c, k, v);
  }
  for (const [c, field] of BY_USER) {
    const m = cache[field];
    if (!m) continue;
    for (const [k, v] of Object.entries(m.byUser)) put(c, k, v);
    put(c, META, { fetchedAt: m.fetchedAt });
  }
  return out;
}

/** Apply opened records to a cache (deletions name their real key). */
export function applyRecords(cache: Cache, changes: { c: string; key: string; value: unknown; deleted: boolean }[]): Cache {
  const next: Cache = { ...cache };
  const touched = new Set<string>();
  const field = <T,>(f: keyof Cache): Record<string, T> => {
    if (!touched.has(f)) {
      (next as unknown as Record<string, unknown>)[f] = { ...((next[f] as Record<string, T> | undefined) ?? {}) };
      touched.add(f);
    }
    return next[f] as unknown as Record<string, T>;
  };
  for (const ch of changes) {
    if (ch.c === "me") {
      if (ch.key === "me") next.me = ch.deleted ? null : (ch.value as Cache["me"]);
      else if (ch.key === META && !ch.deleted) next.lastFriendsSync = (ch.value as { lastFriendsSync?: number })?.lastFriendsSync ?? 0;
      continue;
    }
    const mapped = MAPPED.find(([c]) => c === ch.c);
    if (mapped) {
      const m = field<unknown>(mapped[1]);
      if (ch.deleted) delete m[ch.key];
      else m[ch.key] = ch.value;
      continue;
    }
    const by = BY_USER.find(([c]) => c === ch.c);
    if (by) {
      const f = by[1];
      if (!touched.has(f)) {
        next[f] = { byUser: { ...(next[f]?.byUser ?? {}) }, fetchedAt: next[f]?.fetchedAt ?? 0 } as never;
        touched.add(f);
      }
      const cur = next[f] as { byUser: Record<string, unknown>; fetchedAt: number };
      if (ch.key === META) {
        if (!ch.deleted) cur.fetchedAt = (ch.value as { fetchedAt?: number })?.fetchedAt ?? 0;
      } else if (ch.deleted) delete cur.byUser[ch.key];
      else cur.byUser[ch.key] = ch.value;
    }
    // Collections this version doesn't know yet: ignored (kept on the server).
  }
  return next;
}

/** A small, fast hash of a record's content, to send only what changed. */
export function hashValue(v: unknown): number {
  const str = JSON.stringify(v) ?? "";
  let h1 = 0xdeadbeef;
  let h2 = 0x41c6ce57;
  for (let i = 0; i < str.length; i++) {
    const ch = str.charCodeAt(i);
    h1 = Math.imul(h1 ^ ch, 2654435761);
    h2 = Math.imul(h2 ^ ch, 1597334677);
  }
  h1 = Math.imul(h1 ^ (h1 >>> 16), 2246822507) ^ Math.imul(h2 ^ (h2 >>> 13), 3266489909);
  h2 = Math.imul(h2 ^ (h2 >>> 16), 2246822507) ^ Math.imul(h1 ^ (h1 >>> 13), 3266489909);
  return 4294967296 * (2097151 & h2) + (h1 >>> 0);
}

/** What to send: changed or new records, and deletions for what's gone since the last upload. */
export function diffRecords(current: Map<string, Rec>, sent: Record<string, number>): { upserts: Rec[]; deletes: Rec[]; hashes: Record<string, number> } {
  const upserts: Rec[] = [];
  const deletes: Rec[] = [];
  const hashes: Record<string, number> = {};
  for (const [k, r] of current) {
    const h = hashValue(r.value);
    hashes[k] = h;
    if (sent[k] !== h) upserts.push(r);
  }
  for (const k of Object.keys(sent)) {
    if (!current.has(k)) {
      const [c, key] = k.split("\u0000");
      deletes.push({ c, key, value: null });
    }
  }
  return { upserts, deletes, hashes };
}

// ---------- the engine ----------

const PRIORITY = isMobile ? 10 : 20;
const LEASE_EVERY_MS = 30_000;
const PUSH_DELAY_MS = 5_000;
const PULL_DELAY_MS = 1_000;
/** Changes per request (the server takes up to 5000). */
const CHUNK = 2000;

interface ServerRun {
  held: boolean;
  /** A push or pull is running. */
  busy: boolean;
  again: boolean;
  pushTimer?: ReturnType<typeof setTimeout>;
  pullTimer?: ReturnType<typeof setTimeout>;
}

const runs = new Map<string, ServerRun>();
let leaseTimer: ReturnType<typeof setInterval> | null = null;
let unsubscribe: (() => void) | null = null;
let unlisten: (() => void) | null = null;
let lastCache: Cache | null = null;

type SentKind = "api" | "visits";
const sentKey = (s: ShareServer, kind: SentKind) => (kind === "api" ? `vfm-recsync-sent:${s.id}` : `vfm-recsync-sent-${kind}:${s.id}`);
/** What was last uploaded (hashes per record), kept in localStorage; in memory where that
 *  isn't available (then the worst case is one full upload per start). */
const sentMemory = new Map<string, Record<string, number>>();
function loadSent(s: ShareServer, kind: SentKind = "api"): Record<string, number> {
  const k = sentKey(s, kind);
  try {
    const v = localStorage.getItem(k);
    if (v != null) return JSON.parse(v);
  } catch {
    /* fall back to memory */
  }
  return sentMemory.get(k) ?? {};
}
function saveSent(s: ShareServer, v: Record<string, number> | null, kind: SentKind = "api") {
  const k = sentKey(s, kind);
  if (v) sentMemory.set(k, v);
  else sentMemory.delete(k);
  try {
    if (v) localStorage.setItem(k, JSON.stringify(v));
    else localStorage.removeItem(k);
  } catch {
    /* memory only */
  }
}

/** Servers this device syncs records with. */
export function recordServers(): ShareServer[] {
  const st = getState();
  return st.settings.sharing.servers.filter((s) => s.syncAll && s.info?.recordSync && !s.keyChanged && (s.registered || !!st.serverOnly));
}

function device(): string {
  return getState().local.deviceId || "device";
}

function run(s: ShareServer): ServerRun {
  let r = runs.get(s.id);
  if (!r) runs.set(s.id, (r = { held: false, busy: false, again: false }));
  return r;
}

function note(s: ShareServer, text: string) {
  console.info(`[records] ${s.url}: ${text}`);
}

async function exclusive(s: ShareServer, job: () => Promise<void>) {
  const r = run(s);
  if (r.busy) {
    r.again = true;
    return;
  }
  r.busy = true;
  try {
    await job();
  } catch (e) {
    note(s, `failed: ${e}`);
  } finally {
    r.busy = false;
    if (r.again) {
      r.again = false;
      schedulePull(s, PULL_DELAY_MS);
    }
  }
}

/** Upload what changed (lease holder only). */
async function push(s: ShareServer) {
  const r = run(s);
  if (!r.held || getState().serverOnly) return;
  // Mid-sync the cache is half old, half new (an app that just started): wait for VRChat.
  if (getState().sync.running) {
    schedulePush(s, 10_000);
    return;
  }
  await exclusive(s, async () => {
    const sent = loadSent(s);
    const { upserts, deletes, hashes } = diffRecords(cacheRecords(getState().cache), sent);
    const now = Date.now();
    const changes = [
      ...upserts.map((x) => ({ c: x.c, key: x.key, value: x.value, observedAt: now })),
      ...deletes.map((x) => ({ c: x.c, key: x.key, deleted: true, observedAt: now })),
    ];
    if (!changes.length) return;
    const rejected = new Set<string>();
    for (let i = 0; i < changes.length; i += CHUNK) {
      const res = await backend.records.sync(s.url, device(), false, READS, changes.slice(i, i + CHUNK));
      // The answer also carries what other devices wrote meanwhile (their visits).
      await applyResult(s, res, false);
      for (const x of res.rejected) {
        rejected.add(id(x.c, x.key));
        if (x.why === "lease") r.held = false;
      }
      if (!r.held) break;
    }
    // Remember what the server has: everything sent minus the rejected ones.
    const next: Record<string, number> = {};
    for (const [k, h] of Object.entries(hashes)) if (!rejected.has(k)) next[k] = h;
    if (!r.held) {
      // Lost the lease halfway: start from a full upload when we get it back.
      saveSent(s, null);
      return;
    }
    saveSent(s, next);
    note(s, `uploaded ${changes.length - rejected.size} changes`);
  });
}

/** Fetch what changed; a device without a VRChat login, or one following another
 *  device, applies it. `full`: everything (when starting to follow). */
async function pull(s: ShareServer, full = false) {
  await exclusive(s, async () => {
    const res = await backend.records.sync(s.url, device(), full, READS, []);
    await applyResult(s, res, full);
  });
}

/** Take in what a sync answer brought: VRChat data (on devices that read it) and other
 *  VRChat clients' visits (everywhere). */
async function applyResult(s: ShareServer, res: RecSyncResult, full: boolean) {
  const st = getState();
  if (res.unreadable) note(s, `${res.unreadable} records couldn't be opened (another key?)`);
  // VRChat data: only where this device doesn't fetch it itself.
  if (st.serverOnly || st.following === s.url) {
    const api = res.changes.filter((c) => API_COLLECTIONS.includes(c.c));
    // Replace this device's copy only if the server really has the account's data (the
    // holder may have just taken the lease and not uploaded yet): never wipe it for nothing.
    const replace = (res.reset || full) && api.some((c) => c.c === "me" && c.key === "me" && !c.deleted);
    const resolved = await resolveDeletions(replace ? emptyCache() : getState().cache, api);
    if (api.length) updateCache((c) => applyRecords(replace ? { ...emptyCache(), linkReads: c.linkReads, ageHints: c.ageHints } : c, resolved));
    setState({ recordsAt: Date.now() });
  }
  // Visits from your other VRChat clients.
  const visits = res.changes.filter((c) => c.c === "visits" && c.source !== device());
  if (res.reset) await backend.logsRemoteClear();
  if (!visits.length) return;
  const gone = await resolveVisitDeletions(visits.filter((v) => v.deleted && v.key.startsWith("#")));
  const bySource = new Map<string, { up: [string, unknown][]; del: string[] }>();
  const slot = (src: string) => bySource.get(src) ?? bySource.set(src, { up: [], del: [] }).get(src)!;
  for (const v of visits) {
    if (v.deleted) {
      const g = gone.get(v.key);
      if (g) slot(g.source).del.push(g.key);
    } else {
      const prefix = `${v.source}/`;
      if (v.key.startsWith(prefix)) slot(v.source).up.push([v.key.slice(prefix.length), v.value]);
    }
  }
  for (const [src, { up, del }] of bySource) await backend.logsRemoteApply(src, up, del);
  const report = await backend.logsInfo().catch(() => null);
  if (report) setLogs({ report });
  void refreshStats(true);
}

/** Visit deletions arrive as "#<key id>": match them against the stored visits of other clients. */
async function resolveVisitDeletions(gone: RecOut[]): Promise<Map<string, { source: string; key: string }>> {
  const out = new Map<string, { source: string; key: string }>();
  if (!gone.length) return out;
  const stored = await backend.logsRemoteKeys();
  const ids = await backend.records.keyIds(stored.map(([src, key]) => ["visits", `${src}/${key}`]));
  const want = new Set(gone.map((g) => g.key));
  ids.forEach((kid, i) => {
    if (want.has(kid)) out.set(kid, { source: stored[i][0], key: stored[i][1] });
  });
  return out;
}

/** Upload this device's own game-log visits (no lease: each VRChat client writes its own). */
async function pushVisits(s: ShareServer) {
  if (getState().serverOnly) return;
  await exclusive(s, async () => {
    const own = await backend.logsOwnVisits();
    if (!own.length && !Object.keys(loadSent(s, "visits")).length) return;
    const current = new Map<string, Rec>(own.map(([k, v]) => {
      const key = `${device()}/${k}`;
      return [id("visits", key), { c: "visits", key, value: v }];
    }));
    const sent = loadSent(s, "visits");
    const { upserts, deletes, hashes } = diffRecords(current, sent);
    const now = Date.now();
    const changes = [
      ...upserts.map((x) => ({ c: x.c, key: x.key, value: x.value, observedAt: now })),
      ...deletes.map((x) => ({ c: x.c, key: x.key, deleted: true, observedAt: now })),
    ];
    if (!changes.length) return;
    const rejected = new Set<string>();
    for (let i = 0; i < changes.length; i += CHUNK) {
      const res = await backend.records.sync(s.url, device(), false, READS, changes.slice(i, i + CHUNK));
      await applyResult(s, res, false);
      res.rejected.forEach((x) => rejected.add(id(x.c, x.key)));
    }
    const next: Record<string, number> = {};
    for (const [k, h] of Object.entries(hashes)) if (!rejected.has(k)) next[k] = h;
    saveSent(s, next, "visits");
    note(s, `uploaded ${changes.length - rejected.size} visit changes`);
  });
}

/** Deletions arrive as "#<key id>": find which local record each one means. */
async function resolveDeletions(cache: Cache, changes: RecOut[]): Promise<{ c: string; key: string; value: unknown; deleted: boolean }[]> {
  const gone = changes.filter((c) => c.deleted && c.key.startsWith("#"));
  if (!gone.length) return changes;
  const collections = new Set(gone.map((g) => g.c));
  const local = [...cacheRecords(cache).values()].filter((r) => collections.has(r.c));
  const ids = await backend.records.keyIds(local.map((r) => [r.c, r.key]));
  const byId = new Map(ids.map((kid, i) => [`${local[i].c}\u0000${kid}`, local[i].key]));
  return changes.flatMap((c) => {
    if (!(c.deleted && c.key.startsWith("#"))) return [c];
    const key = byId.get(`${c.c}\u0000${c.key}`);
    return key ? [{ ...c, key }] : [];
  });
}

function schedulePush(s: ShareServer, ms = PUSH_DELAY_MS) {
  const r = run(s);
  clearTimeout(r.pushTimer);
  r.pushTimer = setTimeout(() => void push(s), ms);
}

function schedulePull(s: ShareServer, ms = PULL_DELAY_MS) {
  const r = run(s);
  clearTimeout(r.pullTimer);
  r.pullTimer = setTimeout(() => void pull(s), ms);
}

/** Take or renew the lease on every server; pull where we don't hold it. */
export async function leaseRound() {
  const st = getState();
  for (const s of recordServers()) {
    const r = run(s);
    // Logged in to VRChat (not the cache: a follower's cache comes from the server).
    if (st.serverOnly || !st.session?.logged_in) {
      schedulePull(s, 0);
      continue;
    }
    try {
      const was = r.held;
      const l = await backend.records.lease(s.url, device(), PRIORITY);
      r.held = l.held;
      if (l.held && !was) {
        // Just got it: another device may have written meanwhile, so upload everything once.
        saveSent(s, null);
        note(s, "took the lease");
        if (getState().following === s.url) {
          // Was following: fetch from VRChat again before speaking for the account.
          setState({ following: null });
          void syncAll();
        }
        schedulePush(s, 0);
      } else if (!l.held && was) {
        note(s, `lease now with ${l.holder ?? "another device"}`);
      }
      if (!l.held && getState().following !== s.url && l.holder && l.holder !== device()) {
        // Another of your devices fetches for the account: read from it instead.
        note(s, `following ${l.holder}`);
        setState({ following: s.url });
        void pull(s, true);
      } else schedulePull(s, 0); // the holder too: other clients' visits
    } catch (e) {
      note(s, `lease failed: ${e}`);
    }
  }
}

export function startRecordSync() {
  if (leaseTimer) return;
  void leaseRound();
  leaseTimer = setInterval(() => void leaseRound(), LEASE_EVERY_MS);
  lastCache = getState().cache;
  let lastReport = getState().logs.report;
  for (const s of recordServers()) setTimeout(() => void pushVisits(s), 3_000);
  unsubscribe = subscribeState(() => {
    const st = getState();
    if (st.cache !== lastCache) {
      lastCache = st.cache;
      for (const s of recordServers()) if (run(s).held) schedulePush(s);
    }
    // A log scan finished (new visits, or old ones expired): upload this device's visits.
    // (Only a real scan: summaries refreshed after taking in other clients' visits don't count.)
    if (st.logs.report !== lastReport) {
      lastReport = st.logs.report;
      if (!st.logs.report?.files_seen) return;
      for (const s of recordServers()) setTimeout(() => void pushVisits(s), 2_000);
    }
  });
  // Without a VRChat login the sharing code (which keeps the servers' live streams
  // open) doesn't run, so open them here.
  if (getState().serverOnly) void backend.share.watch(recordServers().map((s) => s.url)).catch(() => {});
  void backend.records
    .onChanged((e) => {
      const s = recordServers().find((x) => x.url === e.url);
      if (s) schedulePull(s);
    })
    .then((u) => (unlisten = u));
}

export async function stopRecordSync() {
  if (getState().following) setState({ following: null });
  if (leaseTimer) clearInterval(leaseTimer);
  leaseTimer = null;
  unsubscribe?.();
  unsubscribe = null;
  unlisten?.();
  unlisten = null;
  if (getState().serverOnly) void backend.share.watch([]).catch(() => {});
  const servers = getState().settings.sharing.servers;
  for (const [sid, r] of runs) {
    clearTimeout(r.pushTimer);
    clearTimeout(r.pullTimer);
    const s = servers.find((x) => x.id === sid);
    if (r.held && s) await backend.records.lease(s.url, device(), PRIORITY, true).catch(() => {});
  }
  runs.clear();
}

/** Sync records with this server right now (the "Sync now" button). */
export async function syncRecordsNow(s: ShareServer) {
  if (run(s).held) await push(s);
  await pushVisits(s);
  await pull(s);
}

// ---------- using the app without a VRChat login ----------

/** Read everything from a server, as the account in the pasted key ("Copy key to another device"). */
export async function startServerOnly(keyText: string, url: string): Promise<void> {
  const me = await backend.serverOnly.start(keyText);
  // The server's own form of the address, to find it if it's already in the list.
  const info = await backend.share.info(url);
  const s = getState().settings.sharing.servers.find((x) => x.url === info.url) ?? (await addServer(url));
  if (!s.info?.recordSync) throw new Error("That server can't sync all your data yet (it needs a newer vfm-server).");
  // Read only: nothing of this device is shared from here.
  updateServer(s.id, { syncAll: true, upload: false, download: false });
  setState({ serverOnly: me.userId, mode: "app", loginBusy: false, loginMessage: "" });
}

/** Back to the login screen; the data read so far stays until you log in or delete it. */
export async function stopServerOnly(): Promise<void> {
  await stopRecordSync();
  await backend.serverOnly.stop();
  setState({ serverOnly: null, mode: "login", selectedId: null, settingsOpen: false, loginMessage: "No longer reading from a server." });
}
