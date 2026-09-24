#!/usr/bin/env bash
# Mutual monitoring, VPS side — one check covers both directions:
#   1. touch our own heartbeat flag (locally) — tells the home side, which
#      reads this file over ssh, that the VPS insomnia daemon is alive
#   2. check the flag the home machine pushes here over ssh — stale flag
#      means the home insomnia daemon (or the whole home machine) is gone
#
# Docker note: runs inside the container, state is /app/state — the same
# dir as ~/insomnia/state on the VPS host (bind-mounted). The home side
# reads the flags here via the host path.

# tags: guard
# period: 1m
# timeout: 5s
# flake: 2m
# repeat: 30m, 2h
# report_restored: false
# message: mutual guard vps→home: $stdout$stderr

touch /app/state/heartbeat

[ -n "$(find /app/state/heartbeat-home -mmin -5 2>/dev/null)" ] \
    || { echo "home is not heartbeating (home daemon or machine down)"; exit 1; }
