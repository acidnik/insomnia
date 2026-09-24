#!/usr/bin/env bash
# Guard: run LOCALLY. Watches the VPS insomnia heartbeat (examples/checks/
# heartbeat.check.sh must run on the VPS). If the file hasn't been touched
# for more than 5 minutes, the VPS daemon (or the whole VPS) is dead — alert.
#
# Requires: ssh key auth local → vps (as root), and the ssh alias "vps" in
# ~/.ssh/config (or replace with root@server.com).
#
# The staleness window must be several times the heartbeat period, so a
# single network blip doesn't false-alarm; `flake` absorbs the rest.

# tags: guard
# period: 2m
# timeout: 30s
# flake: 2m
# repeat: 30m, 2h
# report_restored: false
# message: VPS insomnia is not heartbeating (vps down?)

[ -n "$(ssh vps 'find /root/insomnia/state/heartbeat -mmin -5 2>/dev/null')" ] || exit 1
