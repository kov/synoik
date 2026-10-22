// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

//! `org.freedesktop.a11y.KeyboardMonitor` — mutter's `meta-a11y-manager.c`, the interface Orca
//! drives on Wayland to hear keys and to grab its own (the Orca modifier, its commands).
//!
//! Only Orca may call it: mutter answers the owner of `org.gnome.Orca.KeyboardMonitor` and denies
//! everyone else (`meta-a11y-manager.c:409-411`), since a watch is a keylogger and a grab takes
//! the keyboard. The modifier double-press — a grabbed modifier tapped alone is Orca's, tapped
//! twice quickly is the key's own — is `is_a11y_modifier_first_click`
//! (`meta-seat-impl.c:520-568`), which mutter runs before the keymap sees the key.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use smithay::backend::input::{KeyState, Keycode};
use smithay::input::keyboard::{xkb, Keysym};
use zbus::blocking::object_server::InterfaceRef;
use zbus::fdo::{self, RequestNameFlags};
use zbus::interface;
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName, UniqueName};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::NoneValue;

use super::Start;
use crate::synoik::State;

/// The well-known name whose owner may use the interface (`meta-a11y-manager.c:410-411`).
const ALLOWED_SENDER: &str = "org.gnome.Orca.KeyboardMonitor";

#[derive(Debug, Default)]
struct Data {
    clients: HashMap<OwnedUniqueName, Client>,

    grabbed_mods: HashSet<Keysym>,
    /// mutter's `grabbed_keypresses`: keys whose press a client grabbed, so their release is too.
    suppressed_keys: HashSet<Keysym>,
    first_click: FirstClick,
}

/// `MetaSeatImplPrivate.a11y`: what `is_a11y_modifier_first_click` remembers between keys.
#[derive(Debug, Default)]
struct FirstClick {
    last_keysym: Option<Keysym>,
    last_time: Duration,
    saw_first_release: bool,
    /// Grabbed modifiers whose press was the second of a double press, processed normally.
    pressed_modifiers: HashSet<Keysym>,
}

#[derive(Debug, Default)]
struct Client {
    watched: bool,
    grabbed: bool,
    modifiers: HashSet<Keysym>,
    keystrokes: Vec<(Keysym, u32)>,
}

#[derive(Clone)]
pub struct KeyboardMonitor {
    data: Arc<Mutex<Data>>,
    iface: Arc<OnceLock<InterfaceRef<Self>>>,
}

/// Keyboard monitor key block reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KbMonBlock {
    /// Not blocked.
    Pass,
    /// Blocked, and this is the first press/release of the a11y modifier.
    ModifierFirstPress,
    /// Blocked, and this is not the a11y modifier.
    Block,
}

/// Interface for monitoring of keyboard input by assistive technologies.
///
/// This interface is used by assistive technologies to monitor keyboard input of the compositor.
/// The compositor is expected to listen on the well-known bus name "org.freedesktop.a11y.Manager"
/// at the object path "/org/freedesktop/a11y/Manager".
#[interface(name = "org.freedesktop.a11y.KeyboardMonitor")]
impl KeyboardMonitor {
    // Starts grabbing all key events. The client receives the events through the KeyEvent signal,
    // and in addition, the events aren't handled normally by the compositor. This includes changes
    // to the state of toggles like Caps Lock, Num Lock, and Scroll Lock.
    //
    // This behavior stays in effect until the same client calls UngrabKeyboard or closes its D-Bus
    // connection.
    async fn grab_keyboard(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<()> {
        super::check_sender(conn, hdr.sender(), &[ALLOWED_SENDER], "GrabKeyboard").await?;
        let Some(sender) = hdr.sender() else {
            return Err(fdo::Error::Failed("no sender".to_owned()));
        };
        let sender = OwnedUniqueName::from(sender.to_owned());
        trace!("enabling keyboard grab for {sender}");

        let mut data = self.data.lock().unwrap();
        let client = data.clients.entry(sender).or_default();
        client.grabbed = true;

        Ok(())
    }

    // Reverses the effect of calling GrabKeyboard. If GrabKeyboard wasn't previously called, this
    // method does nothing.
    //
    // After calling this method, the key grabs specified in the last call to SetKeyGrabs, if any,
    // are still in effect. Also, the client will still receive key events through the KeyEvent
    // signal, if it has called WatchKeyboard.
    async fn ungrab_keyboard(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<()> {
        super::check_sender(conn, hdr.sender(), &[ALLOWED_SENDER], "UngrabKeyboard").await?;
        let Some(sender) = hdr.sender() else {
            return Err(fdo::Error::Failed("no sender".to_owned()));
        };
        let sender = OwnedUniqueName::from(sender.to_owned());

        let mut data = self.data.lock().unwrap();
        if let Some(client) = data.clients.get_mut(&sender) {
            trace!("disabling keyboard grab for {sender}");
            client.grabbed = false;
        }

        Ok(())
    }

    // Starts watching all key events. The client receives the events through the KeyEvent signal,
    // but the events are still handled normally by the compositor. This includes changes to the
    // state of toggles like Caps Lock, Num Lock, and Scroll Lock.
    //
    // This behavior stays in effect until the same client calls UnwatchKeyboard or closes its D-Bus
    // connection.
    async fn watch_keyboard(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<()> {
        super::check_sender(conn, hdr.sender(), &[ALLOWED_SENDER], "WatchKeyboard").await?;
        let Some(sender) = hdr.sender() else {
            return Err(fdo::Error::Failed("no sender".to_owned()));
        };
        let sender = OwnedUniqueName::from(sender.to_owned());
        trace!("enabling keyboard watch for {sender}");

        let mut data = self.data.lock().unwrap();
        let client = data.clients.entry(sender).or_default();
        client.watched = true;

        Ok(())
    }

    // Reverses the effect of calling WatchKeyboard. If WatchKeyboard wasn't previously called, this
    // method does nothing.
    //
    // After calling this method, the key grabs specified in the last call to SetKeyGrabs, if any,
    // are still in effect, but other key events are no longer reported to this client.
    async fn unwatch_keyboard(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> fdo::Result<()> {
        super::check_sender(conn, hdr.sender(), &[ALLOWED_SENDER], "UnwatchKeyboard").await?;
        let Some(sender) = hdr.sender() else {
            return Err(fdo::Error::Failed("no sender".to_owned()));
        };
        let sender = OwnedUniqueName::from(sender.to_owned());

        let mut data = self.data.lock().unwrap();
        if let Some(client) = data.clients.get_mut(&sender) {
            trace!("disabling keyboard watch for {sender}");
            client.watched = false;
        }

        Ok(())
    }

    // Sets the current key grabs for the calling client, overriding any previous call to this
    // method. For grabbed key events, the KeyEvent signal is emitted, and normal key event handling
    // is suppressed, including state changes for toggles like Caps Lock and Num Lock.
    //
    // The grabs set by this method stay in effect until the same client calls this method again, or
    // until that client closes its D-Bus connection.
    //
    // Each item in `modifiers` is an XKB keysym. All keys in this list will be grabbed, and keys
    // pressed while any of these keys are down will also be grabbed.
    //
    // Each item in `keystrokes` is a struct with the following fields:
    //
    // - the XKB keysym of the non-modifier key
    // - the XKB modifier mask of the modifiers, if any, for this keystroke
    //
    // If any of the keys in `modifiers` is pressed alone, the compositor is required to ignore the
    // key press and release event if a second key press of the same modifier is not received within
    // a reasonable time frame, for example, the key repeat delay. If such event is received, this
    // second event is processed normally.
    async fn set_key_grabs(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        modifiers: Vec<u32>,
        keystrokes: Vec<(u32, u32)>,
    ) -> fdo::Result<()> {
        super::check_sender(conn, hdr.sender(), &[ALLOWED_SENDER], "SetKeyGrabs").await?;
        let Some(sender) = hdr.sender() else {
            return Err(fdo::Error::Failed("no sender".to_owned()));
        };
        let sender = OwnedUniqueName::from(sender.to_owned());
        trace!("updating key grabs for {sender}");

        let mut data = self.data.lock().unwrap();
        let client = data.clients.entry(sender).or_default();
        client.modifiers = HashSet::from_iter(modifiers.into_iter().map(Keysym::new));
        client.keystrokes =
            Vec::from_iter(keystrokes.into_iter().map(|(k, v)| (Keysym::new(k), v)));

        data.rebuild_grabbed_mods();

        Ok(())
    }

    // The compositor emits this signal for each key press or release.
    //
    // - `released`: whether this is a key-up event
    // - `state`: XKB modifier mask for currently pressed modifiers
    // - `keysym`: XKB keysym for this key
    // - `unichar`: Unicode character for this key, or 0 if none
    // - `keycode`: hardware-dependent keycode for this key
    #[zbus(signal)]
    pub async fn key_event(
        ctxt: &SignalEmitter<'_>,
        released: bool,
        state: u32,
        keysym: u32,
        unichar: u32,
        keycode: u16,
    ) -> zbus::Result<()>;
}

impl KeyboardMonitor {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            data: Arc::new(Mutex::new(Data::default())),
            iface: Arc::new(OnceLock::new()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process_key(
        &self,
        repeat_delay: Duration,
        time: Duration,
        keycode: Keycode,
        released: bool,
        mods: u32,
        keysym: Keysym,
        unichar: u32,
    ) -> KbMonBlock {
        let _span = tracy_client::span!("KeyboardMonitor::process_key");

        let (block, notify) =
            self.data
                .lock()
                .unwrap()
                .process(repeat_delay, time, released, mods, keysym);

        // No interface until the bus connection is up (and never in tests): nobody to tell.
        let Some(iface) = self.iface.get() else {
            return block;
        };
        for name in &notify {
            let _span = tracy_client::span!("emitting key event");

            // Emit to that client only.
            let ctxt = iface
                .signal_emitter()
                .clone()
                .set_destination(BusName::Unique(name.as_ref()));
            let ctxt = &ctxt;
            async_io::block_on(async move {
                if let Err(err) = KeyboardMonitor::key_event(
                    ctxt,
                    released,
                    mods,
                    keysym.raw(),
                    unichar,
                    keycode.raw() as u16,
                )
                .await
                {
                    warn!("error emitting key_event: {err:?}");
                }
            });
        }
        block
    }

    /// Set `client`'s requests as its D-Bus calls would, for tests that drive keys through the
    /// compositor without a bus.
    #[cfg(test)]
    pub fn set_client(&self, client: &str, grab_all: bool, modifiers: &[Keysym]) {
        let mut data = self.data.lock().unwrap();
        let name = OwnedUniqueName::try_from(client).unwrap();
        let entry = data.clients.entry(name).or_default();
        entry.grabbed = grab_all;
        entry.modifiers = modifiers.iter().copied().collect();
        data.rebuild_grabbed_mods();
    }
}

impl Data {
    /// `meta_a11y_manager_notify_clients` (`meta-a11y-manager.c:614-679`), with the first-click
    /// test mutter runs before it: whether the compositor handles the key, and which clients hear
    /// of it.
    fn process(
        &mut self,
        repeat_delay: Duration,
        time: Duration,
        released: bool,
        mods: u32,
        keysym: Keysym,
    ) -> (KbMonBlock, Vec<OwnedUniqueName>) {
        let first_click = self.is_modifier_first_click(keysym, time, released, repeat_delay);

        let notify = self
            .clients
            .iter()
            .filter(|(_, client)| client.should_watch_keypress(&self.suppressed_keys, mods, keysym))
            .map(|(name, _)| name.clone())
            .collect();

        // The second press of a double press, and its release, are the key's own.
        if self.grabbed_mods.contains(&keysym) && !first_click {
            return (KbMonBlock::Pass, notify);
        }

        let mut grabbed;
        if released {
            grabbed = self.suppressed_keys.remove(&keysym);
        } else {
            grabbed = self.suppressed_keys.contains(&keysym);
            if self
                .clients
                .values()
                .any(|client| client.should_grab_keypress(&self.suppressed_keys, mods, keysym))
            {
                self.suppressed_keys.insert(keysym);
                grabbed = true;
            }
        }

        let block = if first_click {
            // Never seen by the keymap: a lone Orca-modifier tap must not toggle Caps Lock.
            KbMonBlock::ModifierFirstPress
        } else if grabbed {
            KbMonBlock::Block
        } else {
            KbMonBlock::Pass
        };
        (block, notify)
    }

    /// `is_a11y_modifier_first_click` (`meta-seat-impl.c:520-568`): whether this event of a
    /// grabbed modifier is part of a lone tap, which the client takes, rather than the second press
    /// of a double press inside the repeat delay, which goes through.
    fn is_modifier_first_click(
        &mut self,
        keysym: Keysym,
        time: Duration,
        released: bool,
        repeat_delay: Duration,
    ) -> bool {
        let state = &mut self.first_click;
        let same_keysym = state.last_keysym == Some(keysym);
        let soon_enough = time.saturating_sub(state.last_time) < repeat_delay;
        state.last_keysym = Some(keysym);
        state.last_time = time;

        if !self.grabbed_mods.contains(&keysym) {
            return false;
        }
        if released && state.pressed_modifiers.remove(&keysym) {
            return false;
        }
        if same_keysym && soon_enough {
            if !released && state.saw_first_release {
                state.saw_first_release = false;
                state.pressed_modifiers.insert(keysym);
                return false;
            }
            state.saw_first_release = true;
            return true;
        }
        state.saw_first_release = false;
        true
    }

    fn rebuild_grabbed_mods(&mut self) {
        self.grabbed_mods.clear();
        for client in self.clients.values() {
            self.grabbed_mods.extend(&client.modifiers);
        }
    }
}

impl Client {
    fn should_grab_keypress(
        &self,
        suppressed_keys: &HashSet<Keysym>,
        mods: u32,
        keysym: Keysym,
    ) -> bool {
        // Grabbing all keys.
        if self.grabbed {
            return true;
        }

        for modifier in &self.modifiers {
            // This is a grabbed modifier, or a grabbed modifier is currently down.
            if *modifier == keysym || suppressed_keys.contains(modifier) {
                return true;
            }
        }

        for (grabbed_keysym, grabbed_mods) in &self.keystrokes {
            // This is a grabbed keystroke.
            if *grabbed_keysym == keysym && *grabbed_mods == mods {
                return true;
            }
        }

        false
    }

    fn should_watch_keypress(
        &self,
        suppressed_keys: &HashSet<Keysym>,
        mods: u32,
        keysym: Keysym,
    ) -> bool {
        if self.watched {
            return true;
        }

        self.should_grab_keypress(suppressed_keys, mods, keysym)
    }
}

async fn monitor_disappeared_clients(
    conn: &zbus::Connection,
    data: Arc<Mutex<Data>>,
) -> anyhow::Result<()> {
    let proxy = fdo::DBusProxy::new(conn)
        .await
        .context("error creating a DBusProxy")?;

    let mut stream = proxy
        .receive_name_owner_changed_with_args(&[(2, UniqueName::null_value())])
        .await
        .context("error creating a NameOwnerChanged stream")?;

    while let Some(signal) = stream.next().await {
        let args = signal
            .args()
            .context("error retrieving NameOwnerChanged args")?;

        let Some(name) = &**args.old_owner() else {
            continue;
        };

        if args.new_owner().is_none() {
            trace!("keyboard monitor client disconnected: {name}");

            let name = OwnedUniqueName::from(name.to_owned());
            let mut data = data.lock().unwrap();
            data.clients.remove(&name);
            data.rebuild_grabbed_mods();
        } else {
            error!("non-null new_owner should've been filtered out");
        }
    }

    Ok(())
}

impl Start for KeyboardMonitor {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let data = self.data.clone();

        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/freedesktop/a11y/Manager", self.clone())?;
        conn.request_name_with_flags("org.freedesktop.a11y.Manager", flags)?;

        let iface = conn
            .object_server()
            .interface("/org/freedesktop/a11y/Manager")?;
        let _ = self.iface.set(iface);

        let async_conn = conn.inner().clone();
        let future = async move {
            if let Err(err) = monitor_disappeared_clients(&async_conn, data.clone()).await {
                warn!("error monitoring keyboard monitor clients: {err:?}");

                // Since the monitor is now broken, prevent any further communication.
                if let Err(err) = async_conn.close().await {
                    warn!("error closing connection: {err:?}");
                }

                let mut data = data.lock().unwrap();
                data.clients.clear();
                data.rebuild_grabbed_mods();
            }
        };
        let task = conn
            .inner()
            .executor()
            .spawn(future, "monitor disappearing keyboard clients");
        task.detach();

        Ok(conn)
    }
}

impl State {
    pub fn a11y_process_key(
        &mut self,
        time: Duration,
        keycode: Keycode,
        state: KeyState,
    ) -> KbMonBlock {
        if self.synoik.a11y_keyboard_monitor.is_none() {
            return KbMonBlock::Pass;
        }

        let keyboard = self.synoik.seat.get_keyboard().unwrap();

        let (mods, keysym, unichar) = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            // SAFETY: we're not changing the ref count.
            let state = unsafe { xkb.state() };

            let keysym = state.key_get_one_sym(keycode);
            let mods = state.serialize_mods(xkb::STATE_MODS_EFFECTIVE);
            let unichar = state.key_get_utf32(keycode);

            (mods, keysym, unichar)
        });

        let config = self.synoik.config.borrow();
        let repeat_delay = Duration::from_millis(u64::from(config.input.keyboard.repeat_delay));
        let released = state == KeyState::Released;

        let Some(monitor) = &self.synoik.a11y_keyboard_monitor else {
            return KbMonBlock::Pass;
        };
        monitor.process_key(repeat_delay, time, keycode, released, mods, keysym, unichar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: Duration = Duration::from_millis(500);

    fn data(grab_all: bool, watch: bool, modifiers: &[Keysym]) -> Data {
        let mut data = Data::default();
        let client = data
            .clients
            .entry(OwnedUniqueName::try_from(":1.7").unwrap())
            .or_default();
        client.grabbed = grab_all;
        client.watched = watch;
        client.modifiers = modifiers.iter().copied().collect();
        data.rebuild_grabbed_mods();
        data
    }

    fn key(data: &mut Data, ms: u64, keysym: Keysym, released: bool) -> (KbMonBlock, usize) {
        let (block, notify) = data.process(DELAY, Duration::from_millis(ms), released, 0, keysym);
        (block, notify.len())
    }

    /// A grabbed modifier tapped alone never reaches the keymap; tapped twice inside the repeat
    /// delay, the second press and its release are the key's own; too slow, both are lone taps.
    /// The client hears every one of them.
    #[test]
    fn a_grabbed_modifier_is_the_clients_unless_double_pressed() {
        use KbMonBlock::*;
        let caps = Keysym::Caps_Lock;
        let mut d = data(false, false, &[caps]);
        assert_eq!(key(&mut d, 0, caps, false), (ModifierFirstPress, 1));
        assert_eq!(key(&mut d, 50, caps, true), (ModifierFirstPress, 1));
        assert_eq!(key(&mut d, 100, caps, false), (Pass, 1));
        assert_eq!(key(&mut d, 150, caps, true), (Pass, 1));

        assert_eq!(key(&mut d, 2000, caps, false), (ModifierFirstPress, 1));
        assert_eq!(key(&mut d, 2050, caps, true), (ModifierFirstPress, 1));
        assert_eq!(
            key(&mut d, 3000, caps, false),
            (ModifierFirstPress, 1),
            "too slow"
        );
        assert_eq!(key(&mut d, 3050, caps, true), (ModifierFirstPress, 1));

        assert_eq!(key(&mut d, 5000, caps, false), (ModifierFirstPress, 1));
        assert_eq!(
            key(&mut d, 5050, Keysym::a, false),
            (Block, 1),
            "a key under the modifier"
        );
        assert_eq!(key(&mut d, 5100, Keysym::a, true), (Block, 1));
        assert_eq!(key(&mut d, 5150, caps, true), (ModifierFirstPress, 1));
        assert_eq!(
            key(&mut d, 5200, Keysym::a, false),
            (Pass, 0),
            "and alone, not"
        );
    }

    /// A grab takes every key; a watch only listens.
    #[test]
    fn a_grab_takes_keys_and_a_watch_hears_them() {
        let mut grab = data(true, false, &[]);
        assert_eq!(key(&mut grab, 0, Keysym::a, false), (KbMonBlock::Block, 1));
        assert_eq!(key(&mut grab, 10, Keysym::a, true), (KbMonBlock::Block, 1));
        let mut watch = data(false, true, &[]);
        assert_eq!(key(&mut watch, 0, Keysym::a, false), (KbMonBlock::Pass, 1));
        assert_eq!(key(&mut watch, 10, Keysym::a, true), (KbMonBlock::Pass, 1));
    }
}
