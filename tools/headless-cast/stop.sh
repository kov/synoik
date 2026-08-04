#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

# Kills ONLY the PIDs start.sh/cast.sh recorded. Never pattern-matches — see the warning in
# start.sh for why that distinction is not academic.
set -u
R=${NH_DIR:-/tmp/nh}
[ -f "$R/pids" ] || { echo "nothing recorded in $R/pids"; exit 0; }
while read -r p; do
    [ -n "$p" ] && kill "$p" 2>/dev/null && echo "killed $p"
done < "$R/pids"
: > "$R/pids"
