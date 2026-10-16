#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

# The synoik splash in the initramfs, in plymouth's place (docs/fork/boot-splash.md §7.4).

# Only when asked for: /etc/dracut.conf.d/90-synoik.conf adds it, and omits plymouth.
check() {
    require_binaries /usr/local/libexec/synoik-splash || return 1
    return 255
}

# The GPU drivers, so the card is there to draw on; systemd's password agents, whose plymouth one
# asks the splash.
depends() {
    echo drm systemd systemd-ask-password
}

install() {
    inst_binary /usr/local/libexec/synoik-splash
    # A release build carries its debug info, ten times the code; dracut does not strip. The
    # symbols stay, for a panic's backtrace.
    if command -v strip >/dev/null; then
        strip --strip-debug "$initdir/usr/local/libexec/synoik-splash" ||
            dwarning "synoik: could not strip the splash"
    fi
    inst_simple "$moddir/synoik-splash.service" "$systemdsystemunitdir/synoik-splash.service"
    $SYSTEMCTL -q --root "$initdir" add-wants sysinit.target synoik-splash.service
    # The splash's images: loaded at start, so nothing is read after switch_root empties the image.
    # Debian's logo is the watermark where its spinner has none (synoik-splash's layout.rs).
    local theme=/usr/share/plymouth/themes/spinner
    inst_multiple -o "$theme/watermark.png" "$theme"/throbber-*.png \
        "$theme/lock.png" "$theme/entry.png" "$theme/bullet.png" \
        "$theme/keyboard.png" "$theme/capslock.png" \
        /usr/share/desktop-base/debian-logos/logo-text-version-64.png
    # The password prompt's: its text's face, and the system keymap, compiled here, since the image
    # has no xkb data to compile it from.
    local font
    font=$(fc-match -f '%{file}' 'Adwaita Sans')
    case "$font" in
        *AdwaitaSans*) inst_simple "$font" /usr/share/synoik-splash/sans.ttf ;;
        *) dwarning "synoik: no Adwaita Sans; the splash draws no text" ;;
    esac
    mkdir -p "$initdir/usr/share/synoik-splash"
    if ! /usr/local/libexec/synoik-splash --compile-keymap \
        > "$initdir/usr/share/synoik-splash/keymap.xkb"; then
        rm -f "$initdir/usr/share/synoik-splash/keymap.xkb"
        dwarning "synoik: no keymap; the splash cannot ask for passwords"
    fi
    # plymouth's client, as 45plymouth installs it: dracut's emergency shell quits the splash
    # through it, and does nothing without it.
    inst_multiple -o plymouth
    # Forwards password requests to the splash, as 11systemd-ask-password does for plymouth; the
    # splash's unit pulls it in.
    inst_multiple -o \
        "$systemdsystemunitdir/systemd-ask-password-plymouth.path" \
        "$systemdsystemunitdir/systemd-ask-password-plymouth.service" \
        systemd-tty-ask-password-agent
}
