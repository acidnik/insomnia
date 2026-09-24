#!/usr/bin/env bash
# tags: disk
# period: 1h
# var: dev=/dev/nvme*,/dev/sd*
# var: free_percent=10
# var: free_gb=10

ssh myserver 'df -h' | parse_df
