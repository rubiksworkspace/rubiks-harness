# pa-uhp

The Unified Harness Protocol server: `prime-agent serve uhp`.

## Scope
Serves Prime Agent as a native harness at UHP `2026-10-19`, Core class: discovery, version negotiation, bearer auth, the error envelope, the harness and model lists, tasks (blocking and background), SSE streaming, session continuation and cancellation. Each task runs as a child `prime-agent -p --mode json` process in its own process group, with the prompt on stdin; `events.rs` turns that process's event stream into UHP events. Responses and session records are stored under `<agent dir>/uhp/` with owner-only permissions; a task a previous server left running is settled as `incomplete` with reason `interrupted` at startup.

## Non-goals
No capability above Core: file input and artifacts, session listing, harness management, input history, idempotency, plugins, environments and memories are all advertised as `false`. No TLS: off loopback the server refuses to bind without `--allow-remote`, and a reverse proxy has to terminate TLS. No per-user scoping or sandbox: one token is one principal, and whoever holds it can run anything the agent can, with the permissions of the server's user. No retention policy: stored responses stay until deleted. No session logic or model knowledge of its own: the composition root supplies the catalog through `CatalogSource`.

## Public API
`serve` runs the server until SIGINT or SIGTERM. `ServerConfig` is its configuration, `CatalogSource` the callback that reads the catalog, and `ModelCatalog`/`ModelEntry` what that callback returns.
