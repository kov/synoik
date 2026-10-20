// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

fn main() {
    println!("cargo:rustc-check-cfg=cfg(have_libinput_plugin_system)");
    if pkg_config::Config::new()
        .atleast_version("1.30.0")
        .probe("libinput")
        .is_ok()
    {
        println!("cargo:rustc-cfg=have_libinput_plugin_system")
    }

    bundle_cursor_theme();
}

/// Embed `resources/cursor-theme` as a table of `(path, bytes)`, the way gnome-shell compiles its
/// copy into a GResource: the fallback theme is always there, whatever the system installed.
/// Symlinked cursors (`move` → `default`) are followed, so each name is a plain entry.
fn bundle_cursor_theme() {
    use std::path::Path;

    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                let abs = path.canonicalize().unwrap().to_str().unwrap().to_owned();
                out.push((rel, abs));
            }
        }
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/cursor-theme");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    walk(&root, &root, &mut files);

    let mut code = String::from("&[\n");
    for (rel, abs) in files {
        code.push_str(&format!("    ({rel:?}, include_bytes!({abs:?})),\n"));
    }
    code.push_str("]\n");
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("cursor_theme.rs");
    std::fs::write(out, code).unwrap();
}
