// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Copy / cut / paste for the compositor's *own* entries (lock screen, polkit, run dialog,
//! folder rename, overview search).
//!
//! **Where this lives in GNOME.** Not in `ClutterText` — the clipboard set is `StEntry`'s
//! (`st/st-entry.c:656-740`), layered on top of the events ClutterText left unhandled, because
//! it needs the *selection* and ClutterText is a plain text model. Same split here: the
//! bindings [`TextEdit`](crate::ui::text_edit::TextEdit) owns stay there, and the three that
//! need a Wayland selection live in this module, checked by [`State::deliver_shell_key`]
//! before the key reaches an entry. Nothing collides — the default and Emacs themes both
//! leave `Ctrl-c`/`Ctrl-x`/`Ctrl-v` and `Insert` alone — so the reference's "only if
//! ClutterText declined" ordering is unobservable and we do not reproduce its plumbing.
//!
//! The bindings, from that same block:
//!
//! * **Paste** — `Ctrl-v` or **`Shift-Insert`** (`:665-687`). Not password-guarded: pasting a
//!   password out of a password manager is the point.
//! * **Copy** — `Ctrl-c`, *only* when `clutter_text_get_password_char () == 0` (`:690-712`).
//! * **Cut** — `Ctrl-x`, same guard, then `clutter_text_delete_selection` (`:714-739`).
//!
//! Middle-click PRIMARY paste (`:619-655`, gated on `StSettings:primary-paste`) is **not**
//! here: none of the five entries handles pointer buttons at all yet.
//!
//! **Divergence — a paste is capped and single-line.** GNOME caps nothing, but these five
//! fields hold a search query, a command, a folder name or a password; a clipboard holding a
//! megabyte of log output has no business being pasted into any of them one character at a
//! time. So we read at most [`PASTE_LIMIT`] bytes and insert the **first line** of what came
//! back. Dropping the rest rather than joining the lines is deliberate: joining silently
//! glues words together, and a single-line field cannot show the difference.

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

use smithay::input::keyboard::Keysym;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction, RegistrationToken};
use smithay::reexports::rustix::fs::{fcntl_setfl, OFlags};
use smithay::reexports::rustix::io::Errno;
use smithay::reexports::rustix::pipe::{pipe_with, PipeFlags};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_selection, SelectionRequestError,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection,
    set_primary_selection,
};
use smithay::wayland::selection::SelectionTarget;

use crate::input_method::{ShellEntry, ShellKey};
use crate::synoik::State;
use crate::ui::text_edit::EditMods;
use crate::utils::xwayland::selection::FromX;

/// The most a single paste may bring in, in bytes.
///
/// Generous for anything these entries are for — a command, a folder name, a search term, a
/// passphrase — and small enough that a runaway clipboard owner cannot make us buffer it. It is
/// also what bounds the *delivery*: pasted text goes in one synthetic keystroke per character
/// (see [`State::type_into_shell_entry`]), so every byte here is one pass through an entry's
/// text path and whatever that re-runs.
pub const PASTE_LIMIT: usize = 1024;

/// How long a clipboard owner has to produce the data before we give up.
///
/// The owner is another client, so this must not be unbounded: a wedged one would otherwise
/// leave the event source (and the user's paste) hanging for the life of the session.
const PASTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The text mime types we offer when copying, and prefer — in this order — when pasting.
///
/// GNOME's own list, `st/st-clipboard.c:49-53` (`supported_mimetypes`, consumed in order by
/// `pick_mimetype`).
pub const TEXT_MIME_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

/// What the compositor put on a selection, and how to get the bytes back.
///
/// Smithay hands this back to [`SelectionHandler::send_selection`] when a client pastes a
/// selection the *compositor* owns, which is both of the ways we own one.
///
/// [`SelectionHandler::send_selection`]: smithay::wayland::selection::SelectionHandler::send_selection
#[derive(Debug, Clone)]
pub enum SelectionData {
    /// The shell copied it out of one of its own entries; we hold the bytes.
    Shell(Arc<[u8]>),
    /// An X11 client owns it; the bytes live over there and are fetched on demand through the
    /// bridge. Nothing is converted until someone actually pastes — see
    /// [`crate::utils::xwayland::selection`].
    X11(SelectionTarget),
}

/// One of the three clipboard bindings, recognised from a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardAction {
    Copy,
    Cut,
    Paste,
}

impl ClipboardAction {
    /// Which clipboard binding — if any — this key is.
    ///
    /// The reference tests `state & CLUTTER_CONTROL_MASK` and nothing else, so `Ctrl-Shift-c`
    /// copies there too and does here. Alt and Super are excluded: those belong to the
    /// compositor's binds, not to a field.
    pub fn from_key(sym: Option<Keysym>, mods: EditMods) -> Option<Self> {
        let sym = sym?;
        if mods.alt || mods.logo {
            return None;
        }
        if mods.ctrl {
            return match sym {
                Keysym::c | Keysym::C => Some(Self::Copy),
                Keysym::x | Keysym::X => Some(Self::Cut),
                Keysym::v | Keysym::V => Some(Self::Paste),
                _ => None,
            };
        }
        // Shift-Insert is a paste as far back as X11 (`st-entry.c:669-670`).
        if mods.shift && matches!(sym, Keysym::Insert | Keysym::KP_Insert) {
            return Some(Self::Paste);
        }
        None
    }
}

/// What a paste read accumulates, shared between its fd source and its timeout.
struct Paste {
    entry: ShellEntry,
    buf: Vec<u8>,
    /// The fd source, so the timeout can take it out of the loop.
    source: Cell<Option<RegistrationToken>>,
    /// The timer, so a finished read can cancel it.
    timeout: Cell<Option<RegistrationToken>>,
    /// Set by whichever of the two arms gets there first, so the other becomes a no-op.
    done: Cell<bool>,
}

impl State {
    /// Handle a clipboard binding aimed at one of the compositor's entries.
    ///
    /// Always returns having *claimed* the key: a copy in a password field is refused, not
    /// passed on, exactly as in the reference (`st-entry.c:690-693` returns TRUE for the whole
    /// `Ctrl-c` block only when the guard passes — but a `Ctrl-c` that fell through to a
    /// compositor bind while a password entry has focus would be a far worse surprise than one
    /// that does nothing).
    pub(crate) fn shell_clipboard_action(&mut self, entry: ShellEntry, action: ClipboardAction) {
        match action {
            ClipboardAction::Copy | ClipboardAction::Cut => {
                // `clutter_text_get_password_char () == 0` — a masked field never yields its
                // contents to the clipboard.
                if entry.is_password() {
                    return;
                }
                let Some(text) = self.shell_entry_selection(entry) else {
                    // No selection: nothing to copy, and nothing to cut either. St checks
                    // `strlen (text)` for exactly this.
                    return;
                };
                self.set_clipboard_text(text);
                if action == ClipboardAction::Cut {
                    // Delete through the entry's own key path rather than reaching into its
                    // model, so whatever an edit triggers there (the search re-runs, the
                    // dialogs drop a stale error) happens as it would for a typed BackSpace.
                    // With a selection up, `TextEdit` deletes exactly that.
                    let theme = self.synoik.gnome_settings.key_theme;
                    self.deliver_shell_key(ShellKey {
                        entry,
                        raw: Some(Keysym::BackSpace),
                        text: None,
                        mods: EditMods::default(),
                        theme,
                        pressed: true,
                    });
                }
            }
            ClipboardAction::Paste => self.paste_into_shell_entry(entry),
        }
    }

    /// The selected text of one of the shell's own entries, or `None` when nothing is selected.
    fn shell_entry_selection(&self, entry: ShellEntry) -> Option<String> {
        let edit = match entry {
            ShellEntry::Shield => self.synoik.unlock_dialog.entry(),
            ShellEntry::Polkit => self.synoik.polkit_dialog.entry(),
            // Only the focused field can hold a selection, and only when one is focused at all.
            ShellEntry::NetworkSecret => match self.synoik.network_secret_dialog.focus() {
                crate::network_secret_dialog::Focus::Field(index) => {
                    self.synoik.network_secret_dialog.entry(index)?
                }
                _ => return None,
            },
            ShellEntry::RunDialog => self.synoik.run_dialog.edit(),
            ShellEntry::FolderRename => self.synoik.folder_dialog.rename_edit()?,
            ShellEntry::OverviewSearch => self.synoik.overview_search.edit(),
            ShellEntry::WorkspaceRename => self.synoik.workspace_rename.as_ref()?.edit(),
        };
        let selected = edit.selected_text()?;
        (!selected.is_empty()).then(|| selected.to_owned())
    }

    /// Take ownership of the clipboard with `text`, offering GNOME's three text mime types.
    pub fn set_clipboard_text(&mut self, text: String) {
        self.synoik.set_clipboard(
            TEXT_MIME_TYPES.iter().map(|m| (*m).to_owned()).collect(),
            text.into_bytes().into(),
        );
    }

    /// Act on something the X11 selection bridge saw.
    ///
    /// The other direction — a Wayland selection reaching X11 — is pushed from
    /// [`SelectionHandler::new_selection`] and [`Synoik::set_clipboard`], so it is not here.
    ///
    /// [`SelectionHandler::new_selection`]: smithay::wayland::selection::SelectionHandler::new_selection
    /// [`Synoik::set_clipboard`]: crate::synoik::Synoik::set_clipboard
    pub fn on_x_selection_event(&mut self, event: FromX) {
        match event {
            FromX::Offer { target, mime_types } => self.publish_x_selection(target, mime_types),
            FromX::Cleared { target } => self.clear_x_selection(target),
            FromX::Request {
                target,
                id,
                mime_type,
            } => self.serve_x_selection_request(target, id, mime_type),
        }
    }

    /// An X11 client owns `target`: publish what it offers, fetching nothing yet.
    fn publish_x_selection(&mut self, target: SelectionTarget, mime_types: Vec<String>) {
        let dh = &self.synoik.display_handle;
        match target {
            SelectionTarget::Clipboard => {
                set_data_device_selection(
                    dh,
                    &self.synoik.seat,
                    mime_types.clone(),
                    SelectionData::X11(target),
                );
                self.synoik.clipboard_mime_types = mime_types;
            }
            SelectionTarget::Primary => {
                set_primary_selection(
                    dh,
                    &self.synoik.seat,
                    mime_types,
                    SelectionData::X11(target),
                );
            }
        }
    }

    /// Nothing owns `target` on X11 any more.
    ///
    /// Only clears what *we* published from X11: between the X owner going away and this landing,
    /// a Wayland client may have taken the selection, and wiping that would lose a live clipboard.
    fn clear_x_selection(&mut self, target: SelectionTarget) {
        let ours = match target {
            SelectionTarget::Clipboard => matches!(
                current_data_device_selection_userdata(&self.synoik.seat).as_deref(),
                Some(SelectionData::X11(_))
            ),
            SelectionTarget::Primary => matches!(
                current_primary_selection_userdata(&self.synoik.seat).as_deref(),
                Some(SelectionData::X11(_))
            ),
        };
        if !ours {
            return;
        }

        let dh = &self.synoik.display_handle;
        match target {
            SelectionTarget::Clipboard => {
                clear_data_device_selection(dh, &self.synoik.seat);
                self.synoik.clipboard_mime_types.clear();
            }
            SelectionTarget::Primary => clear_primary_selection(dh, &self.synoik.seat),
        }
    }

    /// An X11 client is pasting what a Wayland client copied: get it the bytes.
    ///
    /// Every request must be answered, including with `None` — an X client blocks until its
    /// `SelectionNotify` arrives.
    fn serve_x_selection_request(&mut self, target: SelectionTarget, id: u64, mime_type: String) {
        let fd = self.x_selection_bytes(target, mime_type);
        if let Some(bridge) = &self.synoik.x_selection {
            bridge.answer(id, fd);
        }
    }

    /// The read end of a pipe the current owner of `target` is filling with `mime_type`.
    fn x_selection_bytes(&mut self, target: SelectionTarget, mime_type: String) -> Option<OwnedFd> {
        let (read, write) = match pipe_with(PipeFlags::CLOEXEC) {
            Ok(pair) => pair,
            Err(err) => {
                warn!("error creating a pipe for an X11 paste: {err:?}");
                return None;
            }
        };

        // The shell's own selection: we have the bytes, so hand them over on a thread rather
        // than round-tripping through a client that is not there.
        let own = match target {
            SelectionTarget::Clipboard => current_data_device_selection_userdata(&self.synoik.seat)
                .and_then(|data| match &*data {
                    SelectionData::Shell(bytes) => Some(bytes.clone()),
                    SelectionData::X11(_) => None,
                }),
            SelectionTarget::Primary => current_primary_selection_userdata(&self.synoik.seat)
                .and_then(|data| match &*data {
                    SelectionData::Shell(bytes) => Some(bytes.clone()),
                    SelectionData::X11(_) => None,
                }),
        };
        if let Some(bytes) = own {
            std::thread::spawn(move || {
                if let Err(err) = std::io::Write::write_all(&mut File::from(write), &bytes) {
                    debug!("error writing the shell selection to X11: {err}");
                }
            });
            return Some(read);
        }

        // The two protocols have their own identically-shaped error type, so the arms report
        // rather than return.
        let ok = match target {
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.synoik.seat, mime_type, write)
                    .inspect_err(|err| debug!("no clipboard to hand X11: {err}"))
                    .is_ok()
            }
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.synoik.seat, mime_type, write)
                    .inspect_err(|err| debug!("no primary selection to hand X11: {err}"))
                    .is_ok()
            }
        };
        ok.then_some(read)
    }

    /// Start a paste into `entry`.
    ///
    /// The clipboard owner is another client, so the data arrives over a pipe *later*; this
    /// only kicks the transfer off. When the compositor itself owns the selection (our own
    /// copy, or a screenshot) the bytes are already here and go in straight away.
    fn paste_into_shell_entry(&mut self, entry: ShellEntry) {
        let Some(mime) = TEXT_MIME_TYPES
            .iter()
            .find(|m| self.synoik.clipboard_mime_types.iter().any(|o| o == *m))
        else {
            // Nothing on the clipboard, or nothing textual on it (an image, say).
            return;
        };
        let mime = (*mime).to_owned();

        // The shell's own selection: no round trip, and no fd source to leak.
        let own = current_data_device_selection_userdata(&self.synoik.seat).and_then(|data| {
            match &*data {
                SelectionData::Shell(bytes) => Some(paste_text(bytes)),
                // An X11 client owns it; the bytes are over there. Falls through to the pipe.
                SelectionData::X11(_) => None,
            }
        });
        if let Some(text) = own {
            self.type_into_shell_entry(entry, &text);
            return;
        }

        // Where the data has to come from. Read before the pipe so a missing bridge costs
        // nothing.
        let from_x = match current_data_device_selection_userdata(&self.synoik.seat).as_deref() {
            Some(SelectionData::X11(target)) => Some(*target),
            _ => None,
        };
        if from_x.is_some() && self.synoik.x_selection.is_none() {
            return;
        }

        // One at a time. Holding Ctrl-v down would otherwise stack an fd source and a timer per
        // repeat, all of them inserting into the same field when they land.
        if self.synoik.clipboard_paste_pending {
            return;
        }

        let (read, write) = match pipe_with(PipeFlags::CLOEXEC) {
            Ok(pair) => pair,
            Err(err) => {
                warn!("error creating a pipe for the paste: {err:?}");
                return;
            }
        };
        // The source below is level-triggered on the main loop; a blocking read there would
        // stall every frame until the owner got around to writing.
        if let Err(err) = fcntl_setfl(&read, OFlags::NONBLOCK) {
            warn!("error setting the paste pipe non-blocking: {err:?}");
            return;
        }

        match from_x {
            // An X11 client owns it: the bridge converts the selection and fills the pipe. It is
            // compositor-owned as far as smithay is concerned, so the request below would only
            // ever come back `ServerSideSelection`.
            Some(target) => {
                let bridge = self.synoik.x_selection.as_ref().unwrap();
                bridge.fetch(target, mime, write);
            }
            None => match request_data_device_client_selection(&self.synoik.seat, mime, write) {
                Ok(()) => (),
                // All three mean the selection changed between the mime-type check above and now.
                Err(
                    err @ (SelectionRequestError::NoSelection
                    | SelectionRequestError::InvalidMimetype
                    | SelectionRequestError::ServerSideSelection),
                ) => {
                    debug!("no clipboard text to paste: {err}");
                    return;
                }
            },
        }

        let paste = Rc::new(RefCell::new(Paste {
            entry,
            buf: Vec::new(),
            source: Cell::new(None),
            timeout: Cell::new(None),
            done: Cell::new(false),
        }));

        let source = Generic::new(read, Interest::READ, Mode::Level);
        let state = Rc::clone(&paste);
        let token = self
            .synoik
            .event_loop
            .insert_source(source, move |_, fd, this| {
                let mut paste = state.borrow_mut();
                match read_into(fd, &mut paste.buf) {
                    Read::More => Ok(PostAction::Continue),
                    Read::Done => {
                        paste.done.set(true);
                        let entry = paste.entry;
                        let text = paste_text(&paste.buf);
                        if let Some(timeout) = paste.timeout.take() {
                            this.synoik.event_loop.remove(timeout);
                        }
                        drop(paste);
                        this.synoik.clipboard_paste_pending = false;
                        this.type_into_shell_entry(entry, &text);
                        Ok(PostAction::Remove)
                    }
                }
            });
        let token = match token {
            Ok(token) => token,
            Err(err) => {
                warn!("error watching the paste pipe: {err:?}");
                return;
            }
        };

        paste.borrow().source.set(Some(token));

        let timer = smithay::reexports::calloop::timer::Timer::from_duration(PASTE_TIMEOUT);
        let state = Rc::clone(&paste);
        let timeout = self
            .synoik
            .event_loop
            .insert_source(timer, move |_, _, this| {
                let paste = state.borrow();
                if !paste.done.get() {
                    if let Some(source) = paste.source.take() {
                        this.synoik.event_loop.remove(source);
                    }
                    warn!("the clipboard owner never finished writing; paste dropped");
                    this.synoik.clipboard_paste_pending = false;
                }
                smithay::reexports::calloop::timer::TimeoutAction::Drop
            });
        match timeout {
            Ok(timeout) => paste.borrow().timeout.set(Some(timeout)),
            Err(err) => warn!("error arming the paste timeout: {err:?}"),
        }
        self.synoik.clipboard_paste_pending = true;
    }

    /// Put `text` into one of the compositor's entries as if it had been typed.
    ///
    /// One synthetic keystroke per character, for the same reason committed input-method text
    /// takes this route (see `commit_into_shell_entry`): every entry already has a text path
    /// with its own side effects, and reusing it means pasted text behaves exactly like typed
    /// text because it *is* the same path. `TextEdit` refuses control characters, so a stray
    /// tab or carriage return in the clipboard cannot land in a one-line field.
    pub(crate) fn type_into_shell_entry(&mut self, entry: ShellEntry, text: &str) {
        if text.is_empty() {
            return;
        }
        let theme = self.synoik.gnome_settings.key_theme;
        for ch in text.chars() {
            self.deliver_shell_key(ShellKey {
                entry,
                raw: None,
                text: Some(ch),
                mods: EditMods::default(),
                theme,
                pressed: true,
            });
        }
        self.synoik.queue_redraw_all();
    }
}

enum Read {
    More,
    Done,
}

/// Drain what the pipe has, up to [`PASTE_LIMIT`] in total.
fn read_into(fd: &impl std::os::fd::AsFd, buf: &mut Vec<u8>) -> Read {
    loop {
        let remaining = PASTE_LIMIT.saturating_sub(buf.len());
        if remaining == 0 {
            // Everything we are willing to take. The write end is dropped with the source, so
            // the owner learns to stop.
            return Read::Done;
        }
        let mut chunk = [0u8; 4096];
        let want = remaining.min(chunk.len());
        match smithay::reexports::rustix::io::read(fd, &mut chunk[..want]) {
            Ok(0) => return Read::Done,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(Errno::INTR) => (),
            Err(Errno::AGAIN) => return Read::More,
            Err(err) => {
                warn!("error reading the clipboard: {err:?}");
                return Read::Done;
            }
        }
    }
}

/// What actually gets inserted: the first line of the bytes a clipboard owner sent, capped at
/// [`PASTE_LIMIT`].
///
/// The cap is applied here as well as at the read, because a selection the compositor owns
/// never goes through the pipe. See the module docs for why only the first line.
/// `from_utf8_lossy` rather than a refusal: a cap can land mid-character, and losing the last
/// glyph of a truncated paste beats dropping the paste.
fn paste_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.split(['\n', '\r']).next().unwrap_or("");
    // Back off to a character boundary — a `String` cannot be cut mid-code-point.
    let mut end = line.len().min(PASTE_LIMIT);
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_bindings_are_recognised() {
        let ctrl = EditMods::ctrl();
        assert_eq!(
            ClipboardAction::from_key(Some(Keysym::c), ctrl),
            Some(ClipboardAction::Copy)
        );
        assert_eq!(
            ClipboardAction::from_key(Some(Keysym::X), ctrl),
            Some(ClipboardAction::Cut)
        );
        assert_eq!(
            ClipboardAction::from_key(Some(Keysym::v), ctrl),
            Some(ClipboardAction::Paste)
        );
        assert_eq!(
            ClipboardAction::from_key(Some(Keysym::Insert), EditMods::shift()),
            Some(ClipboardAction::Paste),
            "Shift-Insert pastes (st-entry.c:669-670)"
        );
    }

    #[test]
    fn an_unmodified_or_super_key_is_never_a_clipboard_binding() {
        assert_eq!(
            ClipboardAction::from_key(Some(Keysym::c), EditMods::default()),
            None
        );
        assert_eq!(
            ClipboardAction::from_key(Some(Keysym::Insert), EditMods::default()),
            None,
            "plain Insert is not a paste"
        );
        let logo = EditMods {
            ctrl: true,
            logo: true,
            ..EditMods::default()
        };
        assert_eq!(ClipboardAction::from_key(Some(Keysym::v), logo), None);
    }

    #[test]
    fn a_paste_is_the_first_line_only() {
        assert_eq!(paste_text(b"one\ntwo\nthree"), "one");
        assert_eq!(paste_text(b"one\r\ntwo"), "one");
        assert_eq!(paste_text(b"just one"), "just one");
        assert_eq!(paste_text(b""), "");
    }

    #[test]
    fn a_truncated_multibyte_paste_still_yields_text() {
        // The cap can fall mid-character; the paste must survive it.
        let mut bytes = "café".as_bytes().to_vec();
        bytes.pop();
        assert!(paste_text(&bytes).starts_with("caf"));
    }
}
