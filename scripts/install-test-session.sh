#!/usr/bin/env bash
#
# Install a full-GNOME test session that boots this compositor in place of
# gnome-shell, for a dedicated test user — so the real gnome-session, gsd
# daemons, and portals all start around our binary without touching your own
# account or dconf.
#
# It works by overriding ExecStart of the per-user systemd unit GNOME uses to
# launch the shell (org.gnome.Shell@user.service; gnome-session@gnome.target
# requires the "user" instance). The unit is Type=notify, which our binary
# satisfies (sd_notify READY on startup). See docs/fork/RUNNING.md.
#
# The override points straight at the binary in this repo's target directory,
# so iterating is just: rebuild, log the test user out and back in — no
# reinstall needed.
#
# Usage:
#   cargo build                                 # as your normal user, first
#   sudo scripts/install-test-session.sh        # install/update
#   sudo scripts/install-test-session.sh --uninstall
#
# Env knobs:
#   TEST_USER=gsrs      test account to create/configure (default: gsrs)
#   PROFILE=debug       which build the session runs (default: debug)

set -euo pipefail

cd "$(dirname "$0")/.."
repo="$(pwd -P)"

user="${TEST_USER:-gsrs}"
profile="${PROFILE:-debug}"
bin="$repo/target/$profile/niri"
dropin_dir="/home/$user/.config/systemd/user/org.gnome.Shell@user.service.d"

if [ "$(id -u)" -ne 0 ]; then
    echo "error: must run as root: sudo $0 ${*:-}" >&2
    exit 1
fi

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$dropin_dir/override.conf"
    rmdir --parents --ignore-fail-on-non-empty "$dropin_dir" 2>/dev/null || true
    echo "Removed the org.gnome.Shell override for '$user'."
    echo "The account is left in place; remove it with: userdel -r $user"
    exit 0
fi

if [ ! -e "$bin" ]; then
    echo "error: $bin not found — build it first (as your normal user):" >&2
    if [ "$profile" = debug ]; then
        echo "  cargo build" >&2
    else
        echo "  cargo build --profile $profile" >&2
    fi
    exit 1
fi

if ! id "$user" >/dev/null 2>&1; then
    useradd -m -c "gnome-shell-rs test session" "$user"
    echo "Created user '$user'. Set a password so GDM can log it in:"
    passwd "$user"
fi

# The unit runs as $user, which must be able to traverse into the repo and
# exec the binary. Home dirs are sometimes 0700; fail early and say what to fix.
if ! runuser -u "$user" -- test -x "$bin"; then
    echo "error: user '$user' cannot execute $bin." >&2
    echo "Grant traversal along the path, e.g.:" >&2
    d="$(dirname "$bin")"
    while [ "$d" != "/" ]; do
        echo "  setfacl -m u:$user:--x $d" >&2
        d="$(dirname "$d")"
    done
    exit 1
fi

mkdir -p "$dropin_dir"
cat > "$dropin_dir/override.conf" <<EOF
[Service]
ExecStart=
ExecStart=$bin --session
EOF
chown -R "$user:$user" "/home/$user/.config"

echo
echo "Installed the org.gnome.Shell override for '$user':"
echo "  ExecStart=$bin --session"
echo
echo "To try it: switch user (lock screen → other user), log in as '$user'"
echo "choosing the regular \"GNOME\" session. GDM gives it its own VT;"
echo "Ctrl+Alt+F<n> flips between it and your session."
echo
echo "To iterate: rebuild, then log '$user' out and back in — the session"
echo "always runs whatever is currently at $bin."
