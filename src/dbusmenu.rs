// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The `com.canonical.dbusmenu` model — an app indicator's menu, as the shell holds it.
//!
//! A DBusMenu is a **remote tree**: the client owns it, we ask for its layout, and it changes
//! underneath us while it is open. This module is the plain-data half — what a layout means, once
//! the wire types are gone — and the conversion into [`widget::Menu`]'s entries. The bus half is
//! [`crate::dbus::dbusmenu`]; the plan is `docs/fork/status-notifier-port.md`.
//!
//! GNOME Shell has no DBusMenu support at all, so the reference is the
//! `gnome-shell-extension-appindicator` extension's `dbusMenu.js` (v64), cited throughout.
//!
//! **Untrusted-content seam.** Every label, icon name and id here was chosen by a client. Text is
//! flattened and capped like notification text, the tree's depth and breadth are bounded, and
//! nothing crossing into the model carries a wire type.

use crate::ui::widget::{MenuEntry, MenuItem, Ornament};

/// Cap for one label. Same order as the notification caps.
const MAX_LABEL_BYTES: usize = 512;

/// How deep a client's menu tree may nest before we stop following it. Real menus are two or three
/// levels; a deeper one is either a mistake or an attempt to make us recurse.
pub const MAX_DEPTH: usize = 8;

/// How many items one level may hold. A client with more than this has already lost the user.
pub const MAX_CHILDREN: usize = 512;

/// What kind of row a node is — DBusMenu's `type` property (`dbusMenu.js:78`), whose only defined
/// values are `standard` and `separator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeKind {
    #[default]
    Standard,
    Separator,
}

/// One node of a client's menu, validated.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuNode {
    /// The client's own id for this node — what an `Event` or `AboutToShow` names. The root is 0.
    pub id: i32,
    pub label: String,
    /// `enabled` (default true, `dbusMenu.js:88`).
    pub enabled: bool,
    /// `visible` (default true). An invisible node is not drawn *and* not counted.
    pub visible: bool,
    pub kind: NodeKind,
    pub ornament: Ornament,
    /// `icon-name`, if the client named a themed icon for the row.
    pub icon_name: Option<String>,
    /// Whether the client says this node has children (`children-display == "submenu"`), which can
    /// be true before the children have been fetched — that is what `AboutToShow` is for.
    pub has_submenu: bool,
    pub children: Vec<MenuNode>,
}

impl MenuNode {
    /// A node with the spec's defaults, which are *not* Rust's: `enabled` and `visible` default to
    /// true, so a client that sends neither gets a usable row.
    pub fn new(id: i32) -> Self {
        Self {
            id,
            label: String::new(),
            enabled: true,
            visible: true,
            kind: NodeKind::Standard,
            ornament: Ornament::None,
            icon_name: None,
            has_submenu: false,
            children: Vec::new(),
        }
    }
}

/// Strip GTK mnemonics from a label: `_Quit` is "Quit" with Q underlined, and `__` is a literal
/// underscore.
///
/// The extension does `replace(/_([^_])/, '$1')` (`dbusMenu.js:735`) — a **first-match-only**
/// replace with no handling for the escape, so `_Foo _Bar` keeps its second underscore and
/// `Sign __in` loses one of a pair that means one. Doing it properly is the same amount of code.
pub fn strip_mnemonics(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '_' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // `__` is one literal underscore.
            Some('_') => {
                out.push('_');
                chars.next();
            }
            // `_x` marks x as the mnemonic: drop the marker, keep the letter.
            Some(_) => {}
            // A trailing underscore marks nothing; keep it rather than eat it.
            None => out.push('_'),
        }
    }
    out
}

/// Clean an untrusted label for display.
pub fn clean_label(label: &str) -> String {
    let flattened = crate::notifications::flatten_text(&strip_mnemonics(label));
    crate::notifications::clamp_text(flattened, MAX_LABEL_BYTES)
}

/// Turn a client's nodes into menu entries.
///
/// Invisible nodes vanish entirely — not merely hidden, since a hidden row would still take a slot
/// in the widget's ordering. A node the client marked as a submenu becomes a submenu row *only if
/// it actually has children*: `children-display` is set before `AboutToShow` has been answered, and
/// a chevron on a row that expands to nothing is worse than no chevron.
pub fn to_entries(nodes: &[MenuNode]) -> Vec<MenuEntry> {
    let mut out = Vec::new();
    for node in nodes.iter().filter(|n| n.visible) {
        if node.kind == NodeKind::Separator {
            // A leading separator, or one following another, has nothing to divide. GNOME hides
            // exactly these (`popupMenu.js` `_updateSeparatorVisibility`).
            if matches!(out.last(), None | Some(MenuEntry::Separator)) {
                continue;
            }
            out.push(MenuEntry::Separator);
            continue;
        }

        let mut item = MenuItem::new(node.id as u64, clean_label(&node.label));
        item.enabled = node.enabled;
        item.ornament = node.ornament;
        item.icon = node.icon_name.clone();
        item.children = to_entries(&node.children);
        out.push(MenuEntry::Item(item));
    }

    // A trailing separator divides nothing either.
    if matches!(out.last(), Some(MenuEntry::Separator)) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i32, label: &str) -> MenuNode {
        MenuNode {
            label: label.to_owned(),
            ..MenuNode::new(id)
        }
    }

    #[test]
    fn mnemonics_are_stripped_and_escapes_survive() {
        assert_eq!(strip_mnemonics("_Quit"), "Quit");
        // Every marker, not just the first — the extension's regex stops after one.
        assert_eq!(strip_mnemonics("_Sign _in"), "Sign in");
        // `__` is a literal underscore, which the extension's regex also mishandles.
        assert_eq!(strip_mnemonics("Sign __in"), "Sign _in");
        // A trailing marker marks nothing.
        assert_eq!(strip_mnemonics("Weird_"), "Weird_");
        assert_eq!(strip_mnemonics("nothing to do"), "nothing to do");
    }

    #[test]
    fn an_invisible_node_is_gone_rather_than_hidden() {
        let nodes = vec![
            node(1, "Shown"),
            MenuNode {
                visible: false,
                ..node(2, "Hidden")
            },
            node(3, "Also shown"),
        ];
        let entries = to_entries(&nodes);
        assert_eq!(entries.len(), 2);
        let labels: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i.label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["Shown", "Also shown"]);
    }

    #[test]
    fn separators_that_divide_nothing_are_dropped() {
        let sep = || MenuNode {
            kind: NodeKind::Separator,
            ..MenuNode::new(0)
        };
        let nodes = vec![sep(), node(1, "One"), sep(), sep(), node(2, "Two"), sep()];
        let entries = to_entries(&nodes);
        // Leading, doubled and trailing separators all go; the one real divider stays.
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[1], MenuEntry::Separator));
    }

    #[test]
    fn a_submenu_marker_without_children_is_not_a_submenu() {
        // `children-display` is set before `AboutToShow` has filled the children in, and a chevron
        // that expands to nothing reads as broken.
        let nodes = vec![MenuNode {
            has_submenu: true,
            ..node(1, "Settings")
        }];
        let MenuEntry::Item(item) = &to_entries(&nodes)[0] else {
            panic!("expected an item");
        };
        assert!(item.children.is_empty());

        // With children fetched, it is one.
        let nodes = vec![MenuNode {
            has_submenu: true,
            children: vec![node(11, "Account")],
            ..node(1, "Settings")
        }];
        let MenuEntry::Item(item) = &to_entries(&nodes)[0] else {
            panic!("expected an item");
        };
        assert_eq!(item.children.len(), 1);
    }

    #[test]
    fn a_nodes_state_carries_across() {
        let nodes = vec![
            MenuNode {
                enabled: false,
                ..node(1, "Disabled")
            },
            MenuNode {
                ornament: Ornament::Check(true),
                ..node(2, "Checked")
            },
            MenuNode {
                icon_name: Some("folder-symbolic".to_owned()),
                ..node(3, "With icon")
            },
        ];
        let entries = to_entries(&nodes);
        let items: Vec<&MenuItem> = entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item(i) => Some(i),
                _ => None,
            })
            .collect();

        assert!(!items[0].enabled);
        assert_eq!(items[1].ornament, Ornament::Check(true));
        assert_eq!(items[2].icon.as_deref(), Some("folder-symbolic"));
        // Ids survive as the widget's row ids, which is how an activation names a node back to the
        // client.
        assert_eq!(items[0].id, 1);
    }

    #[test]
    fn a_label_is_flattened_and_capped() {
        let nasty = format!("_Open\nthe {}", "x".repeat(MAX_LABEL_BYTES));
        let cleaned = clean_label(&nasty);
        assert!(cleaned.len() <= MAX_LABEL_BYTES);
        assert!(!cleaned.contains('\n'), "a label is one line");
        assert!(cleaned.starts_with("Open the"), "got {cleaned:?}");
    }
}
