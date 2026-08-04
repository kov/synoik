// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::collections::HashMap;
use std::path::PathBuf;

use synoik_ipc::PickedColor;
use zbus::fdo::{self, RequestNameFlags};
use zbus::message::Header;
use zbus::zvariant::OwnedValue;
use zbus::{interface, zvariant};

use super::Start;

/// Who may take a picture of the screen (`screenshot.js:2489-2492`).
///
/// A *different* list from `org.gnome.Shell.Introspect`'s, and deliberately so: gsd-media-keys owns
/// the Print Screen key and has to be able to call this, while the GTK portal has no business
/// here. Without the check, any application on the session bus could silently capture the screen
/// to a path of its choosing.
const ALLOWLIST: [&str; 2] = [
    "org.gnome.SettingsDaemon.MediaKeys",
    "org.freedesktop.impl.portal.desktop.gnome",
];

pub struct Screenshot {
    to_niri: calloop::channel::Sender<ScreenshotToSynoik>,
    from_niri: async_channel::Receiver<SynoikToScreenshot>,
    /// Filled in by [`Start`]: the allowlist check has to ask the bus who owns a name.
    conn: Option<zbus::Connection>,
}

/// The rectangle `SelectArea` hands back: x, y, width, height in global logical coordinates.
pub type SelectedArea = (i32, i32, i32, i32);

/// Where a `SelectArea` result goes. `None` means the user dismissed the picker.
pub type SelectAreaReply = async_channel::Sender<Option<SelectedArea>>;

/// Where an `InteractiveScreenshot` result goes: the saved file's URI, or `None` if dismissed.
pub type InteractiveReply = async_channel::Sender<Option<String>>;

pub enum ScreenshotToSynoik {
    TakeScreenshot {
        include_cursor: bool,
        /// Where the caller wants the file. `None` means our own configured location.
        path: Option<PathBuf>,
    },
    /// The same capture cropped to a rectangle in global logical coordinates.
    TakeScreenshotArea {
        area: (i32, i32, i32, i32),
        path: Option<PathBuf>,
    },
    /// The focused window, which is what GNOME captures (`screenshot.js`'s `ScreenshotWindow`
    /// takes `global.display.focus_window`).
    TakeScreenshotWindow {
        include_cursor: bool,
        path: Option<PathBuf>,
    },
    /// A flash over a rectangle in global logical coordinates — visual only, no reply.
    FlashArea {
        area: (i32, i32, i32, i32),
    },
    /// Open the picker for `SelectArea`; the reply carries the chosen rectangle, or `None` if the
    /// user dismissed it.
    SelectArea(SelectAreaReply),
    /// Open the shell's screenshot UI; the reply carries the saved file's URI, or `None` if the
    /// user dismissed it.
    Interactive(InteractiveReply),
    PickColor(async_channel::Sender<Option<PickedColor>>),
}

pub enum SynoikToScreenshot {
    ScreenshotResult(Option<PathBuf>),
}

#[interface(name = "org.gnome.Shell.Screenshot")]
impl Screenshot {
    /// `filename` is honoured: the caller picked it, and a portal reads back exactly the path it
    /// asked for. An empty one means "wherever you normally put them", which is what a plain
    /// `synoik msg` caller wants.
    async fn screenshot(
        &self,
        include_cursor: bool,
        _flash: bool,
        filename: String,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<(bool, PathBuf)> {
        self.check_sender(&hdr, "Screenshot").await?;
        self.capture(ScreenshotToSynoik::TakeScreenshot {
            include_cursor,
            path: wanted_path(filename),
        })
        .await
    }

    /// `include_frame` is accepted and ignored: our windows have no server-side frame to include
    /// or omit, so both values mean the same capture.
    async fn screenshot_window(
        &self,
        _include_frame: bool,
        include_cursor: bool,
        _flash: bool,
        filename: String,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<(bool, PathBuf)> {
        self.check_sender(&hdr, "ScreenshotWindow").await?;
        self.capture(ScreenshotToSynoik::TakeScreenshotWindow {
            include_cursor,
            path: wanted_path(filename),
        })
        .await
    }

    /// Fire-and-forget: GNOME's `FlashArea` returns as soon as the effect is started
    /// (`screenshot.js`), and the caller does not wait for it to finish.
    async fn flash_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<()> {
        self.check_sender(&hdr, "FlashArea").await?;
        if width <= 0 || height <= 0 {
            return Err(fdo::Error::InvalidArgs("empty area".to_owned()));
        }
        if let Err(err) = self.to_niri.send(ScreenshotToSynoik::FlashArea {
            area: (x, y, width, height),
        }) {
            warn!("error sending message to synoik: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }
        Ok(())
    }

    /// The shell's own screenshot UI, driven over the bus — **the method the portal actually
    /// calls first** (`InteractiveScreenshotAsync`, `screenshot.js:2729-2755`).
    ///
    /// A dismissal is `(false, "")`, not an error: GNOME returns that on the UI's `closed` signal
    /// (`:2742-2745`), and the portal reads the boolean rather than catching a fault. This is the
    /// opposite convention from `SelectArea` next door, which does error — matched to each, not
    /// unified, because the callers differ.
    async fn interactive_screenshot(
        &self,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<(bool, String)> {
        self.check_sender(&hdr, "InteractiveScreenshot").await?;

        let (tx, rx) = async_channel::bounded(1);
        if let Err(err) = self.to_niri.send(ScreenshotToSynoik::Interactive(tx)) {
            warn!("error sending message to synoik: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        match rx.recv().await {
            Ok(Some(uri)) => Ok((true, uri)),
            Ok(None) => Ok((false, String::new())),
            Err(err) => {
                warn!("error receiving message from synoik: {err:?}");
                Err(fdo::Error::Failed("internal error".to_owned()))
            }
        }
    }

    /// Interactive: puts the picker up and blocks until the user picks or dismisses.
    ///
    /// A dismissal is `Cancelled`, not a zero-sized rectangle — the caller has to be able to tell
    /// "the user said no" from "the user selected nothing", and the portal treats the two
    /// differently.
    async fn select_area(
        &self,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<(i32, i32, i32, i32)> {
        self.check_sender(&hdr, "SelectArea").await?;
        let (tx, rx) = async_channel::bounded(1);
        if let Err(err) = self.to_niri.send(ScreenshotToSynoik::SelectArea(tx)) {
            warn!("error sending message to synoik: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        match rx.recv().await {
            Ok(Some(area)) => Ok(area),
            Ok(None) => Err(fdo::Error::Failed("cancelled".to_owned())),
            Err(err) => {
                warn!("error receiving message from synoik: {err:?}");
                Err(fdo::Error::Failed("internal error".to_owned()))
            }
        }
    }

    // GNOME's signature, argument for argument — the caller is the portal, so it is not ours to
    // condense.
    #[allow(clippy::too_many_arguments)]
    async fn screenshot_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        _flash: bool,
        filename: String,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<(bool, PathBuf)> {
        self.check_sender(&hdr, "ScreenshotArea").await?;
        if width <= 0 || height <= 0 {
            return Err(fdo::Error::InvalidArgs("empty area".to_owned()));
        }
        self.capture(ScreenshotToSynoik::TakeScreenshotArea {
            area: (x, y, width, height),
            path: wanted_path(filename),
        })
        .await
    }

    async fn pick_color(
        &self,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        self.check_sender(&hdr, "PickColor").await?;
        let (tx, rx) = async_channel::bounded(1);
        if let Err(err) = self.to_niri.send(ScreenshotToSynoik::PickColor(tx)) {
            warn!("error sending pick color message to synoik: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        let color = match rx.recv().await {
            Ok(Some(color)) => color,
            Ok(None) => {
                return Err(fdo::Error::Failed("no color picked".to_owned()));
            }
            Err(err) => {
                warn!("error receiving message from synoik: {err:?}");
                return Err(fdo::Error::Failed("internal error".to_owned()));
            }
        };

        let mut result = HashMap::new();
        let [r, g, b] = color.rgb;
        result.insert(
            "color".to_string(),
            zvariant::OwnedValue::try_from(zvariant::Value::from((r, g, b))).unwrap(),
        );

        Ok(result)
    }
}

/// GNOME lets `filename` be an absolute path or a bare basename, and treats an empty one as "pick
/// the usual place" (`org.gnome.Shell.Screenshot.xml`, `Screenshot`). Only the absolute case can be
/// honoured verbatim; anything else falls back to our configured screenshot path.
fn wanted_path(filename: String) -> Option<PathBuf> {
    let path = PathBuf::from(filename);
    path.is_absolute().then_some(path)
}

impl Screenshot {
    async fn capture(&self, msg: ScreenshotToSynoik) -> fdo::Result<(bool, PathBuf)> {
        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending message to synoik: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        match self.from_niri.recv().await {
            Ok(SynoikToScreenshot::ScreenshotResult(Some(filename))) => Ok((true, filename)),
            Ok(SynoikToScreenshot::ScreenshotResult(None)) => {
                Err(fdo::Error::Failed("internal error".to_owned()))
            }
            Err(err) => {
                warn!("error receiving message from synoik: {err:?}");
                Err(fdo::Error::Failed("internal error".to_owned()))
            }
        }
    }

    pub fn new(
        to_niri: calloop::channel::Sender<ScreenshotToSynoik>,
        from_niri: async_channel::Receiver<SynoikToScreenshot>,
    ) -> Self {
        Self {
            to_niri,
            from_niri,
            conn: None,
        }
    }

    async fn check_sender(&self, hdr: &Header<'_>, method: &str) -> fdo::Result<()> {
        let Some(conn) = self.conn.as_ref() else {
            return Err(fdo::Error::Failed("internal error".to_owned()));
        };
        super::check_sender(conn, hdr.sender(), &ALLOWLIST, method).await
    }
}

impl Start for Screenshot {
    fn start(mut self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        self.conn = Some(conn.inner().clone());
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Shell/Screenshot", self)?;
        conn.request_name_with_flags("org.gnome.Shell.Screenshot", flags)?;

        Ok(conn)
    }
}
