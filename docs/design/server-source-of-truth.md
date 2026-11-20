# Design: the server as sync hub or as agent, and keeping its VRChat login safe

Status: proposal, for discussion. Nothing here is implemented yet.

Two questions:

1. **What does a server mean for one person's data?** Either the person's app is the source
   (it talks to VRChat, the server stores and forwards), or the server is (it talks to VRChat
   on the person's behalf, the app mostly reads). In both cases every device of that person
   on that server should see the same data.
2. **If the server talks to VRChat for someone, how does it keep that login** across restarts
   without simply leaving it on disk, given VRChat offers no OAuth, only a session cookie?

## Summary

- Make the source a property of **each data stream**, not of the whole server: "friends and
  locations come from the server's agent, game-log visits come from my PC". A single switch
  can't express that the PC's game logs are always richer than the API, while the server's
  connection is the only one that runs 24/7.
- Add one sync mechanism used in both cases: every piece of data is a **record** in a
  collection (friends, profiles, visits, circles, …), versioned by a per-account sequence
  number, so devices only send and fetch records that changed (see "Data schema and sync"). When the app is the source,
  the streams are end-to-end encrypted for the person's own key (like `self` items today). When
  the server's agent is the source, it writes them, so the server can read them; they're
  encrypted at rest.
- Generalize today's hand-off into a **live-connection lease**: per VRChat account, exactly one
  producer holds VRChat's live connection at a time, so VRChat never sees the account twice.
  With an agent set up, the agent always holds it; without, whoever is live (PC first).
- Any account on a server can have an agent; the owner isn't special (decided).
- **Never store the password or a TOTP secret.** Store only VRChat's session cookies. Encrypt
  them at rest under a data key that is *not* on the server's disk: by default it's held by
  the account's own devices, which unlock its agent automatically when one of them connects after a
  restart. Optional other key holders for unattended restarts: TPM / systemd credentials on bare
  metal, or a KMS / Kubernetes Secret.
- In memory, keep the session in a small **separate agent process** that holds it in
  `memfd_secret` memory, with core dumps and ptrace turned off, no swap and a tight seccomp
  filter. The internet-facing HTTP server never holds the cookie. All of this was checked on
  Linux 6.18 (below).

## What exists today

| Piece | Where | What it does |
|---|---|---|
| Exchange | `server/src/api.rs`, `crates/vfm-share` | Items of kind `friends`, `activity`, `self`. Signed, optionally end-to-end encrypted (X25519 + XChaCha20-Poly1305) per recipient; the server stores ciphertext. `self` carries circles, notes and settings to your own devices. |
| Live notices | `GET /v1/events` (SSE) | Tells apps that something changed (inbox, device signals). |
| Logger | `server/src/logger.rs` | One owner per server. Holds a VRChat session, keeps VRChat's pipeline open, counts time together, pushes invites to phones. |
| Logger login | `POST /v1/logger/pair` (sealed from the app) or the admin page (`/admin/api/vrchat/login` + `2fa`) | The password is used once and never stored. Only the session cookies are kept. |
| Session at rest | `vfm-data/logger/session.json` | **Plain JSON, file mode 0600.** (TODO.md "Round 6" says "stored encrypted": only the transport from the app is sealed.) |
| Hand-off | `[logger] hand_off`, `/v1/logger/heartbeat` | While the PC app runs, the server lets go of the pipeline. |
| App's own VRChat data | `src-tauri/src/api.rs`, `pipeline.rs`, `gamelogs.rs`, `livelog.rs` | Friends, profiles, mutuals, favorites, groups, instances via the API; live events via the pipeline; game logs on the PC. All kept locally. |

What a server can't do today: give a second device the friends, profiles and history that the
first device fetched. Each device fetches everything from VRChat itself and needs its own
VRChat login. The logger's history is the only thing that flows back (time-together totals).

## Data: who can produce what

| Stream | From VRChat's API / pipeline (app or server) | Only from the PC's game logs / OSC | Written by the person |
|---|---|---|---|
| Friends list, status, location, online/offline | ✔ | | |
| Profiles, bios, mutuals, favorites, groups | ✔ (rate limited) | | |
| Invites and notifications | ✔ | | |
| Time together | ✔ partly: friends in public/friends+ instances only | ✔ fully: private and invite-only instances, non-friends, exact join/leave | |
| Instance visits, players present, videos, screenshots, avatar switches | | ✔ | |
| Avatar, AFK, muted (OSC) | | ✔ | |
| Circles, notes, settings | | | ✔ (already synced as `self`) |

So yes, **game logs stay richer than the API** for anything about instances: VRChat hides
private and invite-only instances from friends' locations, and only the log knows who was in
the room with you. A full hand-off to the server is possible for everything in the first
column. The PC remains a producer for the second column whenever VRChat runs on it.

## Part 1: client-sourced vs server-sourced

### Terms

- **Account**: one VRChat account (`usr_…`). A person may have several devices on one server
  (same app key today, see "Several devices" in `docs/server-setup.md`).
- **Producer**: whatever writes a stream: a device of the account, or the server's agent for
  that account.
- **Stream**: one kind of data for one account (`friends`, `profiles`, `notifications`,
  `visits`, `together`, …), as an append-only log of deltas plus periodic snapshots, with a
  per-stream sequence number.

### Mode A: the app is the source ("sync hub")

```text
 PC app ── VRChat API/pipeline ──┐
   │  game logs, OSC             │
   └─► server: streams (E2E, for own key) ─► phone, laptop (read, merge)
```

- The device holding the live-connection lease talks to VRChat and writes the streams.
- Streams are sealed for the account's own key, exactly like `self` items. The server sees
  sizes and times only.
- Other devices of the same account read the streams and need **no VRChat login** to show
  friends, profiles and history. To *do* something on VRChat (invite, friend request), they
  either have their own login, or ask a producer device to do it (relayed device signal), or
  use mode B.
- No credentials on the server. If the PC is off, data stops updating (the phone shows "as of
  18:42").

### Mode B: the server's agent is the source ("agent")

```text
 server agent ── VRChat API/pipeline (session stored on the server)
   │
   └─► streams (written by the server, encrypted at rest) ─► all devices (read)
 PC app ── game logs, OSC ─► streams (E2E) ─► all devices
```

- The agent keeps VRChat's live connection 24/7 and runs the periodic refreshes (friends
  list, profiles that went stale, mutuals), following the existing rate rules (`docs/privacy.md`).
- Devices read from the server and send writes (invite, favorite, …) to the agent, which checks
  them against the same allow-list the app uses (`src-tauri/src/api.rs` write table) and
  spaces them.
- The PC still produces the game-log streams; those stay end-to-end encrypted, and devices merge
  them with the agent's streams. The server never needs to read them.
- This is TODO.md "Round 9: use the app without logging in to VRChat in it", for every account
  on the server, not only the owner.

### Per-stream source, and the lease

VRChat should see **one** live connection per account. The server hands out a **lease** per
account (`POST /v1/lease`, one minute, renewed every minute, given up on exit). Only the holder
talks to VRChat for the account's VRChat data and writes it; everyone else reads. A holder that
stops renewing loses it, and the next one takes over. This is today's heartbeat hand-off made
explicit and extended to phones.

| Stream group | Who may hold the lease |
|---|---|
| VRChat data (friends, presence, profiles, notifications, …) | With an agent set up: **the agent, always** (devices only read). Without: **whoever is live** (PC first, then a phone). |
| Game logs and OSC | Not leased: only a PC running VRChat can produce them, and each PC writes its own visits. |

Why "the agent, always" once there is one: it sees exactly the same API data the PC would,
it's on 24/7, and one fixed producer means no handovers, no gaps and nothing to merge. The
PC app then doesn't talk to VRChat for this data at all.

## Data schema and sync

Goal: sync *everything* the app knows, across all of an account's devices, without sending
whole files. A full `cache.json` for 300 friends is megabytes; resending it on every change would
be the "16 MB a minute" case. Instead the unit of sync is one **record**, and a device only
sends and fetches records that changed since it last synced.

### Records and collections

Every piece of synced data is a record: `(collection, key) → value`. The collections map
directly onto what the app stores today:

| Class | Collections (app source today) | Writer | Merge |
|---|---|---|---|
| **VRChat** | `me`; `friends`, `presence` (by user); `profiles` (`Cache.details`); `mutuals`; `favorites`; `groups`; `worlds`; `others`; `moderations`; `userNotes`; `notifications`; `alts` | lease holder | single writer: newest wins |
| **Game log** | `visits` (`history.json`: one record per instance visit, with players, videos, screenshots, avatar switches); `live` (who's here now, one record per PC) | that PC | single writer per PC; visits from several PCs are separate records |
| **Yours** | `circles`, `imports`, `nicknames`, `starred`, `timezones`, `worldNotes`, `inviteTemplates`, `blockReasons`, `shareNotes`, `sharedCircles`, `dismissedGroups`, `slotWatch`, `pushTargets`, settings groups | any device | per field, newest wins (hybrid logical clock); deletes are tombstones |
| **Logs** | `friendLog`, `activity` | lease holder | append-only, union by entry ID |
| **Never synced** | `ageNotes`, `ageHints`, `linkReads`, this device's push registration, graphics / frame rate / tray | n/a | stays on the device, as today |

Derived data isn't synced: time together, suggested groups and stats are recomputed on each
device from `visits`, `presence` history and `friendLog`. That keeps the synced set smaller and
avoids two sources disagreeing.

### Versioning

- The server keeps one **sequence number per account**. Every accepted write gets the next one.
- Each stored record keeps the sequence of its last change; a deleted record stays as a small
  tombstone (key + sequence) for 90 days.
- A device remembers one number: the highest sequence it has. "What changed?" is then
  "records of this account with sequence > N", answered from an index, with no per-device
  state on the server and no unbounded change log.
- A device that has been away longer than tombstones are kept does a full fetch instead.
- Each record also carries `observed_at` (when the producer saw it) and `source` (which device,
  or the agent), so the app can say "from your server, 2 min ago".

### One request does both directions

```text
POST /v1/sync            (signed like every request today)
  { "since": 41873,
    "lease": "…",                       // only from the lease holder, for VRChat collections
    "changes": [ {c, k, v | deleted, hlc, observed_at}, … ] }
→ { "seq": 41911, "changes": [ … everything after 41873, minus what this device just sent … ],
    "more": false }
```

- Batched and compressed: CBOR or MessagePack, then zstd. A first sync is paginated (`more:
  true`).
- `/v1/events` (already there) sends `sync` with the new sequence number, so devices fetch
  right away; no polling.
- Writes are coalesced on the producer: presence and location changes go out every few seconds
  as one batch, not one request per pipeline event.
- Pictures aren't synced: records hold VRChat's image URLs, as the cache does today.

### What this costs

Rough numbers for 300 friends:

| Situation | Data |
|---|---|
| First sync of a new device | everything once: about the size of today's `cache.json` + `history.json`, compressed (typically a few MB) |
| An evening of friends moving between worlds | ~1 presence change per friend per 10 minutes ≈ 0.5 changes/s ≈ a few KB per minute |
| A full friends-list refresh (every 30 min) where nothing changed | nothing: the producer only sends records whose content changed (it compares hashes) |
| Your phone catching up after a day offline | only records changed that day |

### Encryption of records

- **Device-written records** (game logs, your data, and VRChat data while a device holds the
  lease) are **end-to-end encrypted** per record with a key derived from the account's app key
  (your devices share that key already). The server stores ciphertext.
- **Keys are hidden too**: the server sees `HMAC(account key, "friends/usr_…")`, not the user
  ID, so it doesn't learn who your friends are from the record names.
- **Agent-written records** can't be end-to-end encrypted, because the server produces them.
  They're encrypted at rest with the account's data key (Part 2), so a copy of the disk is
  useless; the running server can read them, as it can read the VRChat session anyway.
- Changing the lease from a device to the agent re-writes the VRChat collections in the agent's
  encryption; the device's ciphertext versions become tombstones.

### Server storage

Today the server keeps JSON files per user. Record sync needs an index by `(account,
sequence)`; an embedded database fits: **redb** (pure Rust, single file, keeps the static
musl build and "one file, no dependencies") or SQLite (bundled; adds C code). Suggested: redb.

### Agents for every account

Any account on a server can set up an agent; the server owner's account is not special
(decided). That means:

- Per account: its own data key, its own key slots (Part 2), its own agent process state and
  lease. One account's devices unlock only that account's agent.
- The admin page shows that an account has an agent, its state (locked / running / expired)
  and lets the admin stop or remove it, but never shows or exports the session.
- **The operator problem stays**: whoever runs the server can, with enough effort, get at every
  agent's session while it runs (Part 2, threat model). So the app shows a plain warning before
  you hand a session to a server: "Whoever runs this server can control your VRChat account.
  Only do this on a server run by you or someone you'd give your password to."
- Limits: `[agent] max_agents` and a per-server cap on VRChat requests. Many accounts on one IP
  look unusual to VRChat and may trigger its security checks; the admin page should say how many
  agents run there.

## Part 2: keeping the VRChat login

### What there is to store

VRChat has no OAuth and no scoped tokens. Logging in takes username + password + 2FA (TOTP,
email code or recovery code). The result is two cookies: `auth` (the session) and
`twoFactorAuth` (proof that 2FA passed). Whoever holds them **is** the account, until VRChat
ends the session or the password changes.

| Secret | Gives an attacker | Recommendation |
|---|---|---|
| Password | The account until it's changed; also every other site where it's reused | **Never stored.** Used once to log in, then zeroized. |
| TOTP secret | Every future 2FA code, forever | **Never stored.** Storing it plus the password would let the server re-login unattended, but whoever gets the server then owns the account for good. |
| Session cookies (`auth`, `twoFactorAuth`) | The account until the session ends or the password changes | **Stored, encrypted** (below). When VRChat ends the session, the account holder logs in again (TODO.md "Round 9": expiry notice, renew from the app). |

Recovery if the server is compromised: change the VRChat password, which should end other
sessions (to verify; TODO.md "Log out everywhere" asks the same question), and the app should
say so.

### Threat model

| Threat | Example | Defended by |
|---|---|---|
| Data at rest leaks | Backup, PVC snapshot, stolen SD card, `docker cp` of the volume | Session encrypted with a data key that isn't on that disk |
| Bug in the internet-facing server | A request-parsing bug gives an attacker code execution in the HTTP process | The HTTP process never holds the session; a separate agent process does |
| Other software on the host, same or other user | Something reading `/proc/<pid>/mem`, core dumps, swap | `memfd_secret`, `PR_SET_DUMPABLE=0`, no core dumps, `mlock` / no swap |
| Root on the host, or a container escape with ptrace | Attaches and injects code into the agent | **Not fully defensible** while the agent must use the session; makes it harder and noisier |
| The server's operator, if that's not you | A friend running the server | **Not defensible.** Hence: agents only for people who trust the operator like themselves |

### Tier 1: at rest (survives restarts)

Envelope encryption, like LUKS key slots:

- A random 256-bit **data key (DEK)** per account encrypts the session (XChaCha20-Poly1305,
  already used in `vfm-share`; associated data binds it to the account and server key).
- The DEK is never written in the clear. It's stored **wrapped** by one or more **key
  encryption keys (KEKs)**, each a "slot" that can unlock it. The server needs any one slot.

| KEK slot | Unattended restart | Disk/backup theft | Fits | Notes |
|---|---|---|---|---|
| **The account's devices** (recommended default) | No, but it unlocks itself as soon as any of the account's devices connects (seconds, no typing) | ✔ | Everything | The DEK is sealed to the account's app key (X25519, like `self` items). After a restart that account's agent is "locked": it waits; the next authenticated request from one of the account's devices carries the sealed DEK back (`POST /v1/agent/unlock`). The phone with the app in the background is usually enough. |
| **Passphrase** | No (type it on the admin page after a restart) | ✔ if strong | Everything | Argon2id-derived KEK. Fallback when no device is around. |
| **systemd credentials / TPM2** | ✔ | ✔ (bound to that machine's TPM) | Bare metal, systemd | `systemd-creds encrypt --with-key=tpm2` and `LoadCredentialEncrypted=`; the service gets the KEK in `$CREDENTIALS_DIRECTORY` (non-swappable ramfs, only that service). |
| **Kubernetes Secret / KMS** | ✔ | ✔ only if the Secret store is protected separately (etcd encryption with KMS, External Secrets / OpenBao) | Kubernetes | KEK from an env var or mounted file; a PVC snapshot alone then isn't enough. Equally good: OpenBao/Vault transit, so the KEK never reaches the pod. |
| **A key file next to the data** | ✔ | ✘ | — | Equivalent to today's plaintext. Only as an explicit "I accept this" choice. |

Defaults, per account: that account's devices + an optional passphrase, both managed in the
app (the admin page only sees "locked / unlocked"). A locked agent doesn't run; everything else
on the server does, and the app says why ("locked since restart: open the app on one of your
devices").

### Tier 2: in memory (while running)

Privilege separation, as OpenSSH does it:

```text
 vfm-server (HTTP, internet-facing)  ──unix socket, narrow protocol──►  vfm-agent (no listening socket)
   never holds the session                                                 holds DEK + session in secret memory
                                                                           talks to api.vrchat.cloud / pipeline only
```

- `vfm-agent` is the same binary started with a subcommand, as a child process. The socket
  protocol is a short list of calls: `unlock(sealed DEK)`, `login(username, password)` /
  `2fa(code)`, `vrchat(allow-listed request)`, `status`, `logout`. It never returns the
  cookies.
- Hardening of the agent process, all checked on Linux 6.18 in a container (see "Checked"):
  - **`memfd_secret(2)`** (Linux 5.14+; older kernels needed the `secretmem.enable` boot flag,
    6.18 works without it): the session and DEK live in pages removed from the kernel's direct
    map. Reads through `/proc/<pid>/mem` fail (checked), so debuggers, `gcore` and memory
    scrapers get nothing, even as root; `process_vm_readv` should fail the same way (same
    page-pinning path, not checked yet). Falls back to `mmap` + `mlock` where unavailable.
  - **`prctl(PR_SET_DUMPABLE, 0)`**: no core dumps, and processes of the same user can't
    ptrace it. Yama `ptrace_scope ≥ 1` adds to that (1 on the test machine; distro defaults
    vary).
  - **`mlock`** / `MADV_DONTDUMP` on any buffer that briefly holds the cookie (HTTP headers);
    zeroize after use (`zeroize` crate). `RLIMIT_CORE=0`.
  - **seccomp filter** after startup: only the syscalls for TLS sockets, the unix socket,
    memory and time; no `execve`, `ptrace`, `process_vm_writev`, file writes outside its
    data file.
  - Never logs headers; error messages from VRChat are passed on without request headers.
- Expected limit: root with `CAP_SYS_PTRACE` can still inject code into the agent and have it
  read its own secret memory. That takes a targeted attack on a live process, which is a much
  smaller window than "copy a file".
- The kernel keyring (`add_key`/`keyctl`) would hold secrets outside the process, but container
  runtimes block it by seccomp by default (`EPERM` in the check below), and keyrings aren't
  namespaced per container. Not used.
- Windows / macOS / Termux (the server runs there too): `VirtualLock` + `CryptProtectMemory` on
  Windows, `mlock` on macOS; no separation guarantees beyond the process boundary. The agent
  feature should say "best protected on Linux".

### Kubernetes specifics

- `memfd_secret` was allowed by this container's seccomp profile; whether the cluster's
  `RuntimeDefault` profile allows it must be checked (a one-line test pod). If not, a
  `Localhost` seccomp profile that adds it.
- `memfd_secret` and `mlock` count against `RLIMIT_MEMLOCK`; the session is a few hundred bytes,
  so one page is enough.
- KEK from a Secret only helps if etcd encryption at rest is on (or the Secret comes from an
  external store). Otherwise prefer the device slot.
- Nodes without swap (the Kubernetes default) remove the swap concern for everything else.

## Plan

1. **Session at rest, now**: encrypt `logger/session.json` with a DEK + device slot +
   passphrase slot; locked/unlocked state on the admin page and in the app. Fixes the plaintext
   file without changing anything else. Small.
2. **Agent process**: split the VRChat client out of the HTTP process; `memfd_secret`,
   dumpable 0, seccomp. Medium.
3. **Record sync + lease**: the schema, `/v1/sync`, redb storage, mode A for all of an
   account's devices (no credentials on the server), lease replacing the heartbeat hand-off.
   Start with the VRChat collections a phone without login needs (`me`, `friends`, `presence`,
   `profiles`, `worlds`), then everything else. Large.
4. **Mode B**: per-account agents write the VRChat collections; devices read them and send
   writes through the agent; the app can run with no VRChat login in it (Round 9, for every
   account). Large.
5. Expiry notice and renew-from-app (Round 9), using the same sealed-login path.

## Decisions and open questions

Decided (2026-11-20):

1. Agents for every account on a server, no distinction for the owner.
2. Sync everything, as records with per-account sequence numbers (above).

Open:

1. Key slots: are unattended slots (TPM / systemd credentials, Kubernetes Secret) allowed, and
   only behind an explicit "I accept that whoever controls this machine can unlock it"?
   Recommended default per account: your devices + an optional passphrase.
2. Lease: "the agent, always" once an agent exists, "whoever is live" otherwise (recommended),
   or should a running PC app still take over from the agent?
3. Server database: redb (pure Rust) or SQLite?

## Checked

On Linux 6.18 in a container (seccomp filter active, no `CAP_SYS_PTRACE`):

| Check | Result |
|---|---|
| `memfd_secret(0)` without `secretmem.enable` boot flag | works; `mmap` and writes work |
| Reading that memory through `/proc/self/mem` | **fails (`EIO`)**; the same read of a normal mapping returns the data |
| `mlock` (RLIMIT_MEMLOCK unlimited there) | works |
| `prctl(PR_SET_DUMPABLE, 0)` | works |
| `add_key` into the process keyring | **`EPERM`** (blocked by the container's seccomp profile) |
| Yama `ptrace_scope` | 1 |
