import { beforeEach, describe, expect, it, vi } from "vitest";

const sync = vi.fn();
const remoteApply = vi.fn();
const own = vi.fn();
vi.mock("../backend", () => ({
  isMobile: false,
  isTauri: false,
  backend: {
    records: {
      lease: async () => ({ held: false, expires: 0 }),
      sync: (...a: unknown[]) => sync(...a),
      keyIds: async (items: [string, string][]) => items.map(([, k]) => (k === "quest/wrld_a|1000" ? "#gone" : "#other")),
      onChanged: async () => () => {},
    },
    share: { watch: async () => {} },
    storeWrite: async () => {},
    logsOwnVisits: () => own(),
    logsRemoteApply: (...a: unknown[]) => remoteApply(...a),
    logsRemoteKeys: async () => [["quest", "wrld_a|1000"]],
    logsRemoteClear: async () => {},
    logsInfo: async () => null,
    logsStats: async () => ({}),
  },
}));
vi.mock("../sync", () => ({ syncAll: () => {} }));

import { getState, setState, updateLocal, updateSettings } from "../store";
import { syncRecordsNow } from "../recsync";
import type { ShareServer } from "../types";

const server = { id: "s2", url: "https://vfm.example", syncAll: true, registered: true, keyChanged: false, info: { recordSync: true } } as unknown as ShareServer;
const result = (changes: unknown[] = []) => ({ changes, rejected: [], reset: false, writeMax: {}, seq: 1, unreadable: 0 });
const rec = (source: string, key: string, deleted = false) => ({ c: "visits", key, value: deleted ? null : { visit: { world: "wrld_a" } }, deleted, source, observedAt: 1, hlc: "", seq: 1 });

describe("visits from VRChat clients", () => {
  beforeEach(() => {
    sync.mockReset().mockResolvedValue(result());
    remoteApply.mockReset().mockResolvedValue(undefined);
    own.mockReset().mockResolvedValue([]);
    updateSettings({ sharing: { ...getState().settings.sharing, servers: [server] } });
    updateLocal((l) => ({ ...l, deviceId: "pc" }));
    setState({ serverOnly: null, following: null });
  });

  it("uploads this device's visits under its device ID, only once", async () => {
    own.mockResolvedValue([["wrld_a|1000", { visit: { world: "wrld_a", start: 1000 } }]]);
    await syncRecordsNow(server);
    const sent = sync.mock.calls.flatMap((c) => c[4] as { c: string; key: string }[]);
    expect(sent).toEqual([expect.objectContaining({ c: "visits", key: "pc/wrld_a|1000" })]);
    sync.mockClear();
    await syncRecordsNow(server);
    expect(sync.mock.calls.flatMap((c) => c[4] as unknown[])).toEqual([]);
  });

  it("stores other clients' visits under that client, ignores its own", async () => {
    sync.mockResolvedValue(result([rec("quest", "quest/wrld_a|1000"), rec("pc", "pc/wrld_b|5")]));
    await syncRecordsNow(server);
    expect(remoteApply).toHaveBeenCalledWith("quest", [["wrld_a|1000", { visit: { world: "wrld_a" } }]], []);
    expect(remoteApply.mock.calls.some((c) => c[0] === "pc")).toBe(false);
  });

  it("matches deletions against stored visits", async () => {
    sync.mockResolvedValue(result([rec("quest", "#gone", true)]));
    await syncRecordsNow(server);
    expect(remoteApply).toHaveBeenCalledWith("quest", [], ["wrld_a|1000"]);
  });
});
