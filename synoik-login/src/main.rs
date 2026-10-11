// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `synoik-login`: the login daemon, and (`synoik-login session ...`) the worker it spawns for each
//! session. Root, and nothing else runs PAM (`docs/fork/greeter.md`).

mod bus;
mod channel;
mod config;
mod daemon;
mod ipc;
mod pam;
mod verify;
mod worker;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((mode, rest)) if mode == "session" => {
            worker::Request::parse(rest).and_then(|request| worker::run(&request))
        }
        Some((mode, [service, user])) if mode == "verify" => verify::run(service, user),
        None => daemon::run().map(|()| 0),
        Some(_) => Err(anyhow::anyhow!(
            "usage: synoik-login [session ... | verify SERVICE USER]"
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
