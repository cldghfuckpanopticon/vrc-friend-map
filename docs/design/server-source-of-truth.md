# Design: the server as system of record, its VRChat agent, and keeping the login safe

Status: **agreed design** (decisions below, 2026-11-20). Nothing here is implemented yet.

Goal: the app as **headless** as possible. A server holds all of an account's data and every
device of that account sees the same thing; with an agent, no device needs to talk to VRChat
at all. Two parts:

1. **Who is the source of truth** for each piece of data, and how it reaches every device.
2. **How the server keeps a VRChat login** when it acts for someone, given VRChat offers no
   OAuth, only a session cookie.

## Summary

- **"Source of truth" is two things, kept apart:**
  - **System of record: always the server.** All data of an account lives there, under one
    change sequence per account. Every device reads only from the server, so all devices see the
    same thing.
  - **Authority: per fact, by richness.** Each record is an observation by one producer (the
    agent, a device's API connection, a PC's game log). Different producers write different
    records; the app combines them with a fixed order of which source wins for which fact
    (see "Authority").
- **With an agent** (the account gave the server a VRChat session): the agent produces
  everything VRChat's API can give, 24/7, from one stable IP. Devices never talk to VRChat. A PC
  running VRChat adds what only the game log knows.
- **Without an agent:** the client is the only source of truth for API data. The device holding
  the account's lease fetches it and uploads all of it, end-to-end encrypted; every other device
  of the account (a phone without a VRChat login) sees all of it.
- **Data is records**, in collections that mirror what the app stores today, with a versioned
  schema shared by app and server, per-account sequence numbers so devices only move what
  changed, stored in **SQLite**.
- **Agents for every account** on a server, behind a red, boxed warning that must be
  acknowledged.
- **Login storage:** never the password or a TOTP secret, only the session cookies, encrypted
  under a per-account data key. Unlocked by the account's own devices or a passphrase
  (attended, default); TPM / systemd credentials or a Kubernetes/KMS secret only as a reluctant,
  explicitly accepted opt-in. In memory, a separate hardened agent process (`memfd_secret`, no
  dumps or ptrace, seccomp).

## What exists today

| Piece | Where | What it does |
|---|---|---|
| Exchange | `server/src/api.rs`, `crates/vfm-share` | Items of kind `friends`, `activity`, `self`. Signed, optionally end-to-end encrypted (X25519 + XChaCha20-Poly1305) per recipient; the server stores ciphertext. `self` carries circles, notes and settings to your own devices. |
| Live notices | `GET /v1/events` (SSE) | Tells apps that something changed (inbox, device signals). |
| Logger | `server/src/logger.rs` | One owner per server. Holds a VRChat session, keeps VRChat's pipeline open, counts time together, pushes invites to phones. |
| Logger login | `POST /v1/logger/pair` (sealed from the app) or the admin page (`/admin/api/vrchat/login` + `2fa`) | The password is used once and never stored. Only the session cookies are kept. |
| Session at rest | `vfm-data/logger/session.json` | **Plain JSON, file mode 0600.** (TODO.md "Round 6" says "stored encrypted": only the transport from the app is sealed.) |
| Hand-off | `[logger] hand_off`, `/v1/logger/heartbeat` | While the PC app runs, the server lets go of the pipeline. |
| App's own VRChat data | `src-tauri/src/api.rs`, `pipeline.rs`, `gamelogs.rs`, `livelog.rs` | Friends, profiles, mutuals, favorites, groups, instances via the API; live events via the pipeline; game logs on the PC. All kept locally, as whole JSON files. |

What a server can't do today: give a second device the friends, profiles and history that the
first device fetched. Each device fetches everything from VRChat itself and needs its own
VRChat login. The logger's time-together totals are the only thing that flows back.

## Part 1: source of truth

### Who can observe what

| Fact | VRChat API / pipeline (agent or a device) | PC game log / OSC (only a PC running VRChat) | Written by the person (any device) |
|---|---|---|---|
| Friends list, profiles, mutuals, favorites, groups, worlds, notifications | ✔ | | |
| A friend's status and location | ✔, but private / invite-only instances are hidden | ✔ when they're in *your* instance, private included | |
| Your own location | ✔ | ✔ exact instance, private included | |
| Who was with you (non-friends too), join/leave times, videos, screenshots, avatar switches | | ✔ | |
| Avatar, AFK, muted (OSC) | | ✔ | |
| Circles, nicknames, notes, settings | | | ✔ |

The game log is richer for anything about instances; the API is the only source for
everything else and the only one that can run 24/7.

### System of record: the server

- Every record of an account lives on the server, under that account's change sequence.
- Devices read **only** from the server and write their observations **to** it. A device's
  local files become a cache of the server's state (plus the few things that never leave the
  device).
- So any device sees everything any producer of the account has seen: a phone shows the PC's
  game-log history, the agent's friends list, and the circles edited on a laptop.

### Producers, and one VRChat connection per account

| Producer | Writes | When |
|---|---|---|
| **Agent** (server, per account) | API facts | Whenever the account has an agent: always (it holds the account's lease permanently) |
| **A device's API connection** | API facts | Only without an agent: the device holding the account's **lease** |
| **A PC's game log / OSC** | Game-log facts | Whenever VRChat runs on that PC. Not leased: each PC writes its own records |
| **Any device** | The person's own data | When the person edits something |

The **lease** (`POST /v1/lease`, one minute, renewed every minute, given up on exit) makes sure
VRChat sees one live connection per account. With an agent, the agent keeps it and devices never
open a VRChat connection: one session, one stable IP, no handovers, no gaps. Without an agent,
the first live device takes it (PC first, then a phone) and hands over when it stops renewing.
This replaces today's heartbeat hand-off.

### Authority: which observation wins

Different producers **never write the same record**. The API's view of a friend's location is
`presence/usr_x` from the agent; the PC log's sighting of that friend is a `visits` record from
that PC. Storage therefore has no conflicts between sources; authority is applied when the app
combines records into what it shows:

| Shown | Built from | Rule |
|---|---|---|
| Friends list, profiles, mutuals, favorites, groups, worlds | API records | only source |
| A friend's current location | `presence` (API) + `live` (PC: who's in your instance now) | `live` wins while fresh (it sees private instances); otherwise `presence` |
| Your own current location | `me` (API) + `live` | `live` wins while VRChat runs on a PC |
| Who you were with, history of an instance | `visits` | only source |
| Time together | `presence` history (API, 24/7) + `visits` (PC) | `visits` wins for the time spans it covers (exact, private included); `presence` fills the rest |
| Circles, notes, settings | person records | newest change wins, per field |

Each record carries `source` and `observed_at`, so the app can also say where and when something
came from ("from your server, 2 min ago", "from your PC's game log").

Derived data (time together, suggested groups, stats) is computed on each device from these
records and is not synced, so two producers can never disagree about it.

## Data schema and sync

### Records and collections

Every synced piece of data is a record `(collection, key) → value`. Collections map onto what the
app stores today:

| Class | Collections (app source today) | Producer | Merge |
|---|---|---|---|
| **API** | `me`; `friends`, `presence` (by user); `profiles` (`Cache.details`); `mutuals`; `favorites`; `groups`; `worlds`; `others`; `moderations`; `userNotes`; `notifications`; `alts` | agent, or the lease holder | single producer: newest wins |
| **Game log** | `visits` (`history.json`: one record per instance visit, with players, videos, screenshots, avatar switches); `live` (who's here now, one record per PC) | that PC | each PC its own records |
| **Yours** | `circles`, `imports`, `nicknames`, `starred`, `timezones`, `worldNotes`, `inviteTemplates`, `blockReasons`, `shareNotes`, `sharedCircles`, `dismissedGroups`, `slotWatch`, `pushTargets`, settings groups | any device | per field, newest wins (hybrid logical clock); deletes are tombstones |
| **Logs** | `friendLog`, `activity` | agent, or the lease holder | append-only, union by entry ID |
| **Never synced** | `ageNotes`, `ageHints`, `linkReads`, this device's push registration, graphics / frame rate / tray | n/a | stays on the device, as today |

Pictures aren't synced: records hold VRChat's image URLs, as the cache does today.

### Schema versioning

- **Per collection, a schema version.** Every record carries `v`. The shared crate
  (`crates/vfm-share`) defines one type per collection and version plus upgrade functions,
  used by both the app and the server, so both agree on what "`profiles` v3" is.
- **Forward compatible.** Readers keep fields they don't know and write them back, so an older
  app never strips a newer app's data.
- **Upgrades happen where the data can be read**: the apps upgrade end-to-end encrypted records
  (re-written lazily, on next change); the server upgrades agent records and its own tables.
- **Protocol version** in `/v1/info` (already `protocol: 1`) changes only for breaking changes.
  The server states the oldest app version it works with and vice versa, and the app says "this
  server needs a newer app" instead of failing (TODO.md "Compatibility").
- **The server's database** has its own migration version (SQLite `user_version`).

### Change tracking

- One **sequence number per account**; every accepted write gets the next one.
- Each record keeps the sequence of its last change; a deleted record stays as a tombstone for
  90 days.
- A device remembers one number: the highest sequence it has. "What changed?" is "records of
  this account with sequence > N", from an index; no per-device state on the server and no
  unbounded change log. A device away longer than tombstones are kept does a full fetch.

### One request does both directions

```text
POST /v1/sync            (signed like every request today)
  { "since": 41873,
    "lease": "…",                       // only from the lease holder, for API collections
    "changes": [ {c, k, v, schema, source, observed_at, hlc, deleted?, payload}, … ] }
→ { "seq": 41911, "changes": [ … everything after 41873, minus what this device just sent … ],
    "more": false }
```

- Batched and compressed: CBOR, then zstd. A first sync is paginated (`more: true`).
- `/v1/events` sends `sync` with the new sequence number, so devices fetch right away.
- Producers send only records whose content changed (they compare hashes) and coalesce
  presence/location changes into one batch every few seconds.
- The server rejects writes to API collections from anyone but the lease holder, and writes to
  another PC's game-log records.

### What it costs

Rough numbers for 300 friends:

| Situation | Data |
|---|---|
| First sync of a new device | everything once: about today's `cache.json` + `history.json`, compressed (typically a few MB) |
| An evening of friends moving between worlds | ~0.5 presence changes/s ≈ a few KB per minute |
| A 30-minute full friends refresh where nothing changed | nothing |
| A phone catching up after a day offline | only what changed that day |

### Encryption of records

- **Device-written records** (game logs, your data, and API data without an agent) are
  **end-to-end encrypted** per record, with a key derived from the account's app key (your
  devices share it already). The server stores ciphertext.
- **Record keys are hidden too**: the server sees `HMAC(account key, "friends/usr_…")`, not the
  user ID, so it can't read your friends list from record names.
- **Agent-written records** can't be end-to-end encrypted: the server produces them. They're
  encrypted at rest with the account's data key (Part 2), so a copy of the disk is useless; the
  running agent can read them, as it can read the session anyway.
- Setting up or removing an agent moves the API collections between the two forms (the new
  producer re-writes them; the old versions become tombstones).

### Server storage: SQLite

KV would be enough for end-to-end encrypted records (the server can only store, look up and
list them by sequence). It isn't for the agent: it queries its own data (profiles older than
7 days to refresh, friends that just came online to push, co-location over time for time
together, storage per account for the admin page). Those are indexes and queries, which SQLite
gives declaratively, along with migrations, `sqlite3` for inspecting a live server, `VACUUM
INTO` for consistent backups, and its crash safety. The C code it adds is no new cost: the
server already compiles C for every target (`ring`).

Shape:

- `records(account, collection, key, seq, schema, source, observed_at, hlc, deleted, payload)`,
  primary key `(account, collection, key)`, index `(account, seq)`. Same table for encrypted and
  agent records; `payload` is the (encrypted) CBOR document.
- A few extracted, indexed columns for agent records only (e.g. `fetched_at`, `user_id`), for
  the agent's own queries. Changing a collection's schema needs a database migration only if one
  of those changes.
- Separate tables for accounts, leases, agent state and key slots.
- WAL mode; one writer task; the database file in the data directory, mode 0600.

### Agents for every account

- Any account on a server can set up an agent; the server owner isn't special.
- Per account: its own data key, key slots, agent state and lease. One account's devices unlock
  only that account's agent.
- The admin page shows that an account has an agent and its state (locked / running / expired /
  stopped by the admin), and lets the admin stop or remove it; it never shows or exports the
  session.
- **Before anyone hands a session to a server, the app shows a red, boxed warning** that has to
  be acknowledged (a checkbox, then the button):

  > **Whoever runs this server can take over your VRChat account.** The server keeps your
  > VRChat login to act for you. Its operator, or anyone who breaks into it, can use it while
  > it runs. Only do this on a server run by you, or by someone you'd give your password to.
  > If you stop trusting this server: remove the agent here, then change your VRChat password.

- Limits: `[agent] max_agents` and a server-wide cap on VRChat requests. Many accounts on one IP
  can look unusual to VRChat; the admin page shows how many agents run.

## Part 2: keeping the VRChat login

### What there is to store

VRChat has no OAuth and no scoped tokens. Logging in takes username + password + 2FA (TOTP,
email code or recovery code). The result is two cookies: `auth` (the session) and
`twoFactorAuth`. Whoever holds them **is** the account, until VRChat ends the session.

| Secret | Gives an attacker | Rule |
|---|---|---|
| Password | The account until it's changed; also other sites where it's reused | **Never stored.** Used once to log in, then zeroized. |
| TOTP secret | Every future 2FA code, forever | **Never stored.** With the password it would let the server re-login unattended, but whoever gets the server would then own the account for good. |
| Session cookies | The account until the session ends | **Stored, encrypted** (below). When VRChat ends the session, the account holder logs in again (TODO.md "Round 9": expiry notice, renew from the app). |

Recovery if a server is compromised: remove the agent and change the VRChat password, which
should end other sessions (to verify; TODO.md "Log out everywhere" asks the same).

### Threat model

| Threat | Example | Defended by |
|---|---|---|
| Data at rest leaks | Backup, PVC snapshot, stolen SD card, `docker cp` of the volume | Session encrypted with a data key that isn't on that disk |
| Bug in the internet-facing server | A request-parsing bug gives code execution in the HTTP process | The HTTP process never holds the session; a separate agent process does |
| Other software on the host | Reading `/proc/<pid>/mem`, core dumps, swap | `memfd_secret`, `PR_SET_DUMPABLE=0`, no core dumps, `mlock` / no swap |
| Root on the host, or a container escape with ptrace | Injects code into the agent | **Not fully defensible** while the agent must use the session; made harder and noisier |
| The server's operator | Whoever runs it | **Not defensible.** Hence the red warning |

### Tier 1: at rest (survives restarts)

Envelope encryption, like LUKS key slots:

- A random 256-bit **data key (DEK)** per account encrypts the session and the account's agent
  records (XChaCha20-Poly1305, already used in `vfm-share`; associated data binds it to the
  account and the server's key).
- The DEK is never written in the clear, only **wrapped** by one or more **key slots**. Any one
  slot unlocks it. Without an unlocked slot the account's agent is **locked**: it doesn't run,
  everything else on the server does, and the app says why.

| Slot | Kind | Unlocks after a restart | Copy of disk / backup gets | Notes |
|---|---|---|---|---|
| **The account's devices** | attended, **default** | automatically when any of the account's devices connects (seconds, no typing) | nothing | DEK sealed to the account's app key (X25519, like `self` items); the device sends it back with `POST /v1/agent/unlock`. A phone with the app in the background is usually enough. |
| **Passphrase** | attended, optional | typing it in the app or on the admin page | nothing, if strong | Argon2id-derived key. For when no device is around. |
| **systemd credentials / TPM2** | unattended, opt-in | on boot | nothing without that machine | `systemd-creds encrypt --with-key=tpm2`, `LoadCredentialEncrypted=`; the key appears in `$CREDENTIALS_DIRECTORY` (non-swappable, only that service). |
| **Kubernetes Secret / KMS** | unattended, opt-in | on pod start | nothing from the volume alone, *if* the Secret store is protected separately (etcd encryption with KMS, External Secrets / OpenBao) | Or OpenBao/Vault transit, so the key never reaches the pod. |

**Unattended slots are a reluctant opt-in, on two levels:**

1. The **operator** enables a slot type in the server's config (it needs the TPM credential or
   the Secret to exist). The admin page then shows it as available.
2. The **account holder** opts in per account, in the app, after a second red warning: "Anyone
   who controls this server's machine (or cluster) can unlock your VRChat login without you.
   Use this only if your agent must restart while all your devices are off." They confirm by
   typing the server's name. They can remove the slot at any time.

A key file next to the data is not offered: it would be today's plaintext with extra steps.

### Tier 2: in memory (while running)

Privilege separation, as OpenSSH does it:

```text
 vfm-server (HTTP, internet-facing)  ──unix socket, narrow protocol──►  vfm-agent (no listening socket)
   never holds a session                                                  holds DEKs + sessions in secret memory
                                                                          talks to api.vrchat.cloud / pipeline only
```

- `vfm-agent` is the same binary with a subcommand, started as a child process; one agent
  process serves all accounts (or one per account, if the account count stays small; to decide
  when implementing). The socket protocol is a short list of calls: `unlock(account, sealed
  DEK)`, `login(account, username, password)`, `2fa(account, code)`, `vrchat(account,
  allow-listed request)`, `status`, `logout(account)`. It never returns cookies.
- Hardening of the agent process (checked on Linux 6.18, see "Checked"):
  - **`memfd_secret(2)`** (Linux 5.14+; older kernels needed the `secretmem.enable` boot flag,
    6.18 works without it): sessions and DEKs live in pages removed from the kernel's direct
    map. Reads through `/proc/<pid>/mem` fail (checked), so debuggers, `gcore` and memory
    scrapers get nothing, even as root; `process_vm_readv` should fail the same way (same
    page-pinning path, not checked yet). Falls back to `mmap` + `mlock` where unavailable.
  - **`prctl(PR_SET_DUMPABLE, 0)`**: no core dumps, and processes of the same user can't
    ptrace it. Yama `ptrace_scope ≥ 1` adds to that (1 on the test machine; distro defaults
    vary).
  - **`mlock`** / `MADV_DONTDUMP` on any buffer that briefly holds a cookie (HTTP headers);
    zeroize after use (`zeroize` crate). `RLIMIT_CORE=0`.
  - **seccomp filter** after startup: only what TLS sockets, the unix socket, memory and time
    need; no `execve`, `ptrace`, `process_vm_writev`, no file writes outside its data.
  - Never logs headers; errors from VRChat are passed on without request headers.
- Limit: root with `CAP_SYS_PTRACE` can still inject code into the agent and have it read its
  own secret memory. That's a targeted attack on a live process, a much smaller window than
  "copy a file".
- The kernel keyring isn't used: container runtimes block `add_key`/`keyctl` by seccomp by
  default (`EPERM` in the check below), and keyrings aren't namespaced per container.
- Windows / macOS / Termux (the server runs there too): `VirtualLock` + `CryptProtectMemory` on
  Windows, `mlock` on macOS; no protection beyond the process boundary. The app says "best
  protected on a Linux server".

### Kubernetes specifics

- `memfd_secret` was allowed by this container's seccomp profile; whether the cluster's
  `RuntimeDefault` profile allows it must be checked (a one-line test pod). If not, a
  `Localhost` seccomp profile that adds it.
- `memfd_secret` and `mlock` count against `RLIMIT_MEMLOCK`; a session is a few hundred bytes,
  so one page per account is enough.
- A Kubernetes Secret slot only helps if etcd encryption at rest is on (or the Secret comes from
  an external store).
- Nodes without swap (the Kubernetes default) remove the swap concern for everything else.

## Plan

1. **Session at rest**: encrypt today's `logger/session.json` with a DEK + device slot +
   passphrase slot; locked/unlocked state in the app and on the admin page. Fixes the plaintext
   file without changing anything else. Small.
2. **SQLite + record schema**: the versioned schema crate, the `records` table, `/v1/sync`,
   change tracking, the lease replacing the heartbeat hand-off. Without an agent this already
   gives every device of an account all API data from the lease holder, end-to-end encrypted.
   Start with what a phone without login needs (`me`, `friends`, `presence`, `profiles`,
   `worlds`), then every other collection. Large.
3. **Agent process**: split the VRChat client out of the HTTP process; `memfd_secret`, dumpable
   0, seccomp; per-account agents with key slots and the red warnings. Medium.
4. **Agent as producer**: agents write the API collections and hold the lease permanently;
   devices send writes (invites, favorites, …) through the agent, checked against the app's
   write allow-list; the app runs with no VRChat login in it. Large.
5. **Expiry notice and renew from the app** (Round 9), over the same sealed-login path.

## Decisions

Decided 2026-11-20:

1. **System of record: the server**; authority per fact by richness (Part 1).
2. **Agents for every account** on a server; the owner isn't special. Red, boxed warning that
   must be acknowledged.
3. **Sync everything**, as versioned records with per-account sequence numbers.
4. **Schema versioned**: per collection and record, shared crate, forward compatible.
5. **Storage: SQLite.**
6. **Lease**: the agent holds it permanently when there is one; otherwise whoever is live.
7. **Without an agent, the client is the only source of truth** for API data; it uploads all of
   it, and every other device of the account sees it.
8. **Key slots**: attended by default (the account's devices; passphrase optional); unattended
   (TPM / systemd credentials, Kubernetes / KMS) only as a reluctant opt-in by the operator *and*
   the account holder, behind a red warning.

Details to settle while implementing: the exact per-collection schemas, HLC format, one agent
process for all accounts vs one per account, and tombstone retention.

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
