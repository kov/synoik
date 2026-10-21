// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The password dialog's keyboard (`docs/fork/boot-splash.md` §4.2): every evdev keyboard, read
//! through one xkb state, as plymouthd reads them (`ply-input-device.c`, `ply-keyboard.c`).
//!
//! Armed only while a prompt is up; unarmed, keystrokes only move the xkb state, but for the
//! details view's toggle (§8.2), which is always read. Nothing is
//! grabbed — at boot no session holds the keyboard — but the foreground VT's keyboard is turned
//! off while armed (`KDSKBMODE K_OFF`, as plymouthd's `ply_terminal_set_disabled_input`), so what
//! is typed never reaches its line discipline: not its input queue, not its echo.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::{fs, io};

use xkbcommon::xkb;

const INPUT_DIR: &str = "/dev/input";

/// Where the image keeps the keymap compiled when it was built (`90synoik`): the initramfs has no
/// xkb data to compile one from.
pub const COMPILED_KEYMAP: &str = "/usr/share/synoik-splash/keymap.xkb";

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_LED: u16 = 0x11;
const SYN_REPORT: u16 = 0;
const LED_NUML: u16 = 0;
const LED_CAPSL: u16 = 1;
const LED_SCROLLL: u16 = 2;

/// What a keystroke does to the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Text(String),
    Backspace,
    /// Ctrl+U, Ctrl+W; and Ctrl+C, Ctrl+D, which cannot cancel (§3.2).
    EraseLine,
    Enter,
    /// Esc, plymouth's key, or KEY_INFO, which a host injects: the details view on or off (§8.2).
    /// Pressed, not repeated.
    Details,
}

/// `KEY_INFO`, matched on its code: no keymap needs to name it.
pub const KEY_INFO: u16 = 0x166;

/// The password being typed.
#[derive(Default)]
pub struct Entry {
    text: String,
}

impl Entry {
    /// One per character, as `ply_utf8_string_get_length` counts them.
    pub fn bullets(&self) -> usize {
        self.text.chars().count()
    }

    /// Type `key`; Enter returns the password.
    pub fn apply(&mut self, key: Key) -> Option<String> {
        match key {
            Key::Text(text) => self.text.push_str(&text),
            Key::Backspace => {
                self.text.pop();
            }
            Key::EraseLine => self.clear(),
            Key::Enter => return Some(std::mem::take(&mut self.text)),
            Key::Details => (),
        }
        None
    }

    /// Forget what was typed, overwriting it first.
    pub fn clear(&mut self) {
        let mut bytes = std::mem::take(&mut self.text).into_bytes();
        bytes.fill(0);
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.clear();
    }
}

/// The system keyboard layout, as localed keeps it: `XKB*` in `/etc/vconsole.conf`, else the
/// `Xkb*` options of `/etc/X11/xorg.conf.d/00-keyboard.conf`, else xkbcommon's defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    pub layout: String,
    pub model: String,
    pub variant: String,
    pub options: Option<String>,
}

impl Names {
    pub fn system() -> Self {
        let vconsole = fs::read_to_string("/etc/vconsole.conf").unwrap_or_default();
        let names = Self::from_vconsole(&vconsole);
        if !names.layout.is_empty() {
            return names;
        }
        let x11 = fs::read_to_string("/etc/X11/xorg.conf.d/00-keyboard.conf").unwrap_or_default();
        Self::from_x11(&x11)
    }

    fn from_vconsole(text: &str) -> Self {
        let mut names = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_owned();
            match key.trim() {
                "XKBLAYOUT" => names.layout = value,
                "XKBMODEL" => names.model = value,
                "XKBVARIANT" => names.variant = value,
                "XKBOPTIONS" => names.options = Some(value).filter(|v| !v.is_empty()),
                _ => (),
            }
        }
        names
    }

    fn from_x11(text: &str) -> Self {
        let mut names = Self::default();
        for line in text.lines() {
            let mut words = line.split('"').map(str::trim).filter(|w| !w.is_empty());
            if words.next() != Some("Option") {
                continue;
            }
            let (Some(key), Some(value)) = (words.next(), words.next()) else {
                continue;
            };
            let value = value.to_owned();
            match key {
                "XkbLayout" => names.layout = value,
                "XkbModel" => names.model = value,
                "XkbVariant" => names.variant = value,
                "XkbOptions" => names.options = Some(value).filter(|v| !v.is_empty()),
                _ => (),
            }
        }
        names
    }

    pub fn compile(&self, context: &xkb::Context) -> Option<xkb::Keymap> {
        xkb::Keymap::new_from_names(
            context,
            "",
            &self.model,
            &self.layout,
            &self.variant,
            self.options.clone(),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
    }
}

/// The keymap: the image's compiled one where there is one, else the system's.
pub fn system_keymap(context: &xkb::Context) -> Option<xkb::Keymap> {
    if let Ok(text) = fs::read_to_string(COMPILED_KEYMAP) {
        let keymap = xkb::Keymap::new_from_string(
            context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        );
        if keymap.is_some() {
            return keymap;
        }
        warn!("splash: {COMPILED_KEYMAP} does not compile");
    }
    Names::system().compile(context)
}

/// [`Keyboard::open`] with the system keymap ([`system_keymap`]).
pub fn open_system() -> io::Result<Keyboard> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = system_keymap(&context).ok_or_else(|| io::Error::other("no keymap"))?;
    Keyboard::open(keymap)
}

/// The system keymap as text, for the image to carry (`synoik-splash --compile-keymap`);
/// xkbcommon's default layout if the system's does not compile, since an image with no keymap could
/// never take a password.
pub fn compile_system_keymap() -> Option<String> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = Names::system().compile(&context).or_else(|| {
        eprintln!("synoik-splash: the system keymap does not compile; using the default");
        Names::default().compile(&context)
    })?;
    Some(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

/// The keyboard layout's name ("English (US)"): the last layout's, as plymouthd shows it.
pub fn layout_name(keymap: &xkb::Keymap) -> Option<String> {
    let layouts = keymap.num_layouts();
    let name = keymap.layout_get_name(layouts.checked_sub(1)?);
    Some(name.to_owned()).filter(|n| !n.is_empty())
}

/// Turn one key event into what it does to the entry, updating `state`. `value` is evdev's:
/// 1 down, 0 up, 2 a repeat, which types again without changing the state.
fn key_event(state: &mut xkb::State, code: u16, value: i32) -> (Option<Key>, bool) {
    let keycode = xkb::Keycode::new(u32::from(code) + 8);
    let sym = state.key_get_one_sym(keycode).raw();
    let mut leds = false;
    match value {
        0 => {
            leds = state.update_key(keycode, xkb::KeyDirection::Up) & xkb::STATE_LEDS != 0;
            return (None, leds);
        }
        1 => leds = state.update_key(keycode, xkb::KeyDirection::Down) & xkb::STATE_LEDS != 0,
        2 => (),
        _ => return (None, false),
    }
    if code == KEY_INFO || sym == xkb::keysyms::KEY_Escape {
        return ((value == 1).then_some(Key::Details), leds);
    }
    let key = match sym {
        xkb::keysyms::KEY_Return | xkb::keysyms::KEY_KP_Enter => Some(Key::Enter),
        xkb::keysyms::KEY_BackSpace => Some(Key::Backspace),
        _ => {
            let text = state.key_get_utf8(keycode);
            match text.as_str() {
                "" => None,
                "\u{15}" | "\u{17}" | "\u{3}" | "\u{4}" => Some(Key::EraseLine),
                t if t.chars().any(char::is_control) => None,
                _ => Some(Key::Text(text)),
            }
        }
    };
    // The modifier just pressed has no text, but may have changed the caps-lock indicator.
    (key, leds || sym == xkb::keysyms::KEY_Caps_Lock)
}

struct Device {
    fd: OwnedFd,
    leds: bool,
    path: PathBuf,
    /// Whether a key came from it yet: logged once, never which.
    heard: bool,
}

/// Every keyboard.
pub struct Keyboard {
    epoll: OwnedFd,
    inotify: OwnedFd,
    devices: HashMap<RawFd, Device>,
    keymap: xkb::Keymap,
    state: xkb::State,
    vt: Vt,
    armed: bool,
}

impl Keyboard {
    pub fn open(keymap: xkb::Keymap) -> io::Result<Self> {
        // SAFETY: plain syscalls; each fd is owned at once.
        let epoll = unsafe { cvt(libc::epoll_create1(libc::EPOLL_CLOEXEC)) }?;
        let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };
        let inotify = unsafe { cvt(libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC)) }?;
        let inotify = unsafe { OwnedFd::from_raw_fd(inotify) };
        let dir = CString::new(INPUT_DIR).expect("no NUL");
        // `IN_ATTRIB` too: udev creates the node first and opens its permissions up after.
        let mask = libc::IN_CREATE | libc::IN_ATTRIB;
        // SAFETY: a valid inotify fd and path.
        if unsafe { libc::inotify_add_watch(inotify.as_raw_fd(), dir.as_ptr(), mask) } < 0 {
            warn!(
                "splash: watching {INPUT_DIR}: {}",
                io::Error::last_os_error()
            );
        }
        watch(&epoll, inotify.as_raw_fd())?;

        let state = xkb::State::new(&keymap);
        let mut keyboard = Self {
            epoll,
            inotify,
            devices: HashMap::new(),
            keymap,
            state,
            vt: Vt::open(),
            armed: false,
        };
        if let Ok(entries) = fs::read_dir(INPUT_DIR) {
            for entry in entries.flatten() {
                keyboard.add(&entry.path());
            }
        }
        info!("{} keyboards open", keyboard.devices.len());
        Ok(keyboard)
    }

    /// Start typing a password: what was typed before is dropped, and the VT's keyboard is off
    /// until [`Self::disarm`].
    pub fn arm(&mut self) {
        if self.armed {
            return;
        }
        self.dispatch();
        self.vt.disable();
        self.armed = true;
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn disarm(&mut self) {
        if self.armed {
            self.vt.restore();
            self.armed = false;
        }
    }

    /// Readable when [`Self::dispatch`] has something to read.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.epoll.as_fd()
    }

    pub fn caps_lock(&self) -> bool {
        self.state.led_name_is_active(xkb::LED_NAME_CAPS)
    }

    pub fn layout_name(&self) -> Option<String> {
        layout_name(&self.keymap)
    }

    /// Read what is waiting: new keyboards and keystrokes. Returns the keys typed, and whether
    /// the caps-lock indicator may have changed.
    pub fn dispatch(&mut self) -> (Vec<Key>, bool) {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        // SAFETY: a buffer of ours, its length passed.
        let n = unsafe {
            libc::epoll_wait(
                self.epoll.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                0,
            )
        };
        let mut keys = Vec::new();
        let mut leds = false;
        for event in &events[..usize::try_from(n).unwrap_or(0)] {
            let fd = event.u64 as RawFd;
            if fd == self.inotify.as_raw_fd() {
                for name in read_inotify_names(self.inotify.as_fd()) {
                    self.add(&Path::new(INPUT_DIR).join(name));
                }
            } else {
                leds |= self.read(fd, &mut keys);
            }
        }
        if leds {
            self.write_leds();
        }
        if !self.armed {
            keys.retain(|k| *k == Key::Details);
        }
        (keys, leds)
    }

    fn add(&mut self, path: &Path) {
        let is_event = path
            .file_name()
            .is_some_and(|name| name.as_bytes().starts_with(b"event"));
        if !is_event || self.devices.values().any(|d| d.path == path) {
            return;
        }
        let Ok(cpath) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: a valid path.
        let raw = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if raw < 0 {
            // Not ours yet (udev has not opened it up), or already gone; `IN_ATTRIB` retries.
            info!("{}: {}", path.display(), io::Error::last_os_error());
            return;
        }
        // SAFETY: just opened.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let types = event_types(fd.as_fd());
        if types & (1 << EV_KEY) == 0 {
            return;
        }
        if let Err(err) = watch(&self.epoll, raw) {
            warn!("splash: {}: {err}", path.display());
            return;
        }
        info!("reading {} for a password", path.display());
        self.devices.insert(
            raw,
            Device {
                fd,
                leds: types & (1 << EV_LED) != 0,
                path: path.to_owned(),
                heard: false,
            },
        );
    }

    /// Drain device `fd`. Returns whether the LEDs need an update.
    fn read(&mut self, fd: RawFd, keys: &mut Vec<Key>) -> bool {
        let mut events = [libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: 0,
            code: 0,
            value: 0,
        }; 64];
        let size = std::mem::size_of::<libc::input_event>();
        let mut leds = false;
        loop {
            // SAFETY: a buffer of ours, its length passed.
            let n = unsafe { libc::read(fd, events.as_mut_ptr().cast(), size * events.len()) };
            if n <= 0 {
                let gone =
                    n == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN);
                if gone {
                    if let Some(device) = self.devices.remove(&fd) {
                        debug!("{} went away", device.path.display());
                    }
                }
                return leds;
            }
            for event in &events[..n as usize / size] {
                if event.type_ != EV_KEY {
                    continue;
                }
                if let Some(device) = self.devices.get_mut(&fd).filter(|d| !d.heard) {
                    device.heard = true;
                    info!("keys from {}", device.path.display());
                }
                let (key, changed) = key_event(&mut self.state, event.code, event.value);
                leds |= changed;
                keys.extend(key);
            }
        }
    }

    /// Light every keyboard's LEDs as the xkb state says (`ply_input_device_set_state`).
    fn write_leds(&self) {
        let led = |code, name| libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: EV_LED,
            code,
            value: i32::from(self.state.led_name_is_active(name)),
        };
        let mut events = [
            led(LED_NUML, xkb::LED_NAME_NUM),
            led(LED_CAPSL, xkb::LED_NAME_CAPS),
            led(LED_SCROLLL, xkb::LED_NAME_SCROLL),
            led(0, ""),
        ];
        events[3].type_ = EV_SYN;
        events[3].code = SYN_REPORT;
        events[3].value = 0;
        for device in self.devices.values().filter(|d| d.leds) {
            // SAFETY: a buffer of ours, its length passed. A failed write leaves the LEDs stale.
            unsafe {
                libc::write(
                    device.fd.as_raw_fd(),
                    events.as_ptr().cast(),
                    std::mem::size_of_val(&events),
                );
            }
        }
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        self.disarm();
    }
}

/// The foreground VT, its keyboard off while armed. Opened with the keyboards, so a splash that
/// outlives the initramfs keeps a descriptor its label no longer needs to open (§7.1).
struct Vt {
    fd: Option<OwnedFd>,
    /// The mode to restore, while the keyboard is off.
    saved: Option<libc::c_int>,
}

const KDGKBMODE: libc::c_ulong = 0x4B44;
const KDSKBMODE: libc::c_ulong = 0x4B45;
const K_OFF: libc::c_int = 0x04;

impl Vt {
    fn open() -> Self {
        // SAFETY: a static path.
        let raw = unsafe {
            libc::open(
                c"/dev/tty0".as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if raw < 0 {
            // The outer, unprivileged: tty0 is root's (docs/fork/boot-splash.md §6).
            warn!(
                "splash: /dev/tty0: {}; typing reaches the VT too",
                io::Error::last_os_error()
            );
        }
        Self {
            // SAFETY: just opened.
            fd: (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw) }),
            saved: None,
        }
    }

    /// Off, unless there is no VT or it is off already (a session's, or one a crashed splash left
    /// off: restoring that would turn it back off).
    fn disable(&mut self) {
        let Some(fd) = &self.fd else { return };
        let raw = fd.as_raw_fd();
        let mut mode: libc::c_int = 0;
        // SAFETY: the ioctl's own argument.
        if unsafe { libc::ioctl(raw, KDGKBMODE, &raw mut mode) } < 0 || mode == K_OFF {
            return;
        }
        // SAFETY: the ioctl takes its argument by value.
        if unsafe { libc::ioctl(raw, KDSKBMODE, K_OFF) } < 0 {
            warn!(
                "splash: turning the VT keyboard off: {}",
                io::Error::last_os_error()
            );
            return;
        }
        self.saved = Some(mode);
    }

    fn restore(&mut self) {
        let (Some(fd), Some(mode)) = (&self.fd, self.saved.take()) else {
            return;
        };
        // SAFETY: our fd, the ioctl's own argument, after a flush of anything queued before.
        unsafe {
            libc::tcflush(fd.as_raw_fd(), libc::TCIFLUSH);
            libc::ioctl(fd.as_raw_fd(), KDSKBMODE, mode);
        }
    }
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn watch(epoll: &OwnedFd, fd: RawFd) -> io::Result<()> {
    let mut event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: fd as u64,
    };
    // SAFETY: valid fds and an event of ours.
    cvt(unsafe { libc::epoll_ctl(epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &raw mut event) })
        .map(drop)
}

/// The event types a device reports (`EVIOCGBIT(0)`), as a bit mask.
fn event_types(fd: BorrowedFd<'_>) -> u64 {
    let mut bits: u64 = 0;
    // _IOC(_IOC_READ, 'E', 0x20, 8)
    const EVIOCGBIT0: libc::c_ulong = 0x8008_4520;
    // SAFETY: the ioctl writes at most the 8 bytes it is told of.
    if unsafe { libc::ioctl(fd.as_raw_fd(), EVIOCGBIT0, &raw mut bits) } < 0 {
        return 0;
    }
    bits
}

/// The names in the inotify events waiting on `fd`.
fn read_inotify_names(fd: BorrowedFd<'_>) -> Vec<std::ffi::OsString> {
    let mut names = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: a buffer of ours, its length passed.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            return names;
        }
        let mut at = 0;
        let header = std::mem::size_of::<libc::inotify_event>();
        while at + header <= n as usize {
            // SAFETY: the kernel writes whole events; the header fits per the check above.
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
            let name = &buf[at + header..at + header + event.len as usize];
            let name = name.split(|&b| b == 0).next().unwrap_or_default();
            if !name.is_empty() {
                names.push(std::ffi::OsStr::from_bytes(name).to_owned());
            }
            at += header + event.len as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: u16 = 30;
    const KEY_U: u16 = 22;
    const KEY_C: u16 = 46;
    const KEY_ENTER: u16 = 28;
    const KEY_BACKSPACE: u16 = 14;
    const KEY_ESC: u16 = 1;
    const KEY_LEFTSHIFT: u16 = 42;
    const KEY_LEFTCTRL: u16 = 29;
    const KEY_CAPSLOCK: u16 = 58;

    fn us() -> Option<xkb::State> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let names = Names {
            layout: "us".into(),
            ..Names::default()
        };
        let Some(keymap) = names.compile(&context) else {
            eprintln!("skipping: no xkb data");
            return None;
        };
        Some(xkb::State::new(&keymap))
    }

    fn tap(state: &mut xkb::State, code: u16) -> Option<Key> {
        let (key, _) = key_event(state, code, 1);
        key_event(state, code, 0);
        key
    }

    #[test]
    fn keys_type_into_the_entry() {
        let Some(mut state) = us() else { return };
        let mut entry = Entry::default();
        let mut feed = |key: Option<Key>| key.and_then(|k| entry.apply(k));

        assert_eq!(feed(tap(&mut state, KEY_A)), None);
        key_event(&mut state, KEY_LEFTSHIFT, 1);
        assert_eq!(feed(tap(&mut state, KEY_A)), None);
        key_event(&mut state, KEY_LEFTSHIFT, 0);
        // A held key repeats.
        assert_eq!(
            key_event(&mut state, KEY_A, 1).0,
            Some(Key::Text("a".into()))
        );
        assert_eq!(
            key_event(&mut state, KEY_A, 2).0,
            Some(Key::Text("a".into()))
        );
        key_event(&mut state, KEY_A, 0);
        assert_eq!(tap(&mut state, KEY_BACKSPACE), Some(Key::Backspace));
        // Esc and KEY_INFO toggle the details, once per press.
        assert_eq!(tap(&mut state, KEY_ESC), Some(Key::Details));
        assert_eq!(tap(&mut state, KEY_INFO), Some(Key::Details));
        assert_eq!(key_event(&mut state, KEY_INFO, 1).0, Some(Key::Details));
        assert_eq!(key_event(&mut state, KEY_INFO, 2).0, None);
        key_event(&mut state, KEY_INFO, 0);
        assert_eq!(tap(&mut state, KEY_ENTER), Some(Key::Enter));

        let mut entry = Entry::default();
        for key in [
            Key::Text("a".into()),
            Key::Text("A".into()),
            Key::Text("é".into()),
        ] {
            entry.apply(key);
        }
        assert_eq!(entry.bullets(), 3);
        entry.apply(Key::Backspace);
        assert_eq!(entry.bullets(), 2);
        assert_eq!(entry.apply(Key::Enter).as_deref(), Some("aA"));
        assert_eq!(entry.bullets(), 0);
    }

    #[test]
    fn control_keys_erase() {
        let Some(mut state) = us() else { return };
        key_event(&mut state, KEY_LEFTCTRL, 1);
        assert_eq!(tap(&mut state, KEY_U), Some(Key::EraseLine));
        // No cancelling: systemd's agent takes NO_ANSWER as no UI, and would never ask again.
        assert_eq!(tap(&mut state, KEY_C), Some(Key::EraseLine));
        key_event(&mut state, KEY_LEFTCTRL, 0);

        let mut entry = Entry::default();
        entry.apply(Key::Text("abc".into()));
        assert_eq!(entry.apply(Key::EraseLine), None);
        assert_eq!(entry.bullets(), 0);
    }

    #[test]
    fn caps_lock_lights_its_indicator() {
        let Some(mut state) = us() else { return };
        let (key, changed) = key_event(&mut state, KEY_CAPSLOCK, 1);
        assert_eq!(key, None);
        assert!(changed);
        key_event(&mut state, KEY_CAPSLOCK, 0);
        assert!(state.led_name_is_active(xkb::LED_NAME_CAPS));
        assert_eq!(tap(&mut state, KEY_A), Some(Key::Text("A".into())));
    }

    #[test]
    fn the_layout_comes_from_vconsole_then_x11() {
        let vconsole = "KEYMAP=\"br-abnt2\"\nFONT=eurlatgr\nXKBLAYOUT=\"br\"\nXKBVARIANT=abnt2\n";
        assert_eq!(
            Names::from_vconsole(vconsole),
            Names {
                layout: "br".into(),
                variant: "abnt2".into(),
                ..Names::default()
            }
        );
        // Fedora's: KEYMAP only, so the X11 file decides.
        assert_eq!(Names::from_vconsole("KEYMAP=\"us\"\n").layout, "");
        let x11 = "Section \"InputClass\"\n        Identifier \"system-keyboard\"\n        \
                   MatchIsKeyboard \"on\"\n        Option \"XkbLayout\" \"us,br\"\n        \
                   Option \"XkbOptions\" \"grp:alt_shift_toggle\"\nEndSection\n";
        assert_eq!(
            Names::from_x11(x11),
            Names {
                layout: "us,br".into(),
                options: Some("grp:alt_shift_toggle".into()),
                ..Names::default()
            }
        );
    }

    #[test]
    fn a_compiled_keymap_keeps_its_layout_name() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let names = Names {
            layout: "us".into(),
            ..Names::default()
        };
        let Some(keymap) = names.compile(&context) else {
            eprintln!("skipping: no xkb data");
            return;
        };
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let again = xkb::Keymap::new_from_string(
            &context,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        assert_eq!(layout_name(&again).as_deref(), Some("English (US)"));
    }
}
