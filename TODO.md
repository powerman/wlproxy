# TODO: Rust best practices improvements

## Фаза 1: Тесты

- [x] **1.1** Удалить бесполезный тест `body_size_adj` (тест реализации)
- [x] **1.2** Добавить `tempfile` в dev-deps, переписать интеграционные тесты на `TempDir`
- [x] **1.3** Интеграционный тест для `--title`
- [x] **1.4** Интеграционный тест для `--prefix` и `--prefix-title`
- [x] **1.5** Убрать fallback `target/debug/filterway` из `filterway_binary()`

## Фаза 2: Код — простые исправления

- [x] **2.1** `body: std::vec::Vec<u8>` → `body: Vec<u8>` в `Packet`
- [x] **2.2** `BODY_SIZE_ADJ` сменить тип с `i64` на `u16`
- [x] **2.3** `write_arg_string(data: String)` → `data: &str` + обновить callers
- [x] **2.4** Добавить `// Safety:` комментарии для `unsafe` блоков
- [x] **2.5** `match ready_result { Ok(_) => {} Err(e) => ... }` → `if let Err(e)`
- [x] **2.6** Удалить мёртвый код `send_extra`
- [x] **2.7** `panic!` при неподдерживаемой версии протокола → `Err(...)`

## Фаза 3: Обработка ошибок

- [x] ~~**3.1** Добавить `anyhow` в зависимости~~
- [x] ~~**3.2** Заменить `Errorize` на `anyhow::Context`, перевести main на `anyhow::Result`~~

## Фаза 4: Архитектура

- [ ] **4.1** Вынести `ObjType` enum на уровень модуля
- [ ] **4.2** Извлечь обработчики client→server и server→client в именованные функции
- [ ] **4.3** Убрать `fn inner()` / `match inner()` — `fn main() -> anyhow::Result<()>`

## Фаза 5: CI и сборка

- [ ] **5.1** CI: добавить `rust-cache` в `native` job
- [ ] **5.2** Подумать про `autocfg` вместо ручного probe в `build.rs`
- [ ] **5.3** Удалить Windows из `release.yml` (неподдерживаемая платформа)

## Фаза 6: Документация

- [ ] **6.1** README: убрать Windows-бейджи
- [ ] **6.2** AGENTS.md: добавить контекст про build.rs probing и nightly
- [ ] **6.3** Добавить `deny.toml` для `cargo-deny` (licenses, RUSTSEC)
