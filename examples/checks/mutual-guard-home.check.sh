#!/usr/bin/env bash
# Mutual monitoring, HOME side — one check covers both directions:
#   1. touch our heartbeat flag on the VPS (over ssh) — tells the VPS that
#      the home insomnia (and the machine) is alive
#   2. check the VPS's own heartbeat flag — tells us the VPS insomnia
#      daemon is running its checks
# If ssh itself fails, the whole VPS is unreachable. A reachable machine
# with a stale flag means the VPS daemon is dead.

# tags: guard
# period: 1m
# timeout: 30s
# flake: 2m
# repeat: 30m, 2h
# report_restored: false
# message: mutual guard home→vps: $stdout$stderr

ssh vps 'mkdir -p /root/insomnia/state; touch /root/insomnia/state/heartbeat-home' \
    || { echo "vps unreachable: cannot update heartbeat flag"; exit 1; }

[ -n "$(ssh vps 'find /root/insomnia/state/heartbeat -mmin -5 2>/dev/null')" ] \
    || { echo "vps insomnia is not heartbeating (daemon down, machine reachable)"; exit 1; }
