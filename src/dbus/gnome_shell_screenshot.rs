use std::collections::HashMap;
use std::path::PathBuf;

use niri_ipc::PickedColor;
use zbus::fdo::{self, RequestNameFlags};
use zbus::zvariant::OwnedValue;
use zbus::{interface, zvariant};

use super::Start;

pub struct Screenshot {
    to_niri: calloop::channel::Sender<ScreenshotToNiri>,
    from_niri: async_channel::Receiver<NiriToScreenshot>,
}

pub enum ScreenshotToNiri {
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
    PickColor(async_channel::Sender<Option<PickedColor>>),
}

pub enum NiriToScreenshot {
    ScreenshotResult(Option<PathBuf>),
}

#[interface(name = "org.gnome.Shell.Screenshot")]
impl Screenshot {
    /// `filename` is honoured: the caller picked it, and a portal reads back exactly the path it
    /// asked for. An empty one means "wherever you normally put them", which is what a plain
    /// `niri msg` caller wants.
    async fn screenshot(
        &self,
        include_cursor: bool,
        _flash: bool,
        filename: String,
    ) -> fdo::Result<(bool, PathBuf)> {
        self.capture(ScreenshotToNiri::TakeScreenshot {
            include_cursor,
            path: wanted_path(filename),
        })
        .await
    }

    async fn screenshot_area(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        _flash: bool,
        filename: String,
    ) -> fdo::Result<(bool, PathBuf)> {
        if width <= 0 || height <= 0 {
            return Err(fdo::Error::InvalidArgs("empty area".to_owned()));
        }
        self.capture(ScreenshotToNiri::TakeScreenshotArea {
            area: (x, y, width, height),
            path: wanted_path(filename),
        })
        .await
    }

    async fn pick_color(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        let (tx, rx) = async_channel::bounded(1);
        if let Err(err) = self.to_niri.send(ScreenshotToNiri::PickColor(tx)) {
            warn!("error sending pick color message to niri: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        let color = match rx.recv().await {
            Ok(Some(color)) => color,
            Ok(None) => {
                return Err(fdo::Error::Failed("no color picked".to_owned()));
            }
            Err(err) => {
                warn!("error receiving message from niri: {err:?}");
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
    async fn capture(&self, msg: ScreenshotToNiri) -> fdo::Result<(bool, PathBuf)> {
        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending message to niri: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        match self.from_niri.recv().await {
            Ok(NiriToScreenshot::ScreenshotResult(Some(filename))) => Ok((true, filename)),
            Ok(NiriToScreenshot::ScreenshotResult(None)) => {
                Err(fdo::Error::Failed("internal error".to_owned()))
            }
            Err(err) => {
                warn!("error receiving message from niri: {err:?}");
                Err(fdo::Error::Failed("internal error".to_owned()))
            }
        }
    }

    pub fn new(
        to_niri: calloop::channel::Sender<ScreenshotToNiri>,
        from_niri: async_channel::Receiver<NiriToScreenshot>,
    ) -> Self {
        Self { to_niri, from_niri }
    }
}

impl Start for Screenshot {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Shell/Screenshot", self)?;
        conn.request_name_with_flags("org.gnome.Shell.Screenshot", flags)?;

        Ok(conn)
    }
}
