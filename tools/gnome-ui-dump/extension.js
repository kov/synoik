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

import * as BoxPointer from 'resource:///org/gnome/shell/ui/boxpointer.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
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
    <!-- Many classes into one file. An audit wants a dozen widgets from one open menu, and every
         extra round trip is another chance for the menu to close between calls. -->
    <method name="DumpClasses">
      <arg type="as" direction="in" name="styleClasses"/>
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
    <!-- The one an audit actually runs: schedule it, open the UI by hand, get every widget. -->
    <method name="DumpClassesAfter">
      <arg type="as" direction="in" name="styleClasses"/>
      <arg type="s" direction="in" name="path"/>
      <arg type="u" direction="in" name="delaySeconds"/>
      <arg type="s" direction="out" name="result"/>
    </method>
    <!-- Open a panel menu ourselves, wait for it to be *allocated*, then dump. Prefer this to
         the *After methods for anything in a menu: it cannot race the human. -->
    <method name="DumpMenu">
      <arg type="s" direction="in" name="menuName"/>
      <arg type="as" direction="in" name="styleClasses"/>
      <arg type="s" direction="in" name="path"/>
      <arg type="s" direction="out" name="result"/>
    </method>
    <!-- Same for the overview, which is likewise only allocated once shown. -->
    <method name="DumpOverview">
      <arg type="as" direction="in" name="styleClasses"/>
      <arg type="s" direction="in" name="path"/>
      <arg type="s" direction="out" name="result"/>
    </method>
  </interface>
</node>`;

/** Depth limit, so a stray DumpAll cannot walk the shell into a stall. */
const MAX_DEPTH = 40;

/** ~1s of 50ms polls waiting for a just-opened menu to be laid out, then dump anyway and say so. */
const MAX_ALLOCATION_POLLS = 20;

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
        // A grid sets these instead of `spacing` (`.quick-settings-grid`), and reading only
        // `spacing` reports null — which looks like "no gap" rather than "asked the wrong name".
        spacing_rows: length('spacing-rows'),
        spacing_columns: length('spacing-columns'),
        min_width: length('min-width'),
        min_height: length('min-height'),
        width: length('width'),
        height: length('height'),
        icon_size: guarded(() => node.get_icon_size()),
        background_color: guarded(() => colorOf(node.get_background_color())),
        // Per-side, because a border can differ by edge (`.message-list` borders only its right).
        // Without this a dump proves a border EXISTS but not what colour to draw it, which leaves
        // the reader deriving from the stylesheet — the thing the tool exists to avoid.
        border_color: guarded(() => [
            colorOf(node.get_border_color(St.Side.TOP)),
            colorOf(node.get_border_color(St.Side.RIGHT)),
            colorOf(node.get_border_color(St.Side.BOTTOM)),
            colorOf(node.get_border_color(St.Side.LEFT)),
        ]),
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

/** Say so, in the D-Bus reply, when a group matched only unmapped actors — the menu was not open
 *  when the dump ran and its allocations are preferred sizes. (This is not hypothetical: a
 *  quick-settings dump fired a moment before the menu opened and reported a whole plausible box
 *  model for a tree that had never been laid out.) */
function unmappedWarning(groups) {
    const stale = groups.filter((g) => g.count > 0 && g.mapped === 0).map((g) => g.match);
    return stale.length
        ? `; WARNING unmapped (sizes are preferred, not allocated): ${stale.join(' ')}`
        : '';
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
        const group = this._group(pred, label);
        if (group.count === 0)
            return `no actor matched ${label}`;
        return `${this._write(path, group)}${unmappedWarning([group])}`;
    }

    _group(pred, label) {
        const found = findAll(global.stage, pred, []);
        return {
            match: label,
            count: found.length,
            // Hoisted out of the tree so a reader cannot miss it: an unmapped actor still reports
            // a size, but it is a *preferred* size, not an allocation. Reading a box model off
            // one silently measures a menu that was never on screen.
            mapped: found.filter((a) => a.mapped).length,
            matches: found.map((actor) => ({
                ancestry: ancestry(actor),
                actor: describe(actor, 0),
            })),
        };
    }

    DumpClass(styleClass, path) {
        return this._dumpMatching(
            (actor) => hasClass(actor, styleClass), `.${styleClass}`, path);
    }

    DumpName(name, path) {
        return this._dumpMatching(
            (actor) => actor.name === name, `#${name}`, path);
    }

    // A class that matched nothing is recorded as `count: 0` rather than dropped: in a batch, "the
    // widget is not on screen" and "I misspelled the class" are the two likeliest outcomes, and a
    // silently missing key looks like neither.
    DumpClasses(styleClasses, path) {
        const groups = styleClasses.map((styleClass) => this._group(
            (actor) => hasClass(actor, styleClass), `.${styleClass}`));
        const missing = groups.filter((g) => g.count === 0).map((g) => g.match);
        let result = this._write(path, {groups});
        if (missing.length)
            result += `; nothing matched ${missing.join(' ')}`;
        return result + unmappedWarning(groups);
    }

    _after(delaySeconds, label, run) {
        if (this._timeout)
            GLib.source_remove(this._timeout);
        this._timeout = GLib.timeout_add_seconds(
            GLib.PRIORITY_DEFAULT, delaySeconds || 5, () => {
                this._timeout = null;
                log(`[ui-dump] ${run()}`);
                return GLib.SOURCE_REMOVE;
            });
        return `will dump ${label} in ${delaySeconds || 5}s ` +
            `(open the UI now; result goes to the journal)`;
    }

    DumpClassAfter(styleClass, path, delaySeconds) {
        return this._after(
            delaySeconds, `.${styleClass} to ${path}`,
            () => this.DumpClass(styleClass, path));
    }

    DumpClassesAfter(styleClasses, path, delaySeconds) {
        return this._after(
            delaySeconds, `${styleClasses.length} classes to ${path}`,
            () => this.DumpClasses(styleClasses, path));
    }

    // Open the menu ourselves rather than asking a human to. Every hand-opened dump of the date
    // menu came back `mapped: false` — a scheduled dump races the person opening the UI, and an
    // unmapped actor answers with a *preferred* size that looks exactly like an allocation. Here
    // the open is ours, so we can wait for the thing we actually need: a laid-out actor.
    DumpMenu(menuName, styleClasses, path) {
        const item = Main.panel.statusArea[menuName];
        if (!item) {
            const have = Object.keys(Main.panel.statusArea).join(' ');
            return `no panel status area item named ${menuName}; have: ${have}`;
        }
        if (!item.menu)
            return `${menuName} has no menu`;

        const menu = item.menu;
        // NONE, not FULL: an animated open is allocated *during* the animation, so dumping mid-way
        // reads sizes off a frame that is still growing.
        return this._dumpOpened(
            menuName, styleClasses, path,
            menu.isOpen,
            () => menu.open(BoxPointer.PopupAnimation.NONE),
            () => menu.close(BoxPointer.PopupAnimation.NONE));
    }

    // The overview is not a menu, but it has the same hazard: its chrome is only allocated once
    // shown, so a dump taken with it closed measures preferred sizes.
    DumpOverview(styleClasses, path) {
        return this._dumpOpened(
            'overview', styleClasses, path,
            Main.overview.visible,
            () => Main.overview.show(),
            () => Main.overview.hide());
    }

    // Open (if needed), wait for allocation, dump, and put the surface back the way we found it.
    _dumpOpened(label, styleClasses, path, wasOpen, open, close) {
        if (!wasOpen)
            open();
        this._whenAllocated(styleClasses, (ready) => {
            const result = this.DumpClasses(styleClasses, path);
            if (!wasOpen)
                close();
            log(`[ui-dump] ${result}${ready ? '' : ' (WARNING: gave up waiting for allocation)'}`);
        });
        return `opened ${label}; dumping when allocated (result goes to the journal)`;
    }

    // Poll until every requested class has at least one mapped match with a non-zero allocation,
    // then hand over. Polling rather than a single `later_add`: the menu's children are built
    // lazily on open, so "one layout pass from now" is not reliably late enough.
    _whenAllocated(styleClasses, done) {
        let tries = 0;
        const poll = () => {
            tries++;
            const ready = styleClasses.every((styleClass) => {
                const found = findAll(
                    global.stage, (actor) => hasClass(actor, styleClass), []);
                return found.some(
                    (a) => a.mapped && a.get_allocation_box().get_width() > 0);
            });
            if (!ready && tries < MAX_ALLOCATION_POLLS)
                return GLib.SOURCE_CONTINUE;
            this._timeout = null;
            done(ready);
            return GLib.SOURCE_REMOVE;
        };
        if (this._timeout)
            GLib.source_remove(this._timeout);
        this._timeout = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 50, poll);
    }
}
