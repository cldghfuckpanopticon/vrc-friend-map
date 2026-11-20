import { describe, expect, it } from "vitest";
import { applyRecords, cacheRecords, diffRecords, hashValue } from "../recsync";
import { emptyCache } from "../store";
import type { Cache } from "../types";

function sample(): Cache {
  const c = emptyCache();
  c.me = { id: "usr_me", displayName: "Me" } as Cache["me"];
  c.friends = { usr_a: { id: "usr_a", displayName: "EchoMoth" } as never, usr_b: { id: "usr_b", displayName: "Maple" } as never };
  c.presence = { usr_a: "online", usr_b: "offline" };
  c.details = { usr_a: { bio: "hi" } as never };
  c.mutuals = { usr_a: { count: 3 } as never };
  c.favorites = { usr_a: ["Group 1"] };
  c.worlds = { wrld_1: { name: "Black Cat" } as never };
  c.moderations = { byUser: { usr_x: ["block"] }, fetchedAt: 123 };
  c.lastFriendsSync = 99;
  // Never synced:
  c.ageHints = { usr_a: { t: 1, profileAt: 1, hints: [] } };
  return c;
}

describe("record sync codecs", () => {
  it("round-trips the synced parts of the cache", () => {
    const src = sample();
    const recs = [...cacheRecords(src).values()].map((r) => ({ ...r, deleted: false }));
    const back = applyRecords(emptyCache(), recs);
    expect(back.me).toEqual(src.me);
    expect(back.friends).toEqual(src.friends);
    expect(back.presence).toEqual(src.presence);
    expect(back.details).toEqual(src.details);
    expect(back.mutuals).toEqual(src.mutuals);
    expect(back.favorites).toEqual(src.favorites);
    expect(back.worlds).toEqual(src.worlds);
    expect(back.moderations).toEqual(src.moderations);
    expect(back.lastFriendsSync).toBe(99);
    expect(back.ageHints).toBeUndefined();
  });

  it("keeps device-only data out of the records", () => {
    const keys = [...cacheRecords(sample()).values()].map((r) => r.c);
    expect(keys).not.toContain("ageHints");
    expect(keys).not.toContain("linkReads");
  });

  it("sends only what changed, and deletions", () => {
    const first = diffRecords(cacheRecords(sample()), {});
    expect(first.deletes).toEqual([]);
    expect(first.upserts.length).toBe(cacheRecords(sample()).size);

    const next = sample();
    next.presence = { ...next.presence, usr_a: "offline" };
    delete next.friends.usr_b;
    const d = diffRecords(cacheRecords(next), first.hashes);
    expect(d.upserts.map((r) => `${r.c}/${r.key}`)).toEqual(["presence/usr_a"]);
    expect(d.deletes.map((r) => `${r.c}/${r.key}`)).toEqual(["friends/usr_b"]);
    expect(diffRecords(cacheRecords(next), d.hashes).upserts).toEqual([]);
  });

  it("applies deletions and leaves untouched collections alone", () => {
    const c = sample();
    const after = applyRecords(c, [
      { c: "friends", key: "usr_b", value: null, deleted: true },
      { c: "presence", key: "usr_a", value: "busy", deleted: false },
      { c: "someNewCollection", key: "x", value: 1, deleted: false },
    ]);
    expect(Object.keys(after.friends)).toEqual(["usr_a"]);
    expect(after.presence.usr_a).toBe("busy");
    expect(after.details).toBe(c.details);
    expect(c.friends.usr_b).toBeDefined(); // the original isn't modified
  });

  it("hashes are stable and content-sensitive", () => {
    expect(hashValue({ a: 1 })).toBe(hashValue({ a: 1 }));
    expect(hashValue({ a: 1 })).not.toBe(hashValue({ a: 2 }));
  });
});
