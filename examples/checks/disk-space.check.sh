#!/usr/bin/env bash
# tags: disk
# period: 1h
# var: dev=/dev/nvme*,/dev/sd*
# var: free_percent=5
# var: free_gb=10

# TODO: parse_df helper from libexec
ssh myserver 'df -h'
