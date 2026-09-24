#!/usr/bin/env bash
# Heartbeat: run this check on BOTH machines (local and VPS).
# It touches a timestamp file every minute; the other side's guard-*.check.sh
# watches this file's age over ssh and alerts when it goes stale (i.e. this
# insomnia daemon or the whole machine is dead).
#
# The path to touch depends on where the daemon runs:
#   - home machine (systemd user install): /root/insomnia/state/heartbeat
#     (the checks dir lives directly on the host)
#   - VPS (docker): /app/state/heartbeat — the container path; it is the
#     SAME file as /root/insomnia/state/heartbeat on the VPS host, because
#     docker-compose mounts ~/insomnia/state at /app/state. The home-side
#     guard-vps.check.sh reads it via that host path over ssh.

# period: 1m
# timeout: 5s

# home (systemd):
# touch /root/insomnia/state/heartbeat

# vps (docker):
touch /app/state/heartbeat
