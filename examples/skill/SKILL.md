---
name: "create-insomnia-check"
description: "Create a new monitoring check for the insomnia daemon: what the script should look like, where the real file lives, and where to symlink it."
version: 2
created: "2026-09-24"
updated: "2026-09-24"
---
## When to Use
Use when the user asks to create/add a monitor, check, or alert (\"создай мониторинг X\", \"добавь проверку Y\", \"алерть если Z\"). Also use to answer \"how do I monitor X with insomnia\". Assumes the insomnia daemon is running (systemd user unit `insomnia`) with checks_dir = ~/.config/insomnia/checks and libexec helpers parse_df/parse_curl in PATH of checks.

## Procedure
1. Ask/decide what to monitor and pick the pattern: (a) plain exit-code check, (b) HTTP via curl | parse_curl, (c) disk space via ssh 'df -h' | parse_df, (d) custom script (any #! interpreter).
2. Write the script with a `#!` shebang and a meta header of `# key: value` lines directly under it. Pattern (a): `-f /var/run/flag || exit 1` with `# message: Flag file missing`. Pattern (b): `curl -sS -o /dev/null -w '%{http_code}' URL | parse_curl` with `# var: ignore_codes=503` if needed. Pattern (c): `ssh host 'df -h' | parse_df` with `# var: dev=/dev/nvme*,/dev/sd*`, `# var: free_percent=10`, `# var: free_gb=10`.
3. Choose meta keys for the failure policy: `# period:` (default 5m), `# timeout:` (default 60s), `# flake: 1m` for silent single retry, `# repeat: 30m, 1h, 6h` escalation for re-alerts, `# report_restored: false` to suppress recovery messages (default sends 🟢 restored), `# tags: a, b` free-form, `# message:` template with $name/$exitcode/$stdout/$stderr substitution.
4. Place the real file in the project it belongs to (or directly in checks_dir if it is generic infrastructure): write it to e.g. ~/src/<project>/checks/<id>.check.sh where <id> is a short unique name; the id seen by the daemon is the symlink file name in the checks dir.
5. Symlink it into the watched dir: `mkdir -p ~/.config/insomnia/checks && ln -s ~/src/<project>/checks/<id>.check.sh ~/.config/insomnia/checks/<id>.check.sh`. The daemon hot-reloads via inotify — no restart needed.
6. Test before relying on it: run the script manually and check its exit code (`~/src/<project>/checks/<id>.check.sh; echo $?`) — alert = exit != 0. Verify the daemon picked it up in the log (`journalctl --user -u insomnia -f` or daemon stdout): expect 'check loaded: <id>' and a first run.
7. Confirm schedule/state on disk: ~/.local/state/insomnia/<id>.check.sh.json shows alert_active and next_run_at.
## Pitfalls
- No +x bit = check silently ignored (debug-level log only).
- Hidden files (*.check.sh is fine, .check.sh is not), *.tmp and *~ backups are skipped.
- Meta regex matches only `# word: value` at line start — `##x` or indented `  # period:` after code starts still match, so keep meta lines directly after the shebang and avoid `#` comments that look like `key:`.
- Durations: bare number = seconds, units are s/m/h/d; `1h30m` is valid.
- $stdout/$stderr in message are truncated (1000/1500 chars); total TG message capped at 3900 chars.
- parse_df expects `df -h` (human sizes); parse_curl expects a numeric HTTP code as the last token on stdin.
- Editing the target file of a symlink triggers reload of the check — do not leave scratch edits in place; also editor temp files in checks_dir are skipped, but chmod during editing is what activates a freshly created check.

## Verification
1. Script exits 0/1 as expected when run manually.
2. Daemon log shows 'check loaded: <id>' after symlink creation.
3. State file ~/.local/state/insomnia/<id>.check.sh.json exists and next_run_at is in the future.