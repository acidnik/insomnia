# insomnia

A file-based monitoring daemon in Rust. Every check is an executable file (bash, python, whatever its `#!` says) with metadata in `# key: value` comment lines. Drop it into the watched directory and the daemon schedules it, runs it, and sends alerts to Telegram.

Checks are alerted on **non-zero exit code or timeout**. `stdout`/`stderr` of the check go into the alert message, so a check can be as simple as `curl -fsS ...`.

## Configuration

`~/.config/insomnia/config.toml` (or pass a path as the first CLI argument, or set `INSOMNIA_CONFIG`):

```toml
# Where check scripts live. The daemon watches this dir with inotify:
# adding/editing/removing a file (or a symlink to one) takes effect at once
# (editor save bursts are debounced by a second).
checks_dir = "~/.config/insomnia/checks"

# Where per-check state JSON files are stored. Needed so the daemon remembers
# which checks are already failing and does not re-alert after a restart.
# Default: ~/.local/state/insomnia
# state_dir = "~/.local/state/insomnia"

# Optional dir with helper tools (parse_df, parse_curl, ...) that are
# prepended to PATH of every check. Default: not set
# libexec_dir = "~/src/insomnia/libexec"

[telegram]
# Bot token and chat id for alerts. All settings and secrets live here in
# config.toml — nothing else to configure anywhere.
bot_token = "123456:ABC..."
chat_id = "123456789"

[defaults]
# Used when a check has no `# period:` of its own.
# Default: "5m"
period = "5m"

# Used when a check has no `# timeout:` of its own. A check that hangs longer
# than this is killed (whole process group) and reported as failed.
# Default: "60s"
timeout = "60s"
```

Durations are written as `30`, `30s`, `5m`, `1h`, `1h30m`, `2d` — a bare number means seconds.

## Example checks

A check with every option used, documented inline:

```bash
#!/usr/bin/env bash

# Human-readable display name: used in alerts instead of the file id
# ("api down" instead of "api-health.check.sh down"). No default —
# without it alerts use the file name.
# name: My Service API

# Comma-separated tags, purely for your own grouping/filtering. No default.
# tags: ssh, server, disk

# How often to run the check. Falls back to [defaults].period (5m).
# period: 1h

# Kill the check (and its children — ssh, curl, ...) if it runs longer than
# this; the check is reported as failed with exit=TIMEOUT.
# Falls back to [defaults].timeout (60s).
# timeout: 30s

# Alert message template. $name (the display name from `# name:`, or the
# file id), $exitcode, $stdout, $stderr are
# substituted; long output is truncated to keep the Telegram message sane.
# Default: "🔴 $name: check failed (exit=$exitcode)" followed by stderr (or
# stdout if stderr is empty).
# message: myserver disk almost full ($stdout)

# Re-alert escalation schedule, relative to the moment of the previous alert:
# first repeat after 30m, then after 1h, then every 6h. Without this key no
# repeat alerts are sent — only recovery or a new check failure re-alerts.
# repeat: 30m, 1h, 6h

# Send a "🟢 restored" message when the check recovers.
# Default: true
# report_restored: false

# If the check fails, wait this long and silently retry once before alerting
# (absorbs blips like a service being restarted). Default: disabled.
# flake: 1m

# How often to re-run the check while its alert is active. Falls back to the
# check's own `period` — set this only if you want a different (usually
# faster) re-check cadence while failing.
# recheck: 10s

# Everything after `var:` is passed to the check as an environment variable,
# so parameterized helper tools can read their config from the env.
# var: dev=/dev/nvme*,/dev/sd*
# var: free_percent=5
# var: free_gb=10

ssh myserver 'df -h'
```

An API check — minimal body, all the tuning in the header:

```bash
#!/usr/bin/env bash
# tags: api
# period: 5m
# timeout: 15s
# flake: 1m
# repeat: 30m, 1h, 6h
# var: ignore_codes=503
# message: API health failed (exit=$exitcode)
# $stderr

curl -sS -o /dev/null -w '%{http_code}' https://site.com/api/health | parse_curl
```

The simplest possible check — just an exit code:

```bash
#!/usr/bin/env bash
# period: 10m
# message: Flag file missing

-f /var/run/service/flag || exit 1
```

## Alert lifecycle

1. Check fails → this is when the incident starts (downtime is counted from here); if `flake` is set, the check is silently retried once after that interval.
2. Still failing → alert is sent, the check re-runs every `recheck` (default 1m).
3. Still failing later → repeat alerts follow the `repeat` schedule, counted from the previous alert; each one ends with the total downtime, `(down for 2h 30m)`.
4. Check succeeds → `🟢 restored after 2h 32m` (unless `report_restored: false`), back to the normal `period` schedule.

State (active alert, escalation index, counters, **last run time**) is persisted per check. A daemon restart does not re-alert for already-known failures, and does **not** re-run every check: a check runs after restart only if its period has already elapsed since the last run (edited periods apply from the last run moment); otherwise it keeps its schedule.

## Parser details

Metadata is any line matching `#\s+(\w+): (.*)`. Known keys are consumed, unknown keys are ignored, so other tooling can keep its own `# key: value` headers in the same files. Checks must be executable (`chmod +x`); hidden files, `*.tmp` and editor backups (`*~`) are skipped.

If a check defines `# name:`, that name replaces the file id in Telegram alerts (failed, repeated and restored) — handy to avoid Telegram auto-linking file names like `api-health.check.sh` into fake domain links.

## Helper tools (libexec)

If `libexec_dir` is set in the config, it is prepended to `PATH` of every check. Helper tools parse command output and turn it into an exit code, so checks stay one-liners. They read their thresholds from environment variables set with `# var: key=value`.

### parse_df — low disk space

Reads `df -h` output from stdin. Triggers (exit 1) when available space is below the threshold:

```bash
#!/usr/bin/env bash
# var: dev=/dev/nvme*,/dev/sd*   # glob masks for device or mount point (empty = all real fs)
# var: free_percent=10           # alert when free space < 10% (default: 10)
# var: free_gb=10                # ... or free space < 10 GB (default: 0 = disabled)

ssh myserver 'df -h' | parse_df
```

Alert line: `disk low: / (/dev/sda1): 4.2G free (4%)`.

### parse_curl — HTTP health

Reads the HTTP status code from stdin. Triggers when curl died (no code on stdin) or the code is >= 400:

```bash
#!/usr/bin/env bash
# var: ignore_codes=503,502      # codes to treat as OK (default: none)

curl -sS -o /dev/null -w '%{http_code}' https://site.com/api/health | parse_curl
```

## Running as a system systemd service

A system unit (not a user unit): insomnia runs as your user but survives logout/reboot with no `loginctl enable-linger` dance.

```sh
cargo install --path .                      # binary to ~/.cargo/bin/insomnia
sudo cp insomnia.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now insomnia
journalctl -u insomnia -f
```

The unit runs as `User=nik` (edit to your username) and passes the config path explicitly (`~/.config/insomnia/config.toml`), since a system service doesn't inherit your session environment.

Log levels: `info` (default) — check load/unload/reload and alerts; `debug` — plus per-run results and Telegram sends (what the unit sets); `trace` — plus every inotify event, which is one line per temp file, write and rename of an editor save, so it is only useful when debugging the watcher itself.

## Deploying to a VPS (docker)

The binary is compiled in docker on the local machine (`rust:1-bookworm` — its glibc matches the `debian:bookworm-slim` runtime image; building natively on the host would produce a binary the VPS may not run), rsynced over, and baked into a minimal runtime image on the VPS. The VPS doesn't need a Rust toolchain, the local machine only needs docker.

One-time setup on the VPS:

```sh
git clone https://github.com/acidnik/insomnia ~/insomnia
mkdir -p ~/insomnia/checks ~/insomnia/state
cp deploy/config.vps.example.toml ~/insomnia/config.toml   # then fill in secrets
```

The VPS config must use the container paths (`/app/checks`, `/app/state`, `/app/libexec`) — see `deploy/config.vps.example.toml`. Config, checks and state are bind-mounted from `~/insomnia`; `libexec/` is baked into the image.

Then from the repo, from anywhere:

```sh
just deploy root@server.com   # push → build locally → rsync → build image → restart
just build-vps root@server.com  # rebuild image only
just restart root@server.com    # restart container only
```

Drop new checks into `~/insomnia/checks` on the VPS — the daemon picks them up via inotify, no redeploy needed.

Hot-reload semantics: editing a check re-reads it but does **not** run it immediately — the run schedule is `last run + period`. It runs right away only if the (possibly shortened) period has already elapsed since the last run; a brand-new check runs immediately.

Saving a file in an editor is a burst of inotify events (temp file, write, chmod, rename), so re-reads are debounced: the check is re-read once, a second after the last event of the burst. Removing a check takes effect immediately (its runs stop at once).

## Mutual monitoring — who guards the guards

Two insomnia instances — one at home, one on the VPS — watch each other, so a dead daemon or a dead machine still triggers an alert from the surviving side. The whole scheme is **two checks**, one per machine: each side touches a heartbeat flag for the other and checks the flag it receives. Flags are plain files; freshness = mtime age:

```text
home (insomnia)                              VPS (insomnia)
  mutual-guard-home:
    ssh vps touch .../heartbeat-home  ──────▶  (our pulse, read by the VPS)
    ssh vps find .../heartbeat -mmin  ◀──────  (vps pulse → stale = vps daemon dead)
                                               ssh fails → whole vps unreachable
  mutual-guard-vps:
    touch /app/state/heartbeat         ◀─────  (vps pulse, read by home over ssh)
    find /app/state/heartbeat-home     ──────▶  (home pulse → stale = home daemon down)

all alerts land in the same Telegram chat:
  home daemon/machine dies → stops pushing → VPS sees stale flag, alerts
  VPS daemon dies  → its flag goes stale → home sees it, alerts
  VPS unreachable  → home's ssh fails → home alerts with "vps unreachable"
```

The message of each failed check names the exact problem (`$stdout`/`$stderr` carry the reason): `vps unreachable: cannot update heartbeat flag` vs `vps insomnia is not heartbeating (daemon down, machine reachable)` vs `home is not heartbeating (home daemon or machine down)`.

Why the parameters look the way they do:

- The staleness window (5m) is several times the heartbeat period (1m) — a single missed beat or a network blip must not false-alarm.
- `# flake: 2m` absorbs short ssh/network hiccups before alerting.
- `# report_restored: false` — a machine coming back after hours of downtime would otherwise spam a recovery message for an alert you already dealt with.
- `# timeout: 30s` on the home side (two ssh calls), 5s on the VPS side (local `find` only).

Requirements: ssh key auth home → VPS (alias `vps` in `~/.ssh/config`); the VPS-side check runs inside the insomnia container and needs no ssh at all.

## Building

```sh
cargo build --release
```

## For LLM agents

`examples/skill/SKILL.md` is a ready-made agent skill: point your coding agent at it and "create monitoring for X" becomes a one-liner — it knows the check patterns, where the real file lives, and where to symlink it.
