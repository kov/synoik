// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The daemon's configuration and the environment.d it hands sessions.

use std::collections::BTreeMap;
use std::path::Path;

pub const CONFIG_PATH: &str = "/etc/synoik/login.conf";

/// What `/etc/synoik/login.conf` says. Its keys are GDM's (`custom.conf`'s `[daemon]` group), so
/// an admin's habits carry over.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub autologin: Option<String>,
    /// `[debug] TestUsers`: who root may log in without credentials (`synoik-login
    /// open-session`), for a test seat beside the administrator's own. Ours, not GDM's.
    pub test_users: Vec<String>,
}

impl Config {
    pub fn load() -> Self {
        match std::fs::read_to_string(CONFIG_PATH) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    fn parse(text: &str) -> Self {
        let mut group = String::new();
        let mut enabled = false;
        let mut user = None;
        let mut test_users = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                group = name.trim().to_owned();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match (group.as_str(), key.trim()) {
                ("daemon", "AutomaticLoginEnable") => enabled = is_true(value.trim()),
                ("daemon", "AutomaticLogin") => {
                    user = Some(value.trim().to_owned()).filter(|u| !u.is_empty());
                }
                ("debug", "TestUsers") => {
                    test_users = value
                        .split([',', ' '])
                        .filter(|u| !u.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
                _ => (),
            }
        }
        Self {
            autologin: user.filter(|_| enabled),
            test_users,
        }
    }
}

fn is_true(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1")
}

/// The system environment.d, as `environment.d(5)` reads it for a user manager: every `*.conf`
/// in the system directories by file name, `/etc` hiding a same-named file in `/run` and `/run`
/// one in `/usr/lib`, each `KEY=VALUE` line expanding `$VAR`/`${VAR}` against what came before.
pub fn environment_d() -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    for dir in [
        "/usr/lib/environment.d",
        "/run/environment.d",
        "/etc/environment.d",
    ] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".conf") {
                files.insert(name, entry.path());
            }
        }
    }
    let mut env = BTreeMap::new();
    for path in files.values() {
        if let Ok(text) = std::fs::read_to_string(Path::new(path)) {
            parse_environment(&text, &mut env);
        }
    }
    env
}

fn parse_environment(text: &str, env: &mut BTreeMap<String, String>) {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = expand(value.trim(), env);
        env.insert(key.to_owned(), value);
    }
}

fn expand(value: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let name: String = if chars.peek() == Some(&'{') {
            chars.next();
            chars.by_ref().take_while(|&c| c != '}').collect()
        } else {
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if !(c.is_ascii_alphanumeric() || c == '_') {
                    break;
                }
                name.push(c);
                chars.next();
            }
            name
        };
        if name.is_empty() {
            out.push('$');
        } else if let Some(value) = env.get(&name) {
            out.push_str(value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autologin_needs_both_keys_in_daemon() {
        let on = "[daemon]\nAutomaticLoginEnable=True\nAutomaticLogin=claude\n";
        assert_eq!(Config::parse(on).autologin.as_deref(), Some("claude"));

        let off = "[daemon]\nAutomaticLoginEnable=false\nAutomaticLogin=claude\n";
        assert_eq!(Config::parse(off).autologin, None);

        let elsewhere = "[security]\nAutomaticLoginEnable=true\nAutomaticLogin=claude\n";
        assert_eq!(Config::parse(elsewhere).autologin, None);

        let unnamed = "[daemon]\nAutomaticLoginEnable=true\nAutomaticLogin=\n";
        assert_eq!(Config::parse(unnamed).autologin, None);
    }

    #[test]
    fn test_users_come_from_debug_only() {
        let listed = "[debug]\nTestUsers=gsrs, u2\n";
        assert_eq!(Config::parse(listed).test_users, ["gsrs", "u2"]);

        let elsewhere = "[daemon]\nTestUsers=gsrs\n";
        assert!(Config::parse(elsewhere).test_users.is_empty());
    }

    #[test]
    fn environment_expands_what_came_before() {
        let mut env = BTreeMap::new();
        parse_environment(
            "# comment\nA=one\nB=${A}:two\nC=$A/$MISSING/x\nbad key=1\nD=$\n",
            &mut env,
        );
        assert_eq!(env["A"], "one");
        assert_eq!(env["B"], "one:two");
        assert_eq!(env["C"], "one//x");
        assert_eq!(env["D"], "$");
        assert!(!env.contains_key("bad key"));
    }
}
