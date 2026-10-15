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

# The GPU drivers, so the card is there to draw on.
depends() {
    echo drm systemd
}

install() {
    inst_binary /usr/local/libexec/synoik-splash
    inst_simple "$moddir/synoik-system.service" "$systemdsystemunitdir/synoik-system.service"
    $SYSTEMCTL -q --root "$initdir" add-wants sysinit.target synoik-system.service
    # The splash's images: loaded at start, so nothing is read after switch_root empties the image.
    local theme=/usr/share/plymouth/themes/spinner
    inst_multiple -o "$theme/watermark.png" "$theme"/throbber-*.png
}
