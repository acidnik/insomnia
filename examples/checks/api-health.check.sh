#!/usr/bin/env bash
# tags: api
# period: 5m
# timeout: 15s
# flake: 1m
# repeat: 30m, 1h, 6h
# message: API health failed (exit=$exitcode)
# $stderr

curl -fsS https://site.com/api/health
