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
    inst_simple "$moddir/synoik-system.service" "$systemdsystemunitdir/synoik-system.service"
    $SYSTEMCTL -q --root "$initdir" add-wants sysinit.target synoik-system.service
    # The splash's images: loaded at start, so nothing is read after switch_root empties the image.
    local theme=/usr/share/plymouth/themes/spinner
    inst_multiple -o "$theme/watermark.png" "$theme"/throbber-*.png \
        "$theme/lock.png" "$theme/entry.png" "$theme/bullet.png" \
        "$theme/keyboard.png" "$theme/capslock.png"
    # The password prompt's: its text's face, and the system keymap, compiled here, since the image
    # has no xkb data to compile it from.
    inst_simple /usr/share/fonts/adwaita-sans-fonts/AdwaitaSans-Regular.ttf
    mkdir -p "$initdir/usr/share/synoik-splash"
    if ! /usr/local/libexec/synoik-splash --compile-keymap \
        > "$initdir/usr/share/synoik-splash/keymap.xkb"; then
        rm -f "$initdir/usr/share/synoik-splash/keymap.xkb"
        dwarning "synoik: no keymap; the splash cannot ask for passwords"
    fi
    # Forwards password requests to the splash, as 11systemd-ask-password does for plymouth; the
    # splash's unit pulls it in.
    inst_multiple -o \
        "$systemdsystemunitdir/systemd-ask-password-plymouth.path" \
        "$systemdsystemunitdir/systemd-ask-password-plymouth.service" \
        systemd-tty-ask-password-agent
}
