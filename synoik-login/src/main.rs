// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `synoik-login`: the login daemon, and (`synoik-login session ...`) the worker it spawns for each
//! session. Root, and nothing else runs PAM (`docs/fork/greeter.md`). `synoik-login open-session
//! USER` asks the running daemon to log a test user in (`[debug] TestUsers`).

mod bus;
mod channel;
mod config;
mod daemon;
mod ipc;
mod pam;
mod verify;
mod vt;
mod worker;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((mode, rest)) if mode == "session" => {
            worker::Request::parse(rest).and_then(|request| worker::run(&request))
        }
        Some((mode, [service, user])) if mode == "verify" => verify::run(service, user),
        Some((mode, [user, flags @ ..]))
            if mode == "open-session" && flags.iter().all(|f| f == "--background") =>
        {
            open_session(user, !flags.is_empty())
        }
        Some((mode, [service, user, sep, program @ ..]))
            if mode == "login" && sep == "--" && !program.is_empty() =>
        {
            verify::run_login(service, user, program)
        }
        None => daemon::run().map(|()| 0),
        Some(_) => Err(anyhow::anyhow!(
            "usage: synoik-login [open-session USER [--background] | session ... | verify SERVICE USER \
             | login SERVICE USER -- PROGRAM...]"
        )),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("synoik-login: {err:#}");
            std::process::exit(1);
        }
    }
}

/// Ask the daemon to open `user`'s session, and print its logind id.
fn open_session(user: &str, background: bool) -> anyhow::Result<i32> {
    let connection = zbus::blocking::Connection::system()?;
    let reply = connection.call_method(
        Some("org.gnome.DisplayManager"),
        bus::LOGIN_PATH,
        Some(bus::LOGIN_INTERFACE),
        "OpenSession",
        &(user, background),
    )?;
    let id: String = reply.body().deserialize()?;
    println!("{id}");
    Ok(0)
}
