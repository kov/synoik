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
# Usage:
#   cargo build --release                       # as your normal user, first
#   sudo scripts/install-test-session.sh        # install/update
#   sudo scripts/install-test-session.sh --uninstall
#
# Env knobs:
#   TEST_USER=gsrs      test account to create/configure (default: gsrs)
#   PROFILE=release     which build to install (default: release)

set -euo pipefail

cd "$(dirname "$0")/.."

user="${TEST_USER:-gsrs}"
profile="${PROFILE:-release}"
bin=/usr/local/bin/gnome-shell-rs
dropin_dir="/home/$user/.config/systemd/user/org.gnome.Shell@user.service.d"

if [ "$(id -u)" -ne 0 ]; then
    echo "error: must run as root: sudo $0 ${*:-}" >&2
    exit 1
fi

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$dropin_dir/override.conf" "$bin"
    rmdir --parents --ignore-fail-on-non-empty "$dropin_dir" 2>/dev/null || true
    echo "Removed $bin and the org.gnome.Shell override for '$user'."
    echo "The account is left in place; remove it with: userdel -r $user"
    exit 0
fi

src="target/$profile/niri"
if [ ! -e "$src" ]; then
    echo "error: $src not found — build it first (as your normal user):" >&2
    echo "  cargo build --release" >&2
    exit 1
fi

if ! id "$user" >/dev/null 2>&1; then
    useradd -m -c "gnome-shell-rs test session" "$user"
    echo "Created user '$user'. Set a password so GDM can log it in:"
    passwd "$user"
fi

install -m755 "$src" "$bin"
if command -v restorecon >/dev/null 2>&1; then
    restorecon "$bin"
fi

mkdir -p "$dropin_dir"
cat > "$dropin_dir/override.conf" <<EOF
[Service]
ExecStart=
ExecStart=$bin --session
EOF
chown -R "$user:$user" "/home/$user/.config"

echo
echo "Installed $bin and the org.gnome.Shell override for '$user'."
echo
echo "To try it: switch user (lock screen → other user), log in as '$user'"
echo "choosing the regular \"GNOME\" session. GDM gives it its own VT;"
echo "Ctrl+Alt+F<n> flips between it and your session."
echo
echo "After a rebuild, rerun this script (or just: sudo install -m755 $src $bin)"
echo "and log '$user' out and back in."
