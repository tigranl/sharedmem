# sharedmem — design

Draft v0.1 · 2026-10-25 · nothing built yet · Rust

Shared memory and work awareness for Claude Code: one memory across all my
machines first, then a team view of who is working on what. This document is
for review; sections 15 and 16 list what is decided and what is still open.

## 1. Goals and non-goals

Goals, in order:

1. One memory across all my machines for Claude Code.
2. Team awareness: who is working on what, and overlapping work caught before
   merge. Built only after replaying real logs shows warnings would be right
   most of the time (section 13).
3. Better than claude-mem, mem0 and Graphiti where they are weak: memories
   checked against the code they describe, real per-user auth and scopes,
   writes that are not silently lost, self-hosted, OSI-licensed.

Non-goals for v0:

- claude.ai, Desktop and mobile. Claude Code only, so bearer tokens, no OAuth.
- LLM calls when saving anything.
- Blocking or locking. Everything is advisory and fails open.
- Merging, rebasing or pushing code.
- Uploading file contents. The server may mirror a git remote it is given
  access to; nothing unpushed ever leaves a machine.

## 2. Questions the system answers

Every component exists to answer one of these. Budgets are targets, not
measurements.

| #  | Question | Asked at | Budget | Freshness |
|----|----------|----------|--------|-----------|
| Q1 | What should this agent know about this repo and branch? | SessionStart hook | 300 ms | minutes |
| Q2 | Is anyone else changing this file, or has it changed since my copy? | PreToolUse hook | 200 ms | seconds, precise |
| Q3 | What do we know about X? | `recall` tool | 1 s | must flag stale results |
| Q4 | Save this fact. | `remember` tool | 1 s | never silently lost |
| Q5 | Is this memory still true? | inside Q1 and Q3 | — | as of the code this session sees |
| Q6 | Who is working on what in this repo? | briefing, later a dashboard | 1 s | 1–3 min |
| Q7 | What would rule R have warned about in this log? | `sm-server replay` | none | — |

v0 answers Q1, Q3, Q4 and Q5. Q6 and Q7 come with capture (slice 2), Q2 last.

## 3. Glossary and identity

- **User**: a person. Owns one token per device.
- **Device**: a machine with its own token, revocable on its own.
- **Session**: one Claude Code session, identified by the hooks' `session_id`.
- **Agent**: `user#session`, the unit of presence. Two sessions under one user
  are two agents (Collide found shared identities overwrite each other).
- **Repo**: identified by its normalized `origin` URL: lowercase, no scheme,
  credentials, port or `.git` (`git@github.com:Org/Name.git` →
  `github.com/org/name`). A repo with no `origin` falls back to
  `root:<sha of its root commit>`, which is the same in every clone.
- **Worktree**: a checkout's folder name. Display only, never identity.
- **CodeView**: the code a session sees: `(repo, base commit, {path → blob})`
  where the map holds only files that differ from the base commit.
- **Blob**: a file's content hash in git's blob format, computed in Rust (D11).
- **Observation**: one normalized hook event, the wire format (section 5).
- **Memory**: a fact saved on purpose, scoped `personal` or `repo`, with an
  optional `key` and optional anchors.
- **Anchor**: `(repo, path, symbol?, blob when saved, commit when saved)`. What
  makes staleness checkable. `symbol` stays empty until tree-sitter (slice 6).
- **Finding**: an awareness result: kind, severity, evidence, and whether it is
  asserted (certain) or surfaced (for the agent to judge). Slice 4.

## 4. Architecture

```
 each machine                                         one server (tailnet or TLS proxy)
 ┌────────────────────────────────────┐              ┌──────────────────────────────────┐
 │ Claude Code                        │              │ sm-server                        │
 │  hooks ──▶ sm-hook ──▶ outbox ─────┼─ POST /v1 ──▶│ ingest ─▶ observations (append)  │
 │            ◀── injected context ◀──┼─ GET  /v1 ───│ context ◀─ memories, sessions    │
 │  MCP ──────────────────────────────┼─ /mcp ──────▶│ remember / recall / forget       │
 └────────────────────────────────────┘              │ SQLite · git mirrors (slice 3)   │
                                                     └──────────────────────────────────┘
```

Layers, each owning one kind of complexity:

| Layer | Owns | Hard part | Lives in |
|-------|------|-----------|----------|
| Capture | hook events → observations | latency, offline, fail-open | `hook` crate |
| Ingest | event log, sessions, sequence numbers | gaps, duplicates, auth | `server::ingest` |
| Code state | which code each session sees | code changes, branches, worktrees | `server::code`, `core::code` |
| Code index | files, later symbols and edges | languages, incremental parsing | `server::index` (slice 6) |
| Knowledge | memories, search, staleness | ranking, contradictions | `server::knowledge`, `core::memory` |
| Awareness | presence, findings | precision | `server::awareness`, `core::rules` |
| Delivery | what Claude sees | noise, token budget | `server::context`, `hook` |

Two rules:

1. A layer reads only from layers below it.
2. Code state, the index and awareness are caches. `sm-server rebuild` recreates
   them from the observation log plus git. Memories, tokens and the log itself
   are the only primary data.

Where truth lives, and what a lost event costs:

| Data | Source of truth | If an event is lost |
|------|-----------------|---------------------|
| Pushed commits and branches | the git remote | nothing; the server fetches (slice 3) |
| Uncommitted changes | the machine's working tree | healed by the next observation or snapshot |
| Code index | derived from code | nothing; recompute |
| Presence | short-lived markers | expires in minutes |
| Memories | the `remember` call | cannot happen silently: the call is synchronous and fails loudly (D7) |

## 5. Wire format: Observation v1

The hardest thing to change later, because it ships to every machine. It
carries state, never deltas, so observation N+1 repairs a lost N.

```rust
pub struct Observation {
    pub v: u16,                // schema version, 1
    pub id: Uuid,              // v7, client-generated; the idempotency key
    pub session: String,       // Claude Code session_id
    pub seq: u64,              // per session, starts at 1, strictly increasing
    pub host: String,          // display only; identity comes from the token
    pub at: i64,               // client clock, display only; server stamps received_at
    pub repo: Option<RepoRef>, // None outside a git repo
    pub kind: Kind,
}

pub struct RepoRef {
    pub id: String,            // normalized, section 3
    pub branch: Option<String>,// None when detached
    pub head: String,          // commit sha
    pub worktree: String,      // folder name
}

pub enum Kind {
    SessionStart { source: String },      // startup | resume | clear | compact | fork
    Prompt { text: Option<String> },      // None when prompts are not shared (section 10)
    Read { paths: Vec<String> },
    Wrote { files: Vec<FileState> },
    Snapshot { merge_base: Option<String>, dirty: Vec<FileState> },
    Stop,
    SessionEnd,
}

pub struct FileState {
    pub path: String,          // repo-relative
    pub blob: Option<String>,  // None = deleted
}
```

Server rules:

- Same `id` twice → acknowledge again, store once.
- `seq` > last + 1 → store it and answer `resync` for that session.
- `seq` ≤ last → store it, but apply it to derived state only where it is newer
  than what that state already holds (per path, by `seq`).
- No snapshot yet for a session → answer `resync`.

A `Snapshot` is HEAD, the merge-base with the default branch, and the blob of
every file `git status --porcelain` reports. SessionStart and Stop always send
one; any hook sends one after the server asked for `resync`.

## 6. Hooks

One binary, `sm-hook <event>`, reads the hook's JSON from stdin. It always exits
0, writes only `hookSpecificOutput` JSON to stdout, and logs errors to
`~/.local/state/sharedmem/hook.log`.

| Event | Matcher | Mode | Sends | Returns |
|-------|---------|------|-------|---------|
| SessionStart | * | sync | `SessionStart`, `Snapshot` | context block (section 9) |
| UserPromptSubmit | * | async | `Prompt` | — |
| PostToolUse | Read\|Grep\|Glob | async | `Read` | — |
| PostToolUse | Edit\|Write\|MultiEdit\|NotebookEdit | async | `Wrote` | — |
| Stop | * | async | `Snapshot`, `Stop` | — |
| SessionEnd | * | async | `SessionEnd` | — |
| PostToolUse | `mcp__.*sharedmem.*__remember` | async | anchor baselines (section 7) | — |
| PreToolUse | Edit\|Write\|MultiEdit | sync | — | findings as `additionalContext`, never a permission decision (slice 4) |

Async hooks (`"async": true`) never delay the session, so only SessionStart is
on the clock: 300 ms for the network, 500 ms total, then it injects nothing.

Outbox: one JSON file per observation in `~/.local/state/sharedmem/outbox/`.
Every hook run appends, then flushes up to 100 of the oldest in one POST and
deletes what was acknowledged. Losing the outbox loses activity only, which
snapshots repair.

Sequence numbers: a counter file per session in
`~/.local/state/sharedmem/sessions/`, under a file lock, because async hooks
for one session can run at the same time.

Git: shell out to `git` for `rev-parse`, `remote get-url`, `status
--porcelain=v2` and `merge-base`, all inside `hook::git` (D11). It matches the
user's git exactly; `gix` only if process spawning shows up in measurements.
File hashes are computed in Rust, not by git (D11).

## 7. Server

`sm-server serve` runs axum on tokio.

| Route | Auth | Purpose |
|-------|------|---------|
| `POST /v1/observations` | yes | batch ingest → `{acked: [id], resync: [session]}` |
| `GET /v1/context?session=&repo=` | yes | Q1 payload: memories with anchors, later active work |
| `POST /v1/memories/{id}/anchors` | yes | anchor baselines from the hook: blob per path, plus HEAD |
| `/mcp` | yes | rmcp streamable HTTP: `remember`, `recall`, `forget` |
| `GET /healthz` | no | liveness |

MCP tools:

- `remember(text, repo?, scope?, key?, anchors?: [path])`: saves
  synchronously.
  - The MCP connection does not know which Claude Code session or directory it
    serves, so the SessionStart block tells Claude the repo id to pass.
  - `scope` defaults to `repo` when a repo is given and to `personal`
    otherwise.
  - With a `key`, it replaces the active memory with the same
    `(scope, repo, key)`; the old row records `superseded_by`.
  - Anchors are stored without a baseline. The PostToolUse hook on `remember`
    runs on the machine, hashes the anchored files and posts their blobs and
    HEAD to `/v1/memories/{id}/anchors` straight after the call.
- `recall(query, limit = 10)`: SQLite FTS5 with bm25. Only memories the caller
  may see; the session's repo ranks first. Each result carries staleness.
- `forget(id)`: soft delete, owner only.

Auth is one tower layer over the whole router. It reads
`Authorization: Bearer <token>`, compares the sha256 against `tokens`, and puts
the principal `(user, device)` in request extensions. Tokens are created with
`sm-server token add --user tigran --device laptop`, shown once, stored hashed.

Binds `127.0.0.1` by default. Reach it over Tailscale or behind a TLS reverse
proxy; `--bind` exposes it, and the docs say tokens must not cross a network in
plaintext.

## 8. Storage

SQLite through `rusqlite` with the bundled library, WAL mode, one writer.

```sql
users        (id, name)
tokens       (hash PRIMARY KEY, user_id, device, created_at, revoked_at)

observations (id PRIMARY KEY, session, seq, user_id, device, repo,
              received_at, kind, body JSON)          -- append-only
             UNIQUE (session, seq)

-- derived, rebuildable
sessions     (session PRIMARY KEY, user_id, device, repo, branch, head,
              merge_base, last_seq, last_snapshot_seq, last_seen_at)
session_files(session, path, blob, seq, PRIMARY KEY (session, path))

-- primary
memories     (id, user_id, scope, repo, key, text, created_at,
              superseded_by, deleted_at)
memory_anchors (memory_id, repo, path, symbol, blob_at_save, commit_at_save)
memories_fts (FTS5 over memories.text)
```

A partial unique index on `(scope, repo, key)` where the memory is still active
enforces "newest fact wins".

Running it:

- One writer thread owns the only write connection, and handlers send it
  work over a channel. Reads use a small pool.
- WAL mode with `synchronous = FULL`. Memories are primary data, so a
  `remember` that returned OK must survive a power cut.
- Backups: Litestream streaming to S3-compatible storage, or a nightly
  `VACUUM INTO`.
- Raw observations are kept 90 days (configurable). Memories and derived
  state are kept.
- All SQL lives in `server::store`. Moving to Postgres means rewriting that
  module (FTS5 becomes `tsvector`), not the layers above it. Move only for
  more than one server instance, or for write contention that has actually
  been measured.

Schema changes: each change is a new numbered SQL file in
`crates/server/migrations/` (`0002_add_x.sql`), embedded in the binary and
applied in order by `server::store::migrate` when the store opens. SQLite's
`user_version` records how many have run. Migrations are forward-only, and a
file that has shipped is never edited: a fix is the next file. Each runs in one
transaction together with its version bump, so it applies whole or not at all.
Before migrating an existing database, the server copies it to
`<db>.bak-v<from>` with `VACUUM INTO`. A binary older than the database's
schema refuses to open it (D12).

## 9. Code state, staleness and the context block

Staleness is one pure function in `core`:

```rust
enum Staleness { Fresh, Changed, Deleted, Unknown }
fn staleness(anchor: &Anchor, current: Lookup<Option<Blob>>) -> Staleness
```

Where `current` comes from depends on the caller:

- **SessionStart (exact).** The server returns memories with their anchors; the
  hook hashes the anchored paths locally (D11) and renders the block. No
  server-side git needed.
- **recall (best effort).** The server looks in the session's dirty files
  first, then the git mirror at the session's head (slice 3), and otherwise
  answers `Unknown`. Until mirrors exist, clean files on a moved head are
  `Unknown`. That is honest, never a guess.

The context block is injected at SessionStart as `additionalContext`, at most
about 1,500 tokens (configurable):

```
## sharedmem · github.com/tigran/osshunt @ main
- [repo] Seeded ATS slugs are often wrong; fix companies.yaml first. (companies.yaml: fresh)
- [repo] GITHUB_TOKEN lifts code-search rate limits. (fetch/github.py: CHANGED since saved, verify)
- [personal] Prefers building over upfront scoping questions.
Save durable facts with sharedmem `remember` (repo="github.com/tigran/osshunt",
anchors=[files the fact is about]); pass a `key` to replace an older fact.
```

Selection in v0: the repo's memories, then personal ones, newest first, until
the budget runs out. Relevance ranking waits for real usage data.

## 10. Security and privacy

What leaves a machine: repo id, branch, commit shas, repo-relative paths, blob
hashes, prompt text (when allowed) and memory text. Never file contents, and
never the bodies of Edit or Write tool inputs.

- Prompts: `share_prompts = true` by default for a personal server, `false` on a
  team server unless a repo opts in. Redacting obvious secret patterns is
  required before the team slice.
- Visibility: `personal` memories are visible to their owner only. `repo`
  memories are visible to everyone with that repo; v0 has one user, and the team
  slice adds a repo access table.
- Every query in `knowledge` and `awareness` takes the principal. There is no
  unscoped read path, which is the mistake mem0's self-hosted server made.

## 11. Repository layout

```
sharedmem/
  Cargo.toml           workspace
  crates/
    proto/   wire types: Observation v1, API types. Deps: serde, uuid.
    core/    pure logic, no IO: repo id normalization, CodeView, Anchor,
             staleness, supersession, later finding rules. Deps: proto.
    hook/    bin `sm-hook`: stdin → Observation, git, outbox, blocking HTTP
             (ureq). Deps: proto, core. No async runtime.
    server/  bin `sm-server`: axum, rmcp, rusqlite. Subcommands:
             serve | token | rebuild | replay. Deps: proto, core.
  plugin/    Claude Code plugin: plugin.json (userConfig: url, token),
             hooks/hooks.json, .mcp.json
  DESIGN.md
```

The crate graph enforces the layer rules at compile time: `core` cannot do IO,
and `hook` cannot pull in tokio.

The plugin carries the setup. `userConfig` asks for the server URL and a
`sensitive` token stored in the keychain. `.mcp.json` points at
`${user_config.url}/mcp` with `Authorization: Bearer ${user_config.token}`, and
hooks read `CLAUDE_PLUGIN_OPTION_*`. A teammate installs it from a private
marketplace with one command.

## 12. Testing and evaluation

- `core`: unit tests plus proptest for repo id normalization, staleness and
  supersession.
- `hook`: real hook stdin captured once per event as fixtures, checked against
  expected observations; a test that hook work outside git stays under 20 ms.
- Git: fixture repos built by a test helper in a temp dir (init, commit,
  branch, dirty files, worktree).
- `server`: integration tests on in-memory SQLite for idempotency, gaps leading
  to resync, out-of-order seq, auth refusal and scope isolation.
- Recall: about 20 questions with known answers drawn from my existing memory
  files.
- Replay (slice 2): `sm-server replay --rules <name>` prints the findings a rule
  would have produced on a real log. I label them; a rule ships only above about
  70% useful (ConE's bar).

## 13. Build order

Each slice is usable on its own.

1. **Memory everywhere (Q1, Q3, Q4, Q5).** proto, core (repo id, content
   hash, staleness, supersession), server (tokens, `remember`/`recall`/
   `forget`, `/v1/context`, anchor baselines), hook (SessionStart and the
   `remember` baseline), plugin. Done when a fact saved on the laptop shows up
   in the next desktop session with an honest staleness flag. Importing
   `~/.claude/projects/*/memory/*.md` moves to slice 2.
2. **Capture and replay (Q6, Q7).** The remaining hooks, outbox, seq, resync,
   derived sessions and files, `replay`. Done when two weeks of my own parallel
   sessions are logged and labeled.
3. **Git mirrors.** The server fetches each repo's remote, giving exact recall
   staleness and visibility into pushed branches.
4. **Warnings (Q2).** Only rules that passed replay, for example "another live
   session wrote this file" or "this file changed since your copy". Delivered
   once per path, before the edit.
5. **Team.** Users, repo access, prompt redaction, private marketplace. A pilot
   with 2–3 colleagues for a month, measuring warnings per week and the share
   that were useful.
6. **Depth.** tree-sitter symbols and anchors, embeddings as a "surfaced only"
   tier, typed intents (possibly Foremerge-compatible).

## 14. Prior art this borrows from

- **GitHub Copilot Memory**: facts carry citations that are checked against the
  current code when used.
- **Collide**: hooks over MCP tools ("every measured saving came through the
  hooks"), precision over recall, short-lived presence markers, per-agent
  identity. Ideas only: its engine is FSL-licensed.
- **Foremerge**: typed operations instead of prose ("inference caught one of
  ten real conflicts"); asserted versus surfaced findings.
- **Microsoft ConE**: conservative thresholds; "giving false warnings will
  likely lead to rejection of a tool like ConE."
- **claude-mem remote spool**: outbox, acknowledgement, deduplication on retry.

## 15. Decisions

- **D1** Rust; two binaries, so the hook stays small and fast.
- **D2** git is the source of truth for code; observations are hints.
- **D3** Observations carry state, with per-session sequence numbers; a gap
  triggers a resync.
- **D4** Fail open everywhere; advisory only; hooks never set a permission
  decision.
- **D5** No LLM calls when writing.
- **D6** SQLite (rusqlite, bundled) until one writer is not enough.
- **D7** Remote MCP over HTTP with bearer tokens. Memory writes are synchronous
  and fail loudly; observations are async and may be lost, because snapshots
  repair them.
- **D8** Repo identity is the normalized `origin` URL, falling back to the root
  commit.
- **D9** Write-only hooks run async; only SessionStart (and PreToolUse in slice
  4) is synchronous.
- **D10** No vector database. If embeddings are added (slice 6), they are a
  BLOB column in SQLite searched by brute-force cosine in Rust, merged with
  FTS5 results by reciprocal rank fusion. Add them only if the recall eval
  shows keyword misses. Revisit near a million vectors, and even then use an
  index inside the existing database (sqlite-vec, or pgvector after a Postgres
  move), not a separate service.
- **D11** git is confined to two modules: `hook::git` (repo identity, HEAD,
  branch, changed files) and, from slice 3, `server::mirror`.
  - `proto` and `core` stay VCS-neutral. `RepoId` and `Rev` are opaque
    strings.
  - `ContentHash` is computed in Rust in git's blob format,
    `sha1("blob <len>\0" + bytes)`. It equals git's hash for files without
    clean filters (LFS, line-ending conversion) and works in plain folders.
    Staleness only ever compares our hashes with our hashes, so filters do not
    matter until the slice 3 mirror compares against git's own blobs.
  - Outside a git repo everything except repo-scoped features works, and
    `remember` falls back to `personal`.
  - Supporting another VCS (Sapling, Mercurial, jj without a git backend)
    means a new snapshot module and a new mirror module. `core` does not
    change.
- **D12** Schema migrations are numbered SQL files run by our own small runner
  on `PRAGMA user_version`, not by a migrations crate.
  - Forward-only. There are no down migrations: the `.bak-v<N>` copy taken
    before migrating is the way back.
  - `0001_init.sql` is the schema from before migrations existed, still with
    `IF NOT EXISTS`, so a database created then (version 0) adopts it as a
    no-op and moves to version 1.
  - A database newer than the binary is refused, never touched.
- **D13** Metrics are Prometheus, pulled from `sm-server`.
  - `GET /metrics` has its own listener, `--metrics-bind` (default
    `127.0.0.1:9477`), with no token. It is outside the authed router, so
    metrics never reach the tailnet by accident.
  - Labels take bounded values only: route templates, method, status, tool,
    outcome, scope, state. Never ids, users, devices, repos, paths, query text
    or memory text.
  - The database gauges are read at scrape time. Nothing about metrics is
    stored in the database.
  - The short-lived `sm-hook` cannot be scraped. It will report each run to
    the server, which turns the report into metrics, so every metric is on
    one endpoint. Not built yet.

## 16. Open questions

Each needs checking before the slice that depends on it.

1. Does `additionalContext` from PreToolUse reach Claude when no
   `permissionDecision` is set? Test with a real run (slice 4).
2. ~~How does an rmcp 3.x tool handler read the principal that the auth layer put
   in request extensions?~~ Through the tool's `RequestContext`. rmcp's
   `StreamableHttpService` inserts each HTTP request's `http::request::Parts`
   into that request's MCP extensions on every path: stateless, legacy session
   and initialize (`transport/streamable_http_server/tower.rs` in rmcp 3.7.0).
   The handler reads `ctx.extensions.get::<Parts>()`, then
   `parts.extensions.get::<Principal>()`. The `Extension<Parts>` tool extractor
   does the same thing. The principal is per HTTP request, not per MCP session.
   If it is missing, the tool fails closed. Checked 2026-10-26; see
   `server::mcp::principal`.
3. ~~Does rusqlite's bundled SQLite include FTS5?~~ Yes. `libsqlite3-sys/build.rs`
   builds the bundled library with `-DSQLITE_ENABLE_FTS5` (checked 2026-10-25).
4. What do `git status` plus hashing cost in a large repo, and do we need
   fsmonitor? (slice 2)
5. Can async hooks for one session run concurrently, and in what order? The
   file lock assumes yes. (slice 2)
6. A local stdio MCP proxy for offline `remember` and exact recall staleness.
   Worth it, or does slice 3 make it unnecessary?
7. License: Apache-2.0 or MIT.
8. Name.
9. An MCP call carries no Claude Code session or directory. Slice 1 works
   around it: Claude passes `repo` (from the SessionStart block), and a
   PostToolUse hook on `remember` fills the anchor baselines. The local stdio
   proxy from question 6 would remove both workarounds.
