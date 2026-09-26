# AGENTS.md — правила для LLM

## О проекте

`insomnia` — демон мониторинга на Rust. Каждая проверка — исполняемый файл в watched-директории с мета-инфой в `# key: value` комментариях. Документация пользователя — в `README.md` (английский), там же примеры конфига и проверок.

## Структура

- `src/main.rs` — запуск, inotify-watcher (notify), главный select-цикл
- `src/config.rs` — TOML-конфиг (`checks_dir`, `state_dir`, `libexec_dir`, `[telegram]`, `[defaults]`)
- `src/metadata.rs` — парсер `# key: value` + парсер длительностей (`30s/5m/1h/1h30m/2d`)
- `src/check.rs` — загруженная проверка (id = имя файла, `version` для инвалидации записей планировщика)
- `src/state.rs` — стейт: JSON-файл на проверку в `state_dir` (атомарная запись tmp+rename)
- `src/runner.rs` — запуск через `#!` в собственном process group (setsid), killpg по таймауту
- `src/engine.rs` — планировщик (min-heap по времени), логика алертов
- `src/telegram.rs` — отправка в ТГ (plain text, truncate 3900)

## Ключевые решения (не ломать без необходимости)

- Все настройки и секреты — только в одном `config.toml`, никаких `.env`.
- id проверки = имя файла в watched-директории.
- Стейт — по JSON-файлу на проверку, переживает рестарт демона. Хранит `last_run_at`: рестарт НЕ запускает проверки — запускаются только просроченные (period < now - last_run_at); правки периода применяются от момента последнего прогона.
- Таймаут убивает всю process group проверки (ssh, curl и т.д.), а не только сам скрипт.
- Дефолты: `period=5m`, `timeout=60s`; recheck по умолчанию = period проверки (override per-check `# recheck:`); `report_restored` по умолчанию true.
- Пока алерт активен: перепроверка раз в `recheck`, повторы алерта по расписанию `repeat` от момента последнего алерта.
- `# name:` в шапке проверки — display name в алертах (failed/repeat/restored) вместо id файла; в логах остаётся id. Придумано чтобы клиенты ТГ не линковали имена вида `api-health.check.sh`.
- Hot-reload НЕ запускает проверку: расписание всегда `last_run_at + period` (период может быть новым). Немедленный запуск — только если после правки период уже истёк, или проверка никогда не запускалась.
- `notify::Watcher` trait должен быть в scope для `.watch()` — без него события молча не приходят.
- Редакторы (nvim/vim/...) сохраняют файлы через rename — это убивает inotify-watch на файл. Симлинк-цели смотрим через watch на их родительскую директорию (`watched_dirs` + refcount), переживает замену inode.
- Признак симлинка — `symlink_metadata().is_symlink()`, а НЕ `canonicalize(path) != path`: при относительном `checks_dir` последнее делает симлинком любой файл, движок вешает второй watch на ту же директорию (события приходят с абсолютным путём), все события уходят в dir-ветку, новый файл не загружается, удаление не выгружается. Пути из inotify сверяем канонически (`in_checks_dir`).
- Hot-reload дебаунсится trailing-окном 1s (`pending_reloads`: id -> токен; таймер выживает только у последнего события бурста). Без дебаунса одно сохранение (5-10 inotify-ивентов) даёт столько же релоадов с шагом 600ms. Удаление файла выгружает проверку сразу, без дебаунса.
- `insomnia.service` — **user unit** (`~/.config/systemd/user/`, `systemctl --user`), поэтому `%h` корректно разворачивается в home владельца и хардкодить пути не нужно. Требует `loginctl enable-linger $USER`, иначе менеджер (и демон) умрёт с последней сессией. Системный юнит с `User=` использовался раньше и удалён: в системном менеджере `%h` разворачивался в home менеджера (`/root`), игнорируя `User=`, и пути приходилось хардкодить
- Новая/перезагруженная проверка получает `version+1`: устаревшие записи в heap планировщика отбрасываются по version.

## Что ещё не сделано (roadmap)

- jitter, дашборд/статистика

## Двусторонний мониторинг (охрана охранников)

- Две проверки, по одной на машину (`examples/checks/mutual-*`), обе двунаправленные: тачим флаг для чужой стороны + проверяем свежесть флага, который чуже пушит к нам
- `mutual-guard-home` (дома): `ssh vps touch .../heartbeat-home` (наш пульс) + `ssh vps find .../heartbeat -mmin -5` (пульс VPS; протух = VPS-демон мёртв, ssh упал = VPS недоступен целиком)
- `mutual-guard-vps` (на VPS, в контейнере): локальный `touch /app/state/heartbeat` + `find /app/state/heartbeat-home -mmin -5` (пульс дома; протух = home daemon/machine down). SSH на VPS-стороне не нужен вообще
- Окно протухания 5m >> пульс 1m, `flake: 2m`, `report_restored: false`, причина в алерте через `$stdout`/`$stderr`

## Деплой на VPS (docker)

- `just deploy root@host` — push, сборка release-бинарника **в докере** (`deploy/Dockerfile.build`: builder на `rust:1-bookworm` — glibc совпадает с runtime-образом `debian:bookworm-slim`; нативная сборка на Arch дала бы несовместимый бинарь; крейты/артефакты в BuildKit cache-mounts, экспорт бинаря через `FROM scratch` + `--output`), rsync на VPS, сборка образа из готового бинаря, рестарт контейнера. VPS-половина рецептов (`build-vps`/`restart` без HOST) вызывается по ssh
- `deploy/Dockerfile.deploy` — debian-slim + python3/openssh/ca-certs; бинарь и `libexec/` запекаются в образ
- `deploy/docker-compose.vps.yml` — конфиг/checks/state маунтятся с хоста (`~/insomnia/...` → `/app/...`), проект назван `insomnia` (не "deploy", иначе коллизия с другими стеками на VPS)
- VPS-конфиг обязан использовать контейнерные пути: см. `deploy/config.vps.example.toml`

## libexec-тулзы

- `libexec/parse_df` — python, читает `df -h` из stdin; пороги в env: `dev` (glob-маски), `free_percent` (дефолт 10), `free_gb` (дефолт 0 = выкл); псевдо-ФС (tmpfs/overlay/...) пропускает всегда
- `libexec/parse_curl` — python, читает http code из stdin; триггер: кода нет (curl упал) или код >= 400 и не в `ignore_codes`
- тулзы печатают причину в stdout — дефолтный message демона включает stdout и stderr

## Проверка изменений

- `cargo build` — 0 warnings
- `cargo test` — 4 теста мета-парсера и durations
- парсеры: пайпы в `libexec/*` тестируются напрямую, см. примеры в их docstrings
- smoke: `mkdir -p tmp/checks tmp/state`, конфиг в `tmp/config.toml` (см. tmp/), `RUST_LOG=info timeout 5 cargo run -q -- tmp/config.toml`; hot-reload проверяется созданием/удалением файла в `tmp/checks` на работающем демоне

## Конвенции кода

- Комментарии без нумерованных шагов (номера выбиваются из порядка при эволюции кода) — только описательные подписи
- tmp/ в корне репо — черновики и тестовые стенды, не коммитится
