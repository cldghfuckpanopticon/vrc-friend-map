import { beforeEach, describe, expect, it, vi } from "vitest";

const lease = vi.fn();
const sync = vi.fn();
vi.mock("../backend", () => ({
  isMobile: false,
  isTauri: false,
  backend: {
    records: { lease: (...a: unknown[]) => lease(...a), sync: (...a: unknown[]) => sync(...a), keyIds: async () => [], onChanged: async () => () => {} },
    share: { watch: async () => {} },
    storeWrite: async () => {},
  },
}));
const syncAll = vi.fn();
vi.mock("../sync", () => ({ syncAll: () => syncAll() }));

import { getState, setState, updateSettings } from "../store";
import { leaseRound } from "../recsync";
import type { ShareServer } from "../types";

const server = { id: "s1", url: "https://vfm.example", syncAll: true, registered: true, keyChanged: false, info: { recordSync: true } } as unknown as ShareServer;
const empty = { changes: [], rejected: [], reset: false, writeMax: {}, seq: 0, unreadable: 0 };
const flush = () => new Promise((r) => setTimeout(r, 0));

describe("lease: hold, follow, take over", () => {
  beforeEach(() => {
    lease.mockReset();
    sync.mockReset().mockResolvedValue(empty);
    syncAll.mockReset();
    updateSettings({ sharing: { ...getState().settings.sharing, servers: [server] } });
    setState({ serverOnly: null, following: null, session: { logged_in: true, user_id: "usr_me", display_name: "Me" }, cache: { ...getState().cache, me: { id: "usr_me" } as never, friends: { usr_a: { id: "usr_a" } as never } } });
  });

  it("follows when another device holds it, and takes over (fetching again) when it's free", async () => {
    lease.mockResolvedValueOnce({ held: false, holder: "pc", expires: 1 });
    await leaseRound();
    await flush();
    expect(getState().following).toBe(server.url);
    // Entering follow mode fetches everything (full = true)…
    expect(sync.mock.calls.some((c) => c[2] === true)).toBe(true);
    // …but the holder hasn't uploaded yet: this device's own data stays.
    expect(getState().cache.friends.usr_a).toBeDefined();

    lease.mockResolvedValueOnce({ held: true, holder: "me", expires: 2 });
    await leaseRound();
    expect(getState().following).toBeNull();
    expect(syncAll).toHaveBeenCalledTimes(1);
  });

  it("following: the holder's data replaces this device's copy", async () => {
    sync.mockResolvedValue({
      ...empty,
      changes: [
        { c: "me", key: "me", value: { id: "usr_me" }, deleted: false, source: "pc", observedAt: 1, hlc: "", seq: 1 },
        { c: "friends", key: "usr_b", value: { id: "usr_b" }, deleted: false, source: "pc", observedAt: 1, hlc: "", seq: 2 },
      ],
    });
    lease.mockResolvedValueOnce({ held: false, holder: "pc", expires: 1 });
    await leaseRound();
    await flush();
    expect(Object.keys(getState().cache.friends)).toEqual(["usr_b"]);
  });

  it("a device without a VRChat login never takes the lease", async () => {
    setState({ serverOnly: "usr_me" });
    await leaseRound();
    expect(lease).not.toHaveBeenCalled();
  });
});
