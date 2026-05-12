# General rules for the project

## Project Context (Reference)

Wayland socket proxy that can do minor changes to messages for any programs
that use its downstream socket.

### Source structure

- **`src/main.rs`** — entry point, argument parsing, creating and listening on
  the downstream socket, accepting connections.
- **`src/lib.rs`** — public types (enum `ObjType` for tracking Wayland objects).
- **`src/proto.rs`** — reading/writing Wayland messages (8-byte header + body).
- **`tests/integration.rs`** — integration tests with a compositor emulator.
- **`build.rs`** — removed; FD forwarding via Unix sockets is now done
  through the `uds` crate on stable Rust.

### Key components

- **`Args`** — CLI arguments: `--upstream` (path to compositor socket),
  `--downstream` (path to the new socket),
  `--app-id` / `--title` (replace or prefix — via `--prefix` / `--prefix-title`),
  `--debug`.
- **`Packet`** — Wayland message: `id` (object), `opcode`, `body`.
- **`AncillaryReader` / `AncillaryWriter`** — wrappers for reading/writing
  Unix socket ancillary data (FD forwarding), required by the Wayland protocol.
- File lock (`lock_path`) — prevents running a second instance with the same
  downstream socket.

### Data flow

Each connection spawns two threads:

1. **Client→Server** (downstream → upstream) — reads messages from the client,
   tracks object creation
   (Display → Registry → XdgWmBase → XdgSurface → XdgToplevel),
   modifies `set_app_id` (opcode 3) and `set_title` (opcode 2)
   if the corresponding arguments are set,
   forwards to upstream.
2. **Server→Client** (upstream → downstream) — reads responses from the compositor,
   handles `global` events to determine the type_id of the `xdg_wm_base` interface,
   forwards to the client.

### Lifecycle

1. Creates the downstream socket (with a file lock).
2. Notifies systemd about readiness (via `sd_notify`).
3. Accepts connections from Wayland clients in a loop.
4. For each connection, establishes an upstream connection to the compositor.
5. Spawns two threads (client→server, server→client).
6. On either connection breaking, shutdowns both sockets via defer.

### Object tracking diagram

```text
Display (id=1) → get_registry (opcode=1) → Registry → bind (opcode=0) →
  XdgWmBase → get_xdg_surface → XdgSurface → create_toplevel → XdgToplevel
```

### Nuances

- `sd_notify::booted()` — checks whether the system is booted with systemd,
  for readiness notification.
- `Object type_id` for `xdg_wm_base` is determined dynamically from `global`
  events in the server→client thread.
- Supports xdg_wm_base protocol versions 0–6.
- FD forwarding via Unix sockets (required by the Wayland protocol)
  is done through the `uds` crate (`UnixStreamExt::send_fds`/`recv_fds`),
  available on stable Rust.

### Tasks

Use these commands for corresponding tasks:

- `mise run fmt` — fixes formatting.
- `mise run lint` — runs all linters.
- `mise run test` — runs all tests.
- `mise run build` — builds release binary.
- `mise run ci` — runs full CI pipeline (fmt + lint + test + git dirty check).
- `mise run cover:rust` — runs tests with LLVM coverage instrumentation.
- `mise run cover:rust:total` — prints total coverage percentage.

---

## Mandatory Rules

### Repository Safety

- DO NOT create, amend, squash, rebase,
  or otherwise modify existing commits.
- DO NOT switch branches.
- DO NOT perform any network git operations
  inside this repository
  (e.g. `git push`, `git pull`, `git fetch`).
- You MAY use `git stash` if necessary,
  but clean up after yourself.
- You MAY use `git restore` for reverting local changes.
- Do not delete, rewrite, or mass-modify files
  outside the explicit scope of the task.
- Avoid destructive shell commands
  (e.g. `rm -rf`, recursive operations)
  unless explicitly required.

### Coding Standards

#### Semantic Linefeeds (comments and documentation only)

Start each sentence on a new line.
Break long sentences at natural pauses —
after commas, semicolons, conjunctions,
or between logical clauses.
Do NOT hard-wrap to a fixed column width.
The goal is meaningful diffs:
one changed idea = one changed line.

NOTE: The above example does not mean you should break into very short lines,
you can write lines with up to 96 characters if it's good for semantic.

#### Documentation (markdown)

- Write new documentation in English.
- Avoid adding new documentation
  unless specifically requested by user.
- Update existing documentation together with code changes
  ONLY if otherwise existing documentation became incorrect.
- Keep lines within 96 characters.
  Do NOT break semantically single line unless it won't fit into 96 characters.

#### Commenting

- Write new comments in English.
- Do not add redundant comments
  that restate obvious code behavior.
- Explain rationale, intent, trade-offs,
  and non-obvious behavior.
- Use full sentences in comments and documentation.
- Keep lines within 96 characters.
  Do NOT break semantically single line unless it won't fit into 96 characters.
- NEVER include architecture details and namespace-related gotchas into comments,
  add them into corresponding documentation files instead!
  Script comments may only refer docs on these topics, not duplicate or replace it.
