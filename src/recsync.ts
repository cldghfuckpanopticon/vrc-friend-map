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
// - Other logged-in devices keep fetching from VRChat themselves for now.

import { backend, isMobile, type RecOut } from "./backend";
import { emptyCache, getState, setState, subscribeState, updateCache, updateServer } from "./store";
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

const sentKey = (s: ShareServer) => `vfm-recsync-sent:${s.id}`;
function loadSent(s: ShareServer): Record<string, number> {
  try {
    return JSON.parse(localStorage.getItem(sentKey(s)) ?? "{}");
  } catch {
    return {};
  }
}
function saveSent(s: ShareServer, v: Record<string, number> | null) {
  try {
    if (v) localStorage.setItem(sentKey(s), JSON.stringify(v));
    else localStorage.removeItem(sentKey(s));
  } catch {
    /* only costs a full upload next time */
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

/** Fetch what changed; a device without a VRChat login applies it. */
async function pull(s: ShareServer) {
  await exclusive(s, async () => {
    const st = getState();
    const res = await backend.records.sync(s.url, device(), false, READS, []);
    if (!st.serverOnly) return; // logged-in devices fetch from VRChat themselves (for now)
    const api = res.changes.filter((c) => API_COLLECTIONS.includes(c.c));
    const resolved = await resolveDeletions(res.reset ? emptyCache() : getState().cache, api);
    updateCache((c) => applyRecords(res.reset ? { ...emptyCache(), linkReads: c.linkReads, ageHints: c.ageHints } : c, resolved));
    if (res.unreadable) note(s, `${res.unreadable} records couldn't be opened (another key?)`);
    if (api.length) setState({ recordsAt: Date.now() });
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
async function leaseRound() {
  const st = getState();
  for (const s of recordServers()) {
    const r = run(s);
    if (st.serverOnly || !st.cache.me) {
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
        schedulePush(s, 0);
      } else if (!l.held && was) {
        note(s, `lease now with ${l.holder ?? "another device"}`);
      }
      if (!l.held) schedulePull(s, 0);
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
  unsubscribe = subscribeState(() => {
    const c = getState().cache;
    if (c === lastCache) return;
    lastCache = c;
    for (const s of recordServers()) if (run(s).held) schedulePush(s);
  });
  // Without a VRChat login the sharing code (which keeps the servers' live streams
  // open) doesn't run, so open them here.
  if (getState().serverOnly) void backend.share.watch(recordServers().map((s) => s.url)).catch(() => {});
  void backend.records
    .onChanged((e) => {
      const s = recordServers().find((x) => x.url === e.url);
      if (s && !run(s).held) schedulePull(s);
    })
    .then((u) => (unlisten = u));
}

export async function stopRecordSync() {
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
