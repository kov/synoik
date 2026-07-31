// Dump the live actor tree, with the *resolved* box model, as JSON.
//
// Why this exists: porting a widget from the SCSS means reading a cascade and hoping you stacked
// the containers the way St does. Measuring a screenshot tells you where an edge landed but never
// which container put it there — and a wrong guess looks identical to a right one until some other
// widget disagrees. This asks the running shell instead: for each actor, its allocation *and* what
// `StThemeNode` actually resolved for padding, margin, border, radius, spacing and font.
//
// It reads; it never changes anything.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.gnome.Shell.Extensions.UiDump';
const OBJECT_PATH = '/org/gnome/Shell/Extensions/UiDump';

const IFACE = `
<node>
  <interface name="org.gnome.Shell.Extensions.UiDump">
    <!-- Dump the whole stage. Big; prefer DumpClass. -->
    <method name="DumpAll">
      <arg type="s" direction="in" name="path"/>
      <arg type="s" direction="out" name="result"/>
    </method>
    <!-- Dump every subtree whose root carries this style class, e.g. "message" or
         "datemenu-calendar-column". This is the one you usually want. -->
    <method name="DumpClass">
      <arg type="s" direction="in" name="styleClass"/>
      <arg type="s" direction="in" name="path"/>
      <arg type="s" direction="out" name="result"/>
    </method>
    <!-- Same, but matched on the actor's name (#calendarArea -> "calendarArea"). -->
    <method name="DumpName">
      <arg type="s" direction="in" name="name"/>
      <arg type="s" direction="in" name="path"/>
      <arg type="s" direction="out" name="result"/>
    </method>
    <!-- Wait, then dump: gives you time to open a menu by hand before it runs. -->
    <method name="DumpClassAfter">
      <arg type="s" direction="in" name="styleClass"/>
      <arg type="s" direction="in" name="path"/>
      <arg type="u" direction="in" name="delaySeconds"/>
      <arg type="s" direction="out" name="result"/>
    </method>
  </interface>
</node>`;

/** Depth limit, so a stray DumpAll cannot walk the shell into a stall. */
const MAX_DEPTH = 40;

function colorOf(color) {
    if (!color)
        return null;
    // Cogl.Color components have been both 0-255 integers and 0-1 floats across versions; emit the
    // raw numbers and let the reader decide rather than guessing and silently reporting black.
    const c = [color.red, color.green, color.blue, color.alpha];
    let str = null;
    try {
        str = color.to_string();
    } catch {
        str = null;
    }
    return {raw: c, string: str};
}

/** The theme-node values that decide a box's geometry. Every lookup is guarded: a theme node can
 *  refuse any of these depending on how the actor was styled, and one failure must not lose the
 *  whole dump. */
function themeOf(actor) {
    if (!(actor instanceof St.Widget))
        return null;
    let node;
    try {
        node = actor.get_theme_node();
    } catch {
        return null;
    }

    const side = (fn) => {
        try {
            return [
                fn(St.Side.TOP), fn(St.Side.RIGHT),
                fn(St.Side.BOTTOM), fn(St.Side.LEFT),
            ];
        } catch {
            return null;
        }
    };
    const length = (name) => {
        try {
            const [ok, value] = node.lookup_length(name, false);
            return ok ? value : null;
        } catch {
            return null;
        }
    };
    const guarded = (fn) => {
        try {
            return fn();
        } catch {
            return null;
        }
    };

    return {
        // top, right, bottom, left — the order the CSS shorthand uses.
        padding: side((s) => node.get_padding(s)),
        margin: side((s) => node.get_margin(s)),
        border_width: side((s) => node.get_border_width(s)),
        border_radius: guarded(() => [
            node.get_border_radius(St.Corner.TOPLEFT),
            node.get_border_radius(St.Corner.TOPRIGHT),
            node.get_border_radius(St.Corner.BOTTOMRIGHT),
            node.get_border_radius(St.Corner.BOTTOMLEFT),
        ]),
        // `spacing` is a plain theme length St feeds to the layout manager, and it is what makes
        // an icon-to-text gap differ from the icon's own margin — the two stack.
        spacing: length('spacing'),
        min_width: length('min-width'),
        min_height: length('min-height'),
        width: length('width'),
        height: length('height'),
        icon_size: guarded(() => node.get_icon_size()),
        background_color: guarded(() => colorOf(node.get_background_color())),
        color: guarded(() => colorOf(node.get_foreground_color())),
        font: guarded(() => node.get_font()?.to_string() ?? null),
    };
}

function describe(actor, depth) {
    let abs = null;
    try {
        const [x, y] = actor.get_transformed_position();
        abs = [x, y];
    } catch {
        abs = null;
    }

    const node = {
        type: actor.constructor?.$gtype?.name ?? `${actor}`,
        name: actor.name || null,
        style_class: actor.style_class || null,
        // Inline style beats the stylesheet, so a surprising value is often here.
        inline_style: actor.style || null,
        visible: actor.visible,
        mapped: actor.mapped,
        // Absolute stage coordinates, and the allocation size.
        abs,
        size: [actor.width, actor.height],
        alloc: [actor.x, actor.y],
        theme: themeOf(actor),
        children: [],
    };

    if (depth < MAX_DEPTH) {
        for (const child of actor.get_children())
            node.children.push(describe(child, depth + 1));
    } else {
        node.truncated = true;
    }
    return node;
}

/** Every actor in the tree matching `pred`, without descending into a match (the subtree is the
 *  dump). */
function findAll(actor, pred, out, depth = 0) {
    if (depth > MAX_DEPTH)
        return out;
    if (pred(actor)) {
        out.push(actor);
        return out;
    }
    for (const child of actor.get_children())
        findAll(child, pred, out, depth + 1);
    return out;
}

function hasClass(actor, styleClass) {
    const classes = actor.style_class;
    if (!classes)
        return false;
    return classes.split(/\s+/).includes(styleClass);
}

/** Where the matched actor sits, so a dump carries the containers that contributed to its
 *  position — the whole point when an inset comes from two nested paddings. */
function ancestry(actor) {
    const chain = [];
    let cursor = actor.get_parent();
    while (cursor) {
        let abs = null;
        try {
            const [x, y] = cursor.get_transformed_position();
            abs = [x, y];
        } catch {
            abs = null;
        }
        chain.push({
            type: cursor.constructor?.$gtype?.name ?? `${cursor}`,
            name: cursor.name || null,
            style_class: cursor.style_class || null,
            abs,
            size: [cursor.width, cursor.height],
            theme: themeOf(cursor),
        });
        cursor = cursor.get_parent();
    }
    return chain;
}

export default class UiDumpExtension extends Extension {
    enable() {
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameOwnerFlags.NONE,
            null, null, null);
    }

    disable() {
        if (this._timeout) {
            GLib.source_remove(this._timeout);
            this._timeout = null;
        }
        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = null;
        }
        this._dbus?.unexport();
        this._dbus = null;
    }

    _write(path, payload) {
        const json = JSON.stringify(payload, null, 2);
        try {
            GLib.file_set_contents(path, json);
        } catch (e) {
            return `error writing ${path}: ${e}`;
        }
        return `wrote ${path} (${json.length} bytes)`;
    }

    DumpAll(path) {
        return this._write(path, {
            shell_version: imports.misc.config?.PACKAGE_VERSION ?? null,
            root: describe(global.stage, 0),
        });
    }

    _dumpMatching(pred, label, path) {
        const found = findAll(global.stage, pred, []);
        if (found.length === 0)
            return `no actor matched ${label}`;
        return this._write(path, {
            match: label,
            count: found.length,
            matches: found.map((actor) => ({
                ancestry: ancestry(actor),
                actor: describe(actor, 0),
            })),
        });
    }

    DumpClass(styleClass, path) {
        return this._dumpMatching(
            (actor) => hasClass(actor, styleClass), `.${styleClass}`, path);
    }

    DumpName(name, path) {
        return this._dumpMatching(
            (actor) => actor.name === name, `#${name}`, path);
    }

    DumpClassAfter(styleClass, path, delaySeconds) {
        if (this._timeout)
            GLib.source_remove(this._timeout);
        this._timeout = GLib.timeout_add_seconds(
            GLib.PRIORITY_DEFAULT, delaySeconds || 5, () => {
                this._timeout = null;
                const result = this.DumpClass(styleClass, path);
                log(`[ui-dump] ${result}`);
                return GLib.SOURCE_REMOVE;
            });
        return `will dump .${styleClass} to ${path} in ${delaySeconds || 5}s ` +
            `(open the UI now; result goes to the journal)`;
    }
}
