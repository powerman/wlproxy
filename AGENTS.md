# General rules for the project

## Project Context (Reference)

Wayland socket proxy that can do minor changes to messages for any programs
that use its downstream socket.

### Структура исходников

- **`src/main.rs`** — точка входа, парсинг аргументов, создание и слушание downstream-сокета,
  принятие соединений.
- **`src/proto.rs`** — чтение/запись Wayland-сообщений (8-байтовый заголовок + тело).
- **`build.rs`** — удалён; передача FD через Unix sockets теперь
  осуществляется через крейт `uds` на стабильном Rust.

### Ключевые компоненты

- **`Args`** — CLI-аргументы: `--upstream` (путь к сокету композитора),
  `--downstream` (путь к новому сокету),
  `--app-id` / `--title` (замена или префикс — через `--prefix` / `--prefix-title`),
  `--debug`.
- **`Packet`** — Wayland-сообщение: `id` (объект), `opcode`, `body`.
- **`AncillaryReader` / `AncillaryWriter`** — обёртки для чтения/записи Unix socket
  ancillary data (передача FD), необходимы для Wayland-протокола.
- File lock (`lock_path`) — предотвращает запуск второго экземпляра с тем же downstream-сокетом.

### Поток данных

На каждое соединение создаются два потока:

1. **Client→Server** (downstream → upstream) — читает сообщения от клиента,
   отслеживает создание объектов (Display → Registry → XdgWmBase → XdgSurface → XdgToplevel),
   модифицирует `set_app_id` (opcode 3) и `set_title` (opcode 2)
   если указаны соответствующие аргументы,
   пересылает на upstream.
2. **Server→Client** (upstream → downstream) — читает ответы от композитора,
   обрабатывает `global` события для определения type_id интерфейса `xdg_wm_base`,
   пересылает клиенту.

### Жизненный цикл

1. Создаёт downstream-сокет (с файловой блокировкой).
2. Нотифицирует systemd о готовности (через `sd_notify`).
3. В цикле принимает соединения от Wayland-клиентов.
4. Для каждого соединения устанавливает upstream-соединение к композитору.
5. Запускает два потока (client→server, server→client).
6. При обрыве любого соединения shutdown обоих сокетов через defer.

### Диаграмма объектов (отслеживание)

```text
Display (id=1) → get_registry (opcode=1) → Registry → bind (opcode=0) →
  XdgWmBase → get_xdg_surface → XdgSurface → create_toplevel → XdgToplevel
```

### Нюансы

- `sd_notify::booted()` — проверка, загружена ли система с systemd,
  для уведомления о готовности.
- `Object type_id` для `xdg_wm_base` определяется динамически из `global`-событий
  в server→client-потоке.
- Поддерживаются версии протокола xdg_wm_base 0–6.
- Передача FD через Unix sockets (требуется Wayland-протоколом)
  осуществляется через крейт `uds` (`UnixStreamExt::send_fds`/`recv_fds`),
  доступный на стабильном Rust.

### Tasks

Use these commands for corresponding tasks:

- `mise run fmt` — fixes formatting.
- `mise run lint` — runs all linters.

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
