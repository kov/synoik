//! `org.gnome.Shell.Brightness` — the session object gsd-power drives the shell's brightness
//! through (`js/ui/shellDBus.js:595-637`, interface
//! `data/dbus-interfaces/org.gnome.Shell.Brightness.xml`).
//!
//! This is the one remaining *inbound* role gsd-power has in GNOME 50.1 brightness: the shell owns
//! the hardware ([`crate::backlight`]) and the scale algebra ([`crate::brightness`]), while
//! gsd-power only asks for idle dimming and — where there's an ambient light sensor — feeds an
//! auto-brightness target. Both land as calls on this object.
//!
//! The object lives on its **own** well-known name, not on `org.gnome.Shell`, so it gets its own
//! connection (see the well-known-name placement rule: an object must be exported on the
//! connection owning the name its callers use).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::interface;
use zbus::object_server::SignalEmitter;

use super::Start;

const BUS_NAME: &str = "org.gnome.Shell.Brightness";
const PATH: &str = "/org/gnome/Shell/Brightness";

/// A call from gsd-power into the compositor.
pub enum BrightnessToNiri {
    /// `SetDimming(b)`: enable or disable idle dimming — a clamp on top of the scales, not a move
    /// of them (`brightnessManager.js:242-259`).
    SetDimming(bool),
    /// `SetAutoBrightnessTarget(d)`: the ambient-light system's ideal relative brightness `[0,1]`,
    /// or a negative value to turn auto-brightness off. The scales then bias around it.
    SetAutoBrightnessTarget(f64),
}

/// A change the compositor wants reflected on the bus.
pub enum NiriToBrightness {
    /// Whether any display has brightness control — the `HasBrightnessControl` property, which
    /// gsd-power reads before bothering to dim (`shellDBus.js:614-624`).
    HasControl(bool),
    /// The user moved a brightness scale themselves: `BrightnessChanged`
    /// (`shellDBus.js:626-628`, off the manager's `user-update`). Notably NOT emitted for changes
    /// we made in response to gsd-power itself, or the ambient-light loop would chase its tail.
    UserChanged,
}

pub struct Brightness {
    to_niri: calloop::channel::Sender<BrightnessToNiri>,
    from_niri: async_channel::Receiver<NiriToBrightness>,
    has_control: Arc<AtomicBool>,
}

#[interface(name = "org.gnome.Shell.Brightness")]
impl Brightness {
    async fn set_dimming(&self, enable: bool) {
        let _ = self.to_niri.send(BrightnessToNiri::SetDimming(enable));
    }

    async fn set_auto_brightness_target(&self, target: f64) {
        let _ = self
            .to_niri
            .send(BrightnessToNiri::SetAutoBrightnessTarget(target));
    }

    #[zbus(property)]
    fn has_brightness_control(&self) -> bool {
        self.has_control.load(Ordering::SeqCst)
    }

    /// Emitted by the task in [`Start::start`], never from a method call.
    #[zbus(signal)]
    async fn brightness_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

impl Brightness {
    pub fn new(
        to_niri: calloop::channel::Sender<BrightnessToNiri>,
        from_niri: async_channel::Receiver<NiriToBrightness>,
    ) -> Self {
        Self {
            to_niri,
            from_niri,
            has_control: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Start for Brightness {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let from_niri = self.from_niri.clone();
        let has_control = self.has_control.clone();
        let conn = zbus::blocking::Connection::session()?;

        conn.object_server().at(PATH, self)?;

        // `DoNotQueue` matters as much as the replacement: without it, losing the request leaves us
        // *queued*, which zbus reports as `Ok(InQueue)` — we would then emit PropertiesChanged and
        // BrightnessChanged from a connection that does not own the name, while gsd-power's calls
        // went to whoever does. Treat anything but ownership as a failed start.
        let flags = RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue;
        match conn.request_name_with_flags(BUS_NAME, flags)? {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => (),
            reply => anyhow::bail!(
                "{BUS_NAME} is owned by another process (request replied {reply:?}); \
                 the brightness object is disabled"
            ),
        }

        // Emitter task: the compositor's side of the object. `HasBrightnessControl` is a property,
        // so its change goes out as PropertiesChanged; `BrightnessChanged` is a plain broadcast.
        let emit_conn = conn.inner().clone();
        conn.inner()
            .executor()
            .spawn(
                async move {
                    while let Ok(msg) = from_niri.recv().await {
                        let iface = match emit_conn
                            .object_server()
                            .interface::<_, Brightness>(PATH)
                            .await
                        {
                            Ok(iface) => iface,
                            Err(err) => {
                                warn!("brightness: error resolving our own interface: {err:?}");
                                continue;
                            }
                        };

                        match msg {
                            NiriToBrightness::HasControl(value) => {
                                if has_control.swap(value, Ordering::SeqCst) == value {
                                    continue;
                                }
                                if let Err(err) = iface
                                    .get()
                                    .await
                                    .has_brightness_control_changed(iface.signal_emitter())
                                    .await
                                {
                                    warn!(
                                        "brightness: error emitting HasBrightnessControl: {err:?}"
                                    );
                                }
                            }
                            NiriToBrightness::UserChanged => {
                                if let Err(err) =
                                    Brightness::brightness_changed(iface.signal_emitter()).await
                                {
                                    warn!("brightness: error emitting BrightnessChanged: {err:?}");
                                }
                            }
                        }
                    }
                },
                "org.gnome.Shell.Brightness emitter",
            )
            .detach();

        Ok(conn)
    }
}
