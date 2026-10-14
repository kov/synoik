#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

#
# Make synoik this machine's display manager: the system compositor holds the display, the login
# daemon replaces GDM (docs/fork/system-compositor.md, docs/fork/greeter.md). Everything a package
# would install, from this checkout; the binaries go to /usr/local/bin.
#
# Usage:
#   cargo build --release --workspace          # as your normal user, first
#   sudo scripts/install-display-manager.sh    # install/update, then reboot
#   sudo scripts/install-display-manager.sh --use-gdm   # back to GDM, then reboot
#
# GDM stays installed, so going back is one command — over ssh too, since there is no text
# console on the display under the system compositor. The outer and the sessions speak a
# versioned protocol and refuse a mismatch: install both binaries together, and reboot (or log
# every session out) after an update, so no session runs the old binary against the new outer.
#
# Autologin: /etc/synoik/login.conf, GDM's keys ([daemon] AutomaticLoginEnable,
# AutomaticLogin). It is created with autologin off and never overwritten.
# [debug] TestUsers there lets root log those users in on demand: synoik-login open-session
# (docs/fork/greeter.md §2.6).
#
# Env knobs:
#   PROFILE=release   which build to install (default: release)

set -euo pipefail

cd "$(dirname "$0")/.."
repo="$(pwd -P)"
profile="${PROFILE:-release}"

if [ "$(id -u)" -ne 0 ]; then
    echo "error: must run as root: sudo $0 ${*:-}" >&2
    exit 1
fi

if [ "${1:-}" = "--use-gdm" ]; then
    systemctl disable synoik-login.service synoik-system.service || true
    systemctl enable --force gdm.service
    # The text consoles come back with GDM, and plymouth runs after switch_root again.
    grubby --update-kernel=ALL --remove-args=fbcon=map:9
    rm -f /etc/systemd/system/plymouth-start.service.d/synoik.conf \
        /etc/systemd/system/plymouth-quit-wait.service.d/synoik.conf
    systemctl daemon-reload
    cat <<'EOF'
GDM is the display manager again. Reboot to switch: the system compositor keeps the display (and
DRM master, in systemd's fd store) until it is stopped, so after the reboot nothing of it runs.
To release it without rebooting, from outside the graphical session:
    systemctl stop synoik-login synoik-system
    systemctl clean --what=fdstore synoik-system
    systemctl start gdm
EOF
    exit 0
fi

for bin in synoik synoik-login; do
    if [ ! -x "target/$profile/$bin" ]; then
        echo "error: target/$profile/$bin not found — build it first (as your normal user):" >&2
        echo "  cargo build --profile $profile --workspace" >&2
        exit 1
    fi
done

r=resources

# Binaries. The previous synoik is kept beside it, for going back by hand.
if [ -e /usr/local/bin/synoik ]; then
    cp -a /usr/local/bin/synoik /usr/local/bin/synoik.prev
fi
install -Dm755 "target/$profile/synoik" /usr/local/bin/synoik
install -Dm755 "target/$profile/synoik-login" /usr/local/bin/synoik-login

# Users and the greeter's home.
install -Dm644 $r/synoik-system.sysusers /usr/lib/sysusers.d/synoik-system.conf
install -Dm644 $r/synoik-login.sysusers /usr/lib/sysusers.d/synoik-login.conf
install -Dm644 $r/synoik-login.tmpfiles /usr/lib/tmpfiles.d/synoik-login.conf
install -Dm644 $r/synoik-system.tmpfiles /usr/lib/tmpfiles.d/synoik-system.conf
systemd-sysusers synoik-system.conf synoik-login.conf
systemd-tmpfiles --create synoik-login.conf synoik-system.conf

# System units. After switch_root the system compositor is plymouth (docs/fork/boot-splash.md §2.2).
install -Dm644 -t /etc/systemd/system $r/synoik-system.service $r/synoik-login.service
for unit in plymouth-start plymouth-quit-wait; do
    install -Dm644 $r/plymouth-real-root.conf "/etc/systemd/system/$unit.service.d/synoik.conf"
done

# On a Limina guest the system compositor draws through venus, which its environment file selects:
# a system service does not read environment.d.
limina_env=/etc/environment.d/90-limina-zink.conf
dropin=/etc/systemd/system/synoik-system.service.d/limina.conf
if [ -e "$limina_env" ]; then
    install -d "$(dirname "$dropin")"
    printf '[Service]\nEnvironmentFile=%s\n' "$limina_env" >"$dropin"
else
    rm -f "$dropin"
fi

# PAM, polkit, D-Bus.
install -Dm644 -t /etc/pam.d $r/pam/*
install -Dm644 $r/synoik-system.rules /etc/polkit-1/rules.d/50-synoik-system.rules
install -Dm644 $r/synoik-greeter.rules /usr/share/polkit-1/rules.d/50-synoik-greeter.rules
install -Dm644 $r/synoik-login.dbus.conf /usr/share/dbus-1/system.d/synoik-login.conf

# The greeter's dconf: its profile, and the administrator's (empty) login-screen database.
install -Dm644 $r/synoik-greeter.dconf-profile /usr/share/dconf/profile/synoik-greeter
install -d /etc/dconf/db/synoik_greeter.d
dconf update

# The sessions, under gnome-session.
install -Dm644 -t /usr/share/gnome-session/sessions $r/synoik.session $r/synoik-greeter.session
install -Dm644 $r/synoik.session.conf /usr/lib/systemd/user/gnome-session@synoik.target.d/session.conf
install -Dm644 $r/synoik-greeter.session.conf \
    /usr/lib/systemd/user/gnome-session@synoik-greeter.target.d/session.conf
install -Dm644 -t /usr/lib/systemd/user $r/synoik.service $r/synoik-greeter.service
install -Dm644 -t /usr/share/wayland-sessions $r/synoik.desktop
install -Dm644 -t /usr/share/xdg-desktop-portal $r/synoik-portals.conf

# Our schemas, in the private dir the units name (see synoik.spec.rpkg for why private).
schemas=/usr/share/synoik/glib-2.0/schemas
install -d "$schemas"
rm -f "$schemas"/*.gschema.xml "$schemas"/*.gschema.override
install -Dm644 -t "$schemas" $r/schemas/*.gschema.xml $r/schemas/*.gschema.override
glib-compile-schemas --strict "$schemas"

# Autologin is the administrator's: created off, never overwritten.
if [ ! -e /etc/synoik/login.conf ]; then
    install -d /etc/synoik
    cat >/etc/synoik/login.conf <<'EOF'
# synoik-login's configuration, GDM's custom.conf keys (docs/fork/greeter.md §2.6).
[daemon]
AutomaticLoginEnable=false
#AutomaticLogin=
EOF
fi

# SELinux: the daemon runs in GDM's domain, and the greeter writes where GDM's would
# (docs/fork/greeter.md §9).
if command -v semanage >/dev/null; then
    label() {
        semanage fcontext -a -t "$1" "$2" >/dev/null 2>&1 || semanage fcontext -m -t "$1" "$2"
    }
    label xdm_exec_t /usr/local/bin/synoik-login
    label xdm_var_run_t '/run/synoik-system(/.*)?'
    label xdm_var_lib_t '/var/lib/synoik-greeter(/.*)?'
    restorecon -F /usr/local/bin/synoik-login /usr/local/bin/synoik
    restorecon -RF /var/lib/synoik-greeter
fi

# A bound fbcon would take the display back from the system compositor whenever a session lets
# go of its VT; mapping every console to a framebuffer that does not exist keeps it off them all,
# deferred takeover included, while the kernel's fbdev client still probes the displays at boot,
# which is what lets plymouth find them (docs/fork/system-compositor.md §9 S6).
if [ -e /sys/class/graphics/fbcon ] && ! grep -q 'fbcon=map:' /proc/cmdline; then
    grubby --update-kernel=ALL --args=fbcon=map:9
    echo "Added fbcon=map:9 to every kernel's command line (no text console on the display)."
fi

systemctl daemon-reload
systemctl disable gdm.service || true
systemctl enable synoik-system.service
# The display-manager.service alias is GDM's until now.
systemctl enable --force synoik-login.service

cat <<EOF
synoik is the display manager from the next boot. Reboot to switch.
Going back: sudo $repo/scripts/install-display-manager.sh --use-gdm, then reboot.
EOF
