import { useApp } from "../store";
import { stopServerOnly } from "../recsync";

function ago(ms: number): string {
  if (!ms) return "not yet";
  const s = Math.round((Date.now() - ms) / 1000);
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  return `${Math.round(s / 3600)} h ago`;
}

/** Shown while this device reads everything from a server, without a VRChat login. */
export function ServerOnlyBar() {
  const serverOnly = useApp((s) => s.serverOnly);
  const following = useApp((s) => s.following);
  const at = useApp((s) => s.recordsAt);
  const server = useApp((s) => s.settings.sharing.servers.find((x) => (following ? x.url === following : x.syncAll)));
  if (following && !serverOnly) {
    return (
      <div className="notice" role="status">
        <span>
          Another of your devices fetches your VRChat data and shares it through {server?.info?.name || server?.url || "your server"}; this one
          reads it from there (last synced {ago(at)}). It takes over by itself when that device goes offline.
        </span>
      </div>
    );
  }
  if (!serverOnly) return null;
  return (
    <div className="notice" role="status">
      <span>
        Reading from {server?.info?.name || server?.url || "your server"}, no VRChat login on this device (last synced {ago(at)}). VRChat
        actions like invites aren't available here.
      </span>
      <button className="btn ghost" onClick={() => void stopServerOnly()}>
        Stop
      </button>
    </div>
  );
}
