# sharedmem

A shared memory for Claude Code across your machines. `sm-server` stores the
memories in SQLite and serves the MCP tools `remember`, `recall` and `forget`
over HTTP. At session start, the `sm-hook` binary injects the current repo's
memories. Each anchored file is checked on your machine, and a memory whose
file changed since it was saved is flagged. See [DESIGN.md](DESIGN.md).

## Install

```sh
cargo install --path crates/server   # sm-server, on the machine that hosts the memory
cargo install --path crates/hook     # sm-hook, on every machine that runs Claude Code
```

The hooks call `sm-hook` by name, so `~/.cargo/bin` must be on `PATH` for
Claude Code.

## Run the server

```sh
sm-server token add --db ~/sharedmem.db --user tigran --device laptop   # prints the token once
sm-server serve --db ~/sharedmem.db --bind 127.0.0.1:8377
```

Create one token per device. The server keeps only a sha256 of each token, so
a lost token cannot be shown again: make a new one. There is no revoke command
yet. To revoke a device's token, run
`sqlite3 ~/sharedmem.db "UPDATE tokens SET revoked_at = 1 WHERE device = 'laptop'"`.
`GET /healthz` is the only route that needs no token.

Logs go to stderr. By default you get startup, migrations, warnings and
errors. Set `RUST_LOG` for more:

```sh
RUST_LOG=sm_server=debug sm-server serve ...   # every routed request, auth refusal and tool call
RUST_LOG=sm_server=trace sm-server serve ...   # also recall query text and successful auth
RUST_LOG=debug sm-server serve ...             # also rmcp and axum internals
```

Tokens and memory text are never logged at any level; recall queries only
at `trace`.

Back up the database with `sqlite3 ~/sharedmem.db "VACUUM INTO 'backup.db'"`
or with Litestream.

A new `sm-server` migrates the database schema when it opens it. Before that,
it copies an existing database to `<db>.bak-v<N>`, where N is the old schema
version. An older `sm-server` refuses to open a database that a newer one has
migrated. Schema changes go in `crates/server/migrations/` as new numbered
files (DESIGN.md section 8).

## Reach it from other machines: never send tokens in plaintext

The server speaks plain HTTP and binds to loopback by default. Every request
carries a bearer token, so never expose the port to a network you do not
control. Use one of these:

- **Tailscale.** Bind to the machine's tailnet address, for example
  `--bind 100.x.y.z:8377`, and use `http://<machine>.<tailnet>.ts.net:8377` as
  the URL. WireGuard encrypts the traffic.
- **TLS reverse proxy.** Keep `--bind 127.0.0.1:8377` and put Caddy or nginx
  in front. Then use `https://sharedmem.example.com` as the URL. A Caddyfile
  for that:

  ```
  sharedmem.example.com {
      reverse_proxy 127.0.0.1:8377
  }
  ```

## Metrics

`sm-server serve` also serves Prometheus metrics at `GET /metrics` on a
second listener, `--metrics-bind`, default `127.0.0.1:9477`. It needs no
token, so keep it on loopback. It is separate from `--bind`, so metrics never
reach the tailnet by accident.

`ops/` has a local Prometheus and Grafana:

```sh
docker compose -f ops/compose.yaml up -d   # with sm-server on the default --metrics-bind
docker compose -f ops/compose.yaml down    # keeps the data volumes
```

- Prometheus: <http://127.0.0.1:9090>. It scrapes `host.docker.internal:9477`
  every 15 s; Status → Targets shows whether the job is up.
- Grafana: <http://127.0.0.1:3000/d/sharedmem>, user `admin`, password
  `admin`.

Both bind to loopback. Docker Desktop on macOS reaches the loopback-bound
metrics port through `host.docker.internal`. Other engines may not; there,
run Prometheus on the host instead.

Labels and privacy:

- Labels take bounded values only: the route template
  (`/v1/memories/{id}/anchors`, never the id), method, status, tool, outcome,
  scope, state and reason. Requests that match no route are not counted.
- No metric carries an id, user, device, repo, path, query or memory text.
- The database gauges (memories by scope and state, anchors with and without
  a baseline, tokens, schema version, file size) are read at scrape time.

Prometheus 3 stores bucket bounds as floats, so the empty-recall bucket is
`le="0.0"`. The share of recalls that found nothing:
`rate(sharedmem_recall_results_bucket{le="0.0"}[1h]) / rate(sharedmem_recall_results_count[1h])`.

## Install the Claude Code plugin

This repository is also a plugin marketplace:

```sh
claude plugin marketplace add /path/to/sharedmem
claude plugin install sharedmem@sharedmem
```

Claude Code asks for the server URL (without `/mcp`) and the device token. It
keeps the token in the system keychain.

The plugin sets up the following:

- the `sharedmem` MCP server at `<url>/mcp`, with the token as a bearer header;
- a SessionStart hook that runs `sm-hook session-start` and injects about
  1,500 tokens of memories;
- an async PostToolUse hook on `remember` that runs `sm-hook remembered`. It
  hashes the anchored files and records their state when the memory was saved.

The hooks never block a session. They time out after 300 ms of network time,
always exit 0, and log errors to `~/.local/state/sharedmem/hook.log`. Outside
the plugin, set `SHAREDMEM_URL` and `SHAREDMEM_TOKEN` instead.

## Develop

```sh
cargo test && cargo clippy --all-targets && cargo fmt
```
