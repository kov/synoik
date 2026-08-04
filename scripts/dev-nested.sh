#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Launch the compositor nested (Winit backend) for quick visual testing, with a
# terminal spawned inside so there's something to interact with immediately.
#
# Inside a Wayland/X11 session the Winit (nested-window) backend is selected
# automatically, so this is just `cargo run` with a startup command. See
# docs/fork/RUNNING.md for the testing matrix (notably: the host grabs Super, so
# test the overlay key on a TTY, not nested).
#
# Env knobs:
#   PROFILE=release        build/run the release profile (default: debug)
#   NESTED_TERMINAL=foot   terminal to spawn inside (default: first found)

set -euo pipefail

cd "$(dirname "$0")/.."

profile="${PROFILE:-debug}"
cargo_flags=()
[ "$profile" = release ] && cargo_flags+=(--release)

terminal="${NESTED_TERMINAL:-}"
if [ -z "$terminal" ]; then
    for candidate in kitty kgx gnome-terminal foot alacritty; do
        if command -v "$candidate" >/dev/null 2>&1; then
            terminal="$candidate"
            break
        fi
    done
fi

echo "Building synoik ($profile)…"
cargo build "${cargo_flags[@]}"

if [ -n "$terminal" ]; then
    echo "Launching nested compositor; spawning '$terminal' inside."
    exec cargo run "${cargo_flags[@]}" -- -- "$terminal"
else
    echo "No terminal found to spawn; launching nested compositor only."
    echo "(set NESTED_TERMINAL=... to spawn one)"
    exec cargo run "${cargo_flags[@]}"
fi
