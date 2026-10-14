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

/// The user manager's own `PATH` (systemd's default, what `systemctl --user show-environment`
/// reports on Fedora): environment.d starts from the manager's environment, so `PATH=$PATH:...`
/// appends to this.
const MANAGER_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin";

/// The system environment.d, as `environment.d(5)` reads it for a user manager: every `*.conf`
/// in the system directories by file name, `/etc` hiding a same-named file in `/run` and `/run`
/// one in `/usr/lib`, read in that order on top of the manager's own environment.
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
    environment_from(
        files
            .values()
            .filter_map(|path| std::fs::read_to_string(Path::new(path)).ok()),
    )
}

fn environment_from(texts: impl IntoIterator<Item = impl AsRef<str>>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::from([("PATH".to_owned(), MANAGER_PATH.to_owned())]);
    for text in texts {
        parse_environment(text.as_ref(), &mut env);
    }
    env
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// One file, in `environment.d(5)`'s syntax: `#`/`;` comments, quoting and backslash escapes
/// first, then `$` expansion against what came before.
fn parse_environment(text: &str, env: &mut BTreeMap<String, String>) {
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            break;
        };
        if first == '#' || first == ';' {
            chars.by_ref().find(|&c| c == '\n');
            continue;
        }
        let mut key = String::new();
        let mut assigned = false;
        for c in chars.by_ref() {
            match c {
                '=' => {
                    assigned = true;
                    break;
                }
                '\n' => break,
                c => key.push(c),
            }
        }
        if !assigned {
            continue;
        }
        let value = read_value(&mut chars);
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = expand(&value, env);
        env.insert(key.to_owned(), value);
    }
}

/// The right-hand side, up to the end of its (logical) line: leading and trailing whitespace
/// stripped, `\` taking the next character literally (a newline: a continuation), and any number
/// of quoted parts, `'...'` literal and `"..."` with `\` escaping only `"`, `\`, `$`, `` ` `` and a
/// newline.
fn read_value(chars: &mut Chars) -> String {
    let mut value = String::new();
    // `value` up to here is kept; whitespace past it is trailing, and stripped.
    let mut kept = 0;
    // Before a value part: whitespace is skipped, and a quote opens a quoted part.
    let mut before = true;
    while let Some(c) = chars.next() {
        if c == '\n' {
            break;
        }
        if before {
            match c {
                ' ' | '\t' => continue,
                '\'' | '"' => {
                    read_quoted(chars, &mut value, c);
                    kept = value.len();
                    continue;
                }
                _ => before = false,
            }
        }
        match c {
            '\\' => {
                if let Some(c) = chars.next().filter(|&c| c != '\n') {
                    value.push(c);
                    kept = value.len();
                }
            }
            c => {
                value.push(c);
                if !c.is_whitespace() {
                    kept = value.len();
                }
            }
        }
    }
    value.truncate(kept);
    value
}

fn read_quoted(chars: &mut Chars, value: &mut String, quote: char) {
    while let Some(c) = chars.next() {
        match c {
            c if c == quote => return,
            '\\' if quote == '"' => match chars.next() {
                Some('\n') => {}
                Some(c @ ('"' | '\\' | '$' | '`')) => value.push(c),
                Some(c) => {
                    value.push('\\');
                    value.push(c);
                }
                None => value.push('\\'),
            },
            c => value.push(c),
        }
    }
}

/// `$NAME`, `${NAME}`, `${NAME:-DEFAULT}` (`DEFAULT` when `NAME` would expand empty),
/// `${NAME:+ALTERNATE}` (`ALTERNATE` when it would not) and `$$` for a `$`; an unset name expands
/// empty. `DEFAULT` and `ALTERNATE` are expanded in turn.
fn expand(value: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        if chars.next_if_eq(&'$').is_some() {
            out.push('$');
        } else if chars.next_if_eq(&'{').is_some() {
            // To the matching brace, so a word can hold a `${...}` of its own.
            let mut body = String::new();
            let mut depth = 0;
            for c in chars.by_ref() {
                match c {
                    '}' if depth == 0 => break,
                    '}' => depth -= 1,
                    '{' => depth += 1,
                    _ => {}
                }
                body.push(c);
            }
            let (name, word) = match body.split_once(':') {
                Some((name, word)) if word.starts_with(['-', '+']) => (name, Some(word)),
                _ => (body.as_str(), None),
            };
            let current = env.get(name).map_or("", String::as_str);
            match word.map(|word| word.split_at(1)) {
                None => out.push_str(current),
                Some(("-", default)) if current.is_empty() => out.push_str(&expand(default, env)),
                Some(("-", _)) => out.push_str(current),
                Some((_, alternate)) if !current.is_empty() => {
                    out.push_str(&expand(alternate, env));
                }
                Some(_) => {}
            }
        } else {
            let mut name = String::new();
            while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '_') {
                name.push(c);
            }
            if name.is_empty() {
                out.push('$');
            } else if let Some(value) = env.get(&name) {
                out.push_str(value);
            }
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
            "# comment\nA=one\nB=${A}:two\nC=$A/$MISSING/x\nbad key=1\nD=$\nE=$$A\nno equals\n",
            &mut env,
        );
        assert_eq!(env["A"], "one");
        assert_eq!(env["B"], "one:two");
        assert_eq!(env["C"], "one//x");
        assert_eq!(env["D"], "$");
        assert_eq!(env["E"], "$A");
        assert!(!env.contains_key("bad key"));
        assert_eq!(env.len(), 5);
    }

    #[test]
    fn environment_starts_from_the_managers_path() {
        // Fedora's snapd file: both lines broke a session's PATH and XDG_DATA_DIRS.
        let env = environment_from([
            "PATH=$PATH:/var/lib/snapd/snap/bin\n\
             XDG_DATA_DIRS=${XDG_DATA_DIRS:-/usr/local/share/:/usr/share/}:/var/lib/snapd/desktop\n",
        ]);
        assert_eq!(
            env["PATH"],
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/var/lib/snapd/snap/bin"
        );
        assert_eq!(
            env["XDG_DATA_DIRS"],
            "/usr/local/share/:/usr/share/:/var/lib/snapd/desktop"
        );
    }

    #[test]
    fn environment_default_and_alternate() {
        // environment.d(5)'s example 1.
        let mut env = BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]);
        parse_environment(
            "PATH=/opt/foo/bin:$PATH\n\
             LD_LIBRARY_PATH=/opt/foo/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}\n\
             XDG_DATA_DIRS=/opt/foo/share:${XDG_DATA_DIRS:-/usr/local/share/:/usr/share/}\n\
             SET=x\n\
             KEPT=${SET:-unused}\n\
             ALT=${SET:+[${SET}]}\n",
            &mut env,
        );
        assert_eq!(env["PATH"], "/opt/foo/bin:/usr/bin");
        assert_eq!(env["LD_LIBRARY_PATH"], "/opt/foo/lib");
        assert_eq!(
            env["XDG_DATA_DIRS"],
            "/opt/foo/share:/usr/local/share/:/usr/share/"
        );
        assert_eq!(env["KEPT"], "x");
        assert_eq!(env["ALT"], "[x]");
    }

    #[test]
    fn environment_quoting_and_escapes() {
        // environment.d(5)'s example 2, with its stated results.
        let mut env = BTreeMap::new();
        parse_environment(
            "# A comment\n\
             test_var=(variable)\n\
             ; Another comment\n\
             test_comment=value # This is a part of the variable, and not a comment!\n\
             \n\
             test_none= \\ before\\\nafter \"\" '' \\${test_var}\n\
             test_double=\" \\ before\\\nafter \"\" '' \\${test_var}\"\n\
             test_single=' \\ before\\\nafter \"\" '' \\${test_var}'\n",
            &mut env,
        );
        assert_eq!(env["test_var"], "(variable)");
        assert_eq!(
            env["test_comment"],
            "value # This is a part of the variable, and not a comment!"
        );
        assert_eq!(env["test_none"], " beforeafter \"\" '' (variable)");
        assert_eq!(env["test_double"], " \\ beforeafter  '' (variable)");
        assert_eq!(env["test_single"], " \\ before\\\nafter \"\"  \\(variable)");
        assert_eq!(env.len(), 5);
    }
}
