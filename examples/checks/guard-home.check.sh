#!/usr/bin/env bash
# Guard: run ON THE VPS. Watches the local machine's insomnia heartbeat
# (examples/checks/heartbeat.check.sh must run locally). If the file hasn't
# been touched for more than 5 minutes, the local daemon (or the whole
# machine) is dead — alert. This is the side that still works when your
# home machine/internet is down, so this alert reaches Telegram from the VPS.
#
# Requires: ssh key auth vps → local machine (as root, e.g. via Tailscale/
# WireGuard or a forwarded ssh port), the ssh alias "home" in the VPS's
# /root/.ssh/config, AND the ssh key mounted into the container (uncomment
# the key/known_hosts volumes in deploy/docker-compose.vps.yml — the check
# runs inside the insomnia container on the VPS).

# tags: guard
# period: 2m
# timeout: 30s
# flake: 2m
# repeat: 30m, 2h
# report_restored: false
# message: local insomnia is not heartbeating (home machine down?)

[ -n "$(ssh home 'find /root/insomnia/state/heartbeat -mmin -5 2>/dev/null')" ] || exit 1
