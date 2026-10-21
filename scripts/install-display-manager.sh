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
# Fedora (dracut, grubby, authselect's PAM stacks) and Debian (initramfs-tools, update-grub, the
# common-* stacks) are told apart by /etc/os-release. A Fedora booting systemd-boot gets its kernel
# command line from /etc/kernel/cmdline and its images from kernel-install instead of grubby and
# dracut.
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

distro=fedora
# shellcheck source=/dev/null
. /etc/os-release
case " ${ID:-} ${ID_LIKE:-} " in
    *" debian "*) distro=debian ;;
esac

# The initramfs splash (docs/fork/boot-splash.md §7.4, §7.6). dracut: where it finds the module,
# and the conf that swaps it for plymouth. initramfs-tools: the hook, the boot script that
# overrides plymouth's by name, and the outer's ordering after the disks.
dracut_module=/usr/lib/dracut/modules.d/90synoik
dracut_conf=/etc/dracut.conf.d/90-synoik.conf
itools_hook=/usr/share/initramfs-tools/hooks/synoik
itools_script=/etc/initramfs-tools/scripts/init-premount/plymouth
# The outer waits for the disks while the initramfs splash asks for their passwords (boot-splash.md
# §7.2), and the splash's own unit after switch_root (dracut's).
splash_dropin=/etc/systemd/system/synoik-system.service.d/initrd-splash.conf
splash_unit=/etc/systemd/system/synoik-splash.service
# Debian's kernel command line: a grub.d snippet, which update-grub reads after /etc/default/grub.
grub_snippet=/etc/default/grub.d/90-synoik.cfg
# What it gets: no fbcon on any display, and no fbdev client either (below).
kernel_args="fbcon=map:9 drm_kms_helper.fbdev_emulation=0"
# Debian's choice of display manager. Every display manager's postinst points
# display-manager.service at /lib/systemd/system/<its basename>.service from it on each upgrade,
# so an alias made only with `systemctl enable` goes back to GDM at gdm3's next upgrade.
default_dm=/etc/X11/default-display-manager

# systemd-boot, with kernel-install placing the kernels as its entries (the bls layout): the
# command line is /etc/kernel/cmdline, which kernel-install writes into each entry. GRUB's tools
# would edit entries nothing boots.
sdboot=false
if [ "$distro" = fedora ] && bootctl is-installed --graceful >/dev/null 2>&1 &&
    kernel-install inspect 2>/dev/null | grep -q 'KERNEL_INSTALL_LAYOUT=bls$'; then
    sdboot=true
fi

# Put back `args` into /etc/kernel/cmdline (`add`) or take them out of it (`remove`), each once.
edit_kernel_cmdline() {
    local verb=$1 arg out=()
    local -a have
    read -r -a have </etc/kernel/cmdline
    for arg in "${have[@]}"; do
        case " $kernel_args " in
            *" $arg "*) ;;
            *) out+=("$arg") ;;
        esac
    done
    if [ "$verb" = add ]; then
        # shellcheck disable=SC2206
        out+=($kernel_args)
    fi
    printf '%s\n' "${out[*]}" >/etc/kernel/cmdline
}

# Install every kernel systemd-boot has an entry for again: its entry takes the command line, and
# its initramfs is rebuilt.
reinstall_kernels() {
    local root token dir kver
    root=$(kernel-install inspect | sed -n 's/^ *KERNEL_INSTALL_BOOT_ROOT=//p')
    token=$(kernel-install inspect | sed -n 's/^ *KERNEL_INSTALL_ENTRY_TOKEN=//p')
    for dir in "$root/$token"/*/; do
        kver=$(basename "$dir")
        if [ -e "/lib/modules/$kver/vmlinuz" ]; then
            kernel-install add "$kver" "/lib/modules/$kver/vmlinuz"
        fi
    done
}

if [ "${1:-}" = "--use-gdm" ]; then
    systemctl disable synoik-login.service synoik-system.service || true
    if [ "$distro" = debian ]; then
        if [ -e "$default_dm.pre-synoik" ]; then
            mv "$default_dm.pre-synoik" "$default_dm"
        else
            rm -f "$default_dm"
        fi
    fi
    # plymouth back in the initramfs.
    if [ "$distro" = debian ]; then
        if [ -e "$itools_hook" ] || [ -e "$itools_script" ]; then
            rm -f "$itools_hook" "$itools_script"
            echo "Rebuilding the initramfs images with plymouth..."
            update-initramfs -u -k all
        fi
    elif [ -e "$dracut_conf" ] || [ -e "$dracut_module" ]; then
        rm -rf "$dracut_conf" "$dracut_module"
        if [ "$sdboot" = false ]; then
            echo "Rebuilding the initramfs images with plymouth..."
            dracut -f --regenerate-all
        fi
    fi
    rm -f "$splash_dropin" "$splash_unit"
    systemctl enable --force gdm.service
    # The text consoles come back with GDM, and plymouth runs after switch_root again.
    if [ "$distro" = debian ]; then
        if [ -e "$grub_snippet" ]; then
            rm -f "$grub_snippet"
            update-grub
        fi
    elif [ "$sdboot" = true ]; then
        edit_kernel_cmdline remove
        echo "Reinstalling the kernels with plymouth and the text consoles..."
        reinstall_kernels
    else
        grubby --update-kernel=ALL --remove-args="$kernel_args"
    fi
    for unit in plymouth-start plymouth-quit-wait plymouth-reboot plymouth-poweroff plymouth-halt \
        plymouth-kexec; do
        rm -f "/etc/systemd/system/$unit.service.d/synoik.conf"
    done
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

# The splash draws plymouth's spinner theme, and plymouth's client is how the boot's tools reach
# it (cryptsetup's askpass, dracut's emergency shell).
if [ ! -x /usr/bin/plymouth ] || [ ! -e /usr/share/plymouth/themes/spinner/throbber-0001.png ]; then
    echo "error: plymouth and its spinner theme are needed:" >&2
    if [ "$distro" = debian ]; then
        echo "  apt install plymouth plymouth-themes" >&2
    else
        echo "  dnf install plymouth plymouth-theme-spinner" >&2
    fi
    exit 1
fi

# X11 clients reach the session through xwayland-satellite; without it they do not run (Debian
# packages none).
if ! command -v xwayland-satellite >/dev/null; then
    echo "warning: no xwayland-satellite: X11 applications will not run" >&2
fi

for bin in synoik synoik-login synoik-splash; do
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
install -Dm755 "target/$profile/synoik-splash" /usr/local/libexec/synoik-splash

# Users and the greeter's home.
install -Dm644 $r/synoik-system.sysusers /usr/lib/sysusers.d/synoik-system.conf
install -Dm644 $r/synoik-login.sysusers /usr/lib/sysusers.d/synoik-login.conf
install -Dm644 $r/synoik-login.tmpfiles /usr/lib/tmpfiles.d/synoik-login.conf
install -Dm644 $r/synoik-system.tmpfiles /usr/lib/tmpfiles.d/synoik-system.conf
systemd-sysusers synoik-system.conf synoik-login.conf
systemd-tmpfiles --create synoik-login.conf synoik-system.conf

# System units. After switch_root the system compositor is plymouth (docs/fork/boot-splash.md §2.2).
install -Dm644 -t /etc/systemd/system $r/synoik-system.service
# Earlier installs restarted the splash's unit into the outer from a unit of its own.
if [ -e /etc/systemd/system/synoik-splash-handover.service ]; then
    systemctl disable synoik-splash-handover.service || true
    rm -f /etc/systemd/system/synoik-splash-handover.service \
        /etc/systemd/system/synoik-system.service.d/initramfs-tools.conf
fi
# On Debian the login daemon's unit is where the display-manager postinsts look for it ($default_dm).
if [ "$distro" = debian ]; then
    rm -f /etc/systemd/system/synoik-login.service
    install -Dm644 -t /usr/lib/systemd/system $r/synoik-login.service
else
    install -Dm644 -t /etc/systemd/system $r/synoik-login.service
fi
for unit in plymouth-start plymouth-quit-wait; do
    install -Dm644 $r/plymouth-real-root.conf "/etc/systemd/system/$unit.service.d/synoik.conf"
done
install -Dm644 $r/plymouth-reboot-mode.conf /etc/systemd/system/plymouth-reboot.service.d/synoik.conf
for unit in plymouth-poweroff plymouth-halt plymouth-kexec; do
    install -Dm644 $r/plymouth-shutdown-mode.conf "/etc/systemd/system/$unit.service.d/synoik.conf"
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
if [ "$distro" = debian ]; then
    install -Dm644 -t /etc/pam.d $r/pam/debian/synoik-*
else
    install -Dm644 -t /etc/pam.d $r/pam/synoik-*
fi
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

# The user manager's ceiling for the sessions' LimitMEMLOCK (README.md): created, never
# overwritten. It applies once the user managers restart: the reboot below.
if [ ! -e /etc/systemd/system/user@.service.d/memlock.conf ]; then
    install -Dm644 $r/user-memlock.conf /etc/systemd/system/user@.service.d/memlock.conf
fi

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
    restorecon -F /usr/local/bin/synoik-login /usr/local/bin/synoik /usr/local/libexec/synoik-splash
    restorecon -RF /var/lib/synoik-greeter
fi

# A bound fbcon would take the display back from the system compositor whenever a session lets
# go of its VT; mapping every console to a framebuffer that does not exist keeps it off them all,
# deferred takeover included (docs/fork/system-compositor.md §9 S6). The kernel's fbdev client goes
# too: its first modeset scans out a black framebuffer for ~0.5 s before the splash, and the
# splash probes the displays itself (boot-splash.md §2.2.2).
if [ "$distro" = debian ]; then
    want="GRUB_CMDLINE_LINUX=\"\$GRUB_CMDLINE_LINUX $kernel_args\""
    if [ "$(tail -1 "$grub_snippet" 2>/dev/null)" != "$want" ]; then
        install -d "$(dirname "$grub_snippet")"
        printf '%s\n' '# synoik: no text console or fbdev client on the display (scripts/install-display-manager.sh).' \
            "$want" >"$grub_snippet"
        update-grub
    fi
elif [ "$sdboot" = true ]; then
    # Into the entries with the initramfs images, below.
    edit_kernel_cmdline add
else
    grubby --update-kernel=ALL --args="$kernel_args"
fi
echo "Kernel command line: $kernel_args (no text console on the display)."

# The splash replaces plymouth in the initramfs (boot-splash.md §7.4, §7.6); it asks for the
# passwords that unlock disks there too.
echo "Rebuilding the initramfs images..."
if [ "$distro" = debian ]; then
    install -Dm755 $r/initramfs-tools/hook "$itools_hook"
    install -Dm755 $r/initramfs-tools/init-premount "$itools_script"
    install -Dm644 $r/synoik-system-after-cryptsetup.conf "$splash_dropin"
    update-initramfs -u -k all
else
    rm -rf "$dracut_module"
    install -Dm755 $r/dracut/90synoik/module-setup.sh "$dracut_module/module-setup.sh"
    install -Dm644 $r/dracut/90synoik/synoik-splash.service "$dracut_module/synoik-splash.service"
    install -Dm644 $r/dracut/90-synoik.conf "$dracut_conf"
    install -Dm644 $r/synoik-system-after-cryptsetup.conf "$splash_dropin"
    # The initramfs's unit lives on after switch_root: the real root knows it by its file.
    install -Dm644 $r/dracut/90synoik/synoik-splash.service "$splash_unit"
    if [ "$sdboot" = true ]; then
        reinstall_kernels
    else
        dracut -f --regenerate-all
    fi
fi

systemctl daemon-reload
systemctl disable gdm.service || true
systemctl enable synoik-system.service
# The display-manager.service alias is GDM's until now, or an earlier install's: `reenable`, as a
# plain enable keeps an alias that names this unit at the path it moved from.
systemctl reenable --force synoik-login.service
if [ "$distro" = debian ]; then
    if [ -e "$default_dm" ] && [ ! -e "$default_dm.pre-synoik" ] &&
        [ "$(cat "$default_dm")" != /usr/local/bin/synoik-login ]; then
        cp -a "$default_dm" "$default_dm.pre-synoik"
    fi
    echo /usr/local/bin/synoik-login >"$default_dm"
fi

cat <<EOF
synoik is the display manager from the next boot. Reboot to switch.
Going back: sudo $repo/scripts/install-display-manager.sh --use-gdm, then reboot.
EOF
