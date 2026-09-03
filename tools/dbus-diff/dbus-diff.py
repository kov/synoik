#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
#
# Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

"""Diff the D-Bus surface we serve against GNOME's reference XML.

Usage:  tools/dbus-diff/dbus-diff.py [--refs DIR ...] > /tmp/dbusdiff.txt

Reference XML comes from the read-only checkouts (`~/Projects/mutter`,
`~/Projects/gnome-shell`); ours from `#[interface(name = ...)]` blocks under `src/`.

Caveat that no name diff can close: **a member that is present may still be a stub.**
`DisplayConfig::night_light_supported` returns a hardcoded `false` and counts as served.
Treat a full row as "the name resolves", never as "the behaviour is there".
"""

import argparse
import glob
import os
import re
import xml.etree.ElementTree as ET

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DEFAULT_REFS = [
    os.path.expanduser("~/Projects/mutter/data/dbus-interfaces"),
    os.path.expanduser("~/Projects/gnome-shell/data/dbus-interfaces"),
]
# Names we serve nothing for and never will; reported separately rather than as gaps.
DELIBERATE = {
    "org.gnome.Mutter.X11",
    "org.gnome.Mutter.Devkit",
    "org.gnome.Mutter.DebugControl",
    "org.gnome.Shell.PerfHelper",
    "org.gnome.Shell.CalendarServer",
    "org.gnome.Shell.HotplugSniffer",
}


def load_reference(dirs):
    ref = {}
    for d in dirs:
        for f in sorted(glob.glob(os.path.join(d, "*.xml"))):
            root = ET.parse(f).getroot()
            nodes = [root] if root.tag == "interface" else root.iter("interface")
            for iface in nodes:
                entry = ref.setdefault(
                    iface.get("name"), {"file": os.path.basename(f), "m": set()}
                )
                for child in iface:
                    if child.tag in ("method", "signal", "property"):
                        entry["m"].add(child.tag[0] + ":" + child.get("name"))
    return ref


def pascal(name):
    return "".join(part.capitalize() for part in name.split("_") if part)


def load_ours():
    """Map each `#[interface(name = ...)]` block to the members it exports.

    zbus derives the member name from the fn name unless `#[zbus(name = ...)]` says
    otherwise, and the attribute decides method/signal/property — so both are honoured.
    """
    ours = {}
    for path in glob.glob(os.path.join(ROOT, "src/**/*.rs"), recursive=True):
        src = open(path).read()
        if "#[interface(" not in src:
            continue
        rel = os.path.relpath(path, ROOT)
        current, pending = None, []
        for line in src.split("\n"):
            named = re.search(r'#\[interface\(name\s*=\s*"([^"]+)"', line)
            if named:
                current = ours.setdefault(named.group(1), {"file": rel, "m": set()})
                continue
            if current is None:
                continue
            stripped = line.strip()
            if stripped.startswith("#[zbus("):
                pending.append(stripped)
                continue
            fn = re.match(r"(?:pub )?(?:async )?fn\s+(\w+)", stripped)
            if fn:
                kind, name = "m", None
                for attr in pending:
                    renamed = re.search(r'name\s*=\s*"([^"]+)"', attr)
                    if renamed:
                        name = renamed.group(1)
                    if "signal" in attr:
                        kind = "s"
                    if "property" in attr:
                        kind = "p"
                current["m"].add(kind + ":" + (name or pascal(fn.group(1))))
            pending = []
    return ours


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--refs", nargs="*", default=DEFAULT_REFS)
    args = ap.parse_args()

    ref, ours = load_reference(args.refs), load_ours()
    for name in sorted(set(ref) | set(ours)):
        r, o = ref.get(name), ours.get(name)
        if o is None:
            if name in DELIBERATE or not name.startswith(("org.gnome.Shell", "org.gnome.Mutter")):
                continue
            print(f"=== {name}: ABSENT ({r['file']}, {len(r['m'])} members)")
        elif r is None:
            print(f"=== {name}: ours only [{o['file']}]")
        else:
            print(f"=== {name} [{o['file']}] have {len(o['m'] & r['m'])}/{len(r['m'])}")
            missing = sorted(r["m"] - o["m"])
            if missing:
                print("    MISSING:", ", ".join(missing))


if __name__ == "__main__":
    main()
