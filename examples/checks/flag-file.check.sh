#!/usr/bin/env bash
# period: 10m
# message: Flag file missing

-f /var/run/service/flag || exit 1
