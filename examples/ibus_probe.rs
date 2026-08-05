//! Live probe for [`synoik::dbus::ibus`]: connect to an ibus-daemon and round-trip a dead-key
//! sequence, printing what comes back. This is the only way to check the IBus half without a
//! full session, and it is what caught the `WAYLAND_DISPLAY`-before-`DISPLAY` ordering bug.
//!
//! **Run it against a daemon of your own, not the session's** — it calls `SetGlobalEngine`,
//! which changes the input source for everything else on that bus:
//!
//! ```sh
//! ibus-daemon --panel disable --config disable --address unix:path=/tmp/ibusprobe.sock &
//! IBUS_ADDRESS=unix:path=/tmp/ibusprobe.sock cargo run --example ibus_probe
//! ```
//!
//! Keep the socket path short: `sockaddr_un` truncates at 108 bytes, and a path under the usual
//! scratch directory silently lands the daemon on a *different* socket than the one you asked
//! for. A stock daemon also refuses per-context `SetEngine` (`use-global-engine` is on), which
//! is why the global call is the one used here.
//!
//! Expected output ends with `COMMIT: Some("á")`.

use std::time::Duration;

use futures_util::{future, StreamExt};

fn main() -> anyhow::Result<()> {
    async_io::block_on(run())
}

async fn run() -> anyhow::Result<()> {
    use synoik::dbus::ibus;

    let addr = ibus::discover_address().ok_or_else(|| anyhow::anyhow!("no address"))?;
    println!("address: {} (pid {:?})", addr.address, addr.pid);

    let (_conn, bus, ctx) = ibus::connect().await?;
    println!("input context created");

    let engine = ibus::engine_name("xkb", "us+intl", Some("eng"));
    println!("engine: {engine}");
    bus.set_global_engine(&engine).await?;
    ctx.focus_in().await?;

    let mut commits = ctx.receive_commit_text().await?;

    // `'` then `a` on us(intl) must come back as a single committed "á".
    // dead_acute = 0xfe51, a = 0x61. Keycodes are evdev: apostrophe = 40, a = 30.
    println!(
        "dead_acute handled = {}",
        ctx.process_key_event(0xfe51, 40, 0).await?
    );
    println!("a handled = {}", ctx.process_key_event(0x61, 30, 0).await?);

    let timeout = std::pin::pin!(async_io::Timer::after(Duration::from_secs(2)));
    let next = std::pin::pin!(commits.next());
    match future::select(next, timeout).await {
        future::Either::Left((Some(signal), _)) => {
            let args = signal.args()?;
            println!("COMMIT: {:?}", ibus::ibus_text(&args.text));
        }
        future::Either::Left((None, _)) => println!("signal stream ended"),
        future::Either::Right(_) => println!("no commit within 2s"),
    }

    ctx.focus_out().await?;
    Ok(())
}
