//! Does IBus still compose when the field is declared a password?
//!
//! Run against your own daemon, never the session's (`SetGlobalEngine` is bus-wide):
//!
//! ```sh
//! ibus-daemon --panel disable --config disable --address unix:path=/tmp/ibusprobe.sock &
//! IBUS_ADDRESS=unix:path=/tmp/ibusprobe.sock cargo run --example ibus_password_probe
//! ```

use std::time::Duration;

use futures_util::{future, StreamExt};

fn main() -> anyhow::Result<()> {
    async_io::block_on(run())
}

async fn run() -> anyhow::Result<()> {
    use synoik::dbus::ibus;

    for (label, purpose, hints) in [
        ("free-form", ibus::purpose::FREE_FORM, 0),
        (
            "password",
            ibus::purpose::PASSWORD,
            ibus::hints::PRIVATE | ibus::hints::HIDDEN_TEXT,
        ),
        ("pin", ibus::purpose::PIN, 0),
    ] {
        // A fresh context each time: content type is per-context and we want no carry-over.
        let (_conn, bus, ctx) = ibus::connect().await?;
        bus.set_global_engine("xkb:us:intl:eng").await?;
        ctx.focus_in().await?;
        ctx.set_content_type((purpose, hints)).await?;

        let mut commits = ctx.receive_commit_text().await?;
        let dead = ctx.process_key_event(0xfe51, 40, 0).await?;
        let letter = ctx.process_key_event(0x61, 30, 0).await?;

        let timeout = std::pin::pin!(async_io::Timer::after(Duration::from_millis(1500)));
        let next = std::pin::pin!(commits.next());
        let committed = match future::select(next, timeout).await {
            future::Either::Left((Some(signal), _)) => {
                ibus::ibus_text(&signal.args()?.text).unwrap_or_default()
            }
            _ => "<nothing>".to_owned(),
        };

        println!(
            "{label:<10} dead_acute filtered={dead:<5} a filtered={letter:<5} commit={committed:?}"
        );
        ctx.focus_out().await?;
    }
    Ok(())
}
