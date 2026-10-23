// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Remote search providers — the apps that answer overview searches over
//! `org.gnome.Shell.SearchProvider2` (`js/ui/remoteSearch.js`).
//!
//! An app announces itself with a key file in `<datadir>/gnome-shell/search-providers/`; this
//! module reads those files, applies `org.gnome.desktop.search-providers` to them and defines the
//! two channels that connect the compositor to the provider client in
//! [`crate::dbus::search_provider`].
//!
//! **Process seam.** Result names, descriptions and icons are whatever the provider sends. The
//! client owns its bus connection and does all of the parsing and bounding; across the seam
//! travel only the plain, validated types here ([`SynoikToSearch`] in, [`SearchToSynoik`] out), so
//! the client can move into its own process without the compositor changing.

use std::path::{Path, PathBuf};

use gio::glib;

use crate::app_system::{AppEntry, AppIconRef};

/// `KEY_FILE_GROUP` (`remoteSearch.js:10`).
const KEY_FILE_GROUP: &str = "Shell Search Provider";

/// `MAX_LIST_SEARCH_RESULTS_ROWS` (`search.js:23`): how many rows a provider's section shows.
pub const MAX_LIST_ROWS: usize = 5;

/// One `.ini` file, as read — before the settings or the app catalog had a say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    pub desktop_id: String,
    pub bus_name: String,
    pub object_path: String,
    /// `Version=2` or later speaks `SearchProvider2`, which adds `LaunchSearch` and the
    /// terms/timestamp arguments of `ActivateResult`.
    pub version2: bool,
    /// `AutoStart=false` sends every call with D-Bus's no-auto-start flag, so a provider that is
    /// not running stays not running.
    pub auto_start: bool,
    /// `DefaultDisabled=true` providers run only when listed in `enabled`.
    pub default_enabled: bool,
}

/// `org.gnome.desktop.search-providers`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchProviderSettings {
    pub disabled: Vec<String>,
    pub enabled: Vec<String>,
    pub disable_external: bool,
    pub sort_order: Vec<String>,
}

/// Where a provider answers, and how to call it — what the client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEndpoint {
    /// The provider's identity on both sides of the seam: its app's desktop id.
    pub id: String,
    pub bus_name: String,
    pub object_path: String,
    pub version2: bool,
    pub auto_start: bool,
}

/// A provider that will be searched, with what its section header shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub endpoint: ProviderEndpoint,
    /// The app's name, the section header's label (`ProviderInfo`).
    pub name: String,
    pub icon: AppIconRef,
}

impl Provider {
    pub fn id(&self) -> &str {
        &self.endpoint.id
    }

    /// Whether the header can open the search in the app (`canLaunchSearch`): only
    /// `SearchProvider2` has `LaunchSearch`.
    pub fn can_launch_search(&self) -> bool {
        self.endpoint.version2
    }
}

/// Read one provider file (`loadRemoteSearchProvider`, `remoteSearch.js:75-139`).
pub fn parse_provider_file(path: &Path) -> Option<ProviderSpec> {
    let keyfile = glib::KeyFile::new();
    keyfile
        .load_from_file(path, glib::KeyFileFlags::NONE)
        .ok()?;
    parse_keyfile(&keyfile).or_else(|| {
        debug!("ignoring search provider {}", path.display());
        None
    })
}

fn parse_keyfile(keyfile: &glib::KeyFile) -> Option<ProviderSpec> {
    if !keyfile.has_group(KEY_FILE_GROUP) {
        return None;
    }
    let string = |key: &str| keyfile.string(KEY_FILE_GROUP, key).ok().map(String::from);
    let boolean = |key: &str| keyfile.boolean(KEY_FILE_GROUP, key).ok();
    // `version >= 2` compares a string with a number, so JS converts: "2" and "3" qualify, and
    // anything that is not a number does not.
    let version2 = string("Version")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .is_some_and(|v| v >= 2.);
    Some(ProviderSpec {
        bus_name: string("BusName")?,
        object_path: string("ObjectPath")?,
        desktop_id: string("DesktopId")?,
        version2,
        auto_start: boolean("AutoStart").unwrap_or(true),
        default_enabled: !boolean("DefaultDisabled").unwrap_or(false),
    })
}

/// The directories providers are read from: `gnome-shell/search-providers` under each system
/// data dir. Not the user's own data dir — `collectFromDatadirs('search-providers', false)`.
pub fn provider_dirs() -> Vec<PathBuf> {
    glib::system_data_dirs()
        .into_iter()
        .map(|dir| dir.join("gnome-shell").join("search-providers"))
        .collect()
}

/// Every provider file in `dirs`, the first one per object path winning
/// (`objectPaths[objectPath]`, `remoteSearch.js:95-96`).
///
/// Within a directory files are taken in name order, where gnome-shell takes the order the
/// directory enumerates in, which is arbitrary.
pub fn discover(dirs: &[PathBuf]) -> Vec<ProviderSpec> {
    let mut specs: Vec<ProviderSpec> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let Some(spec) = parse_provider_file(&path) else {
                continue;
            };
            if specs.iter().any(|s| s.object_path == spec.object_path) {
                continue;
            }
            specs.push(spec);
        }
    }
    specs
}

/// The providers to search, in section order (`loadRemoteSearchProviders`,
/// `remoteSearch.js:140-186`): a provider needs a shown app, the settings decide which run, and
/// `sort-order` decides where, the rest following by app name.
pub fn select(
    specs: &[ProviderSpec],
    settings: &SearchProviderSettings,
    lookup: impl Fn(&str) -> Option<AppEntry>,
) -> Vec<Provider> {
    if settings.disable_external {
        return Vec::new();
    }
    let mut providers: Vec<Provider> = Vec::new();
    for spec in specs {
        // A provider is known by its app, on both sides of the seam; a second file for the same
        // app would have its answers taken for the first's.
        if providers.iter().any(|p| p.id() == spec.desktop_id) {
            continue;
        }
        let provider = (|| {
            let app = lookup(&spec.desktop_id).filter(|app| app.should_show)?;
            let id = &spec.desktop_id;
            let wanted = if spec.default_enabled {
                !settings.disabled.contains(id)
            } else {
                settings.enabled.contains(id)
            };
            wanted.then(|| Provider {
                endpoint: ProviderEndpoint {
                    id: id.clone(),
                    bus_name: spec.bus_name.clone(),
                    object_path: spec.object_path.clone(),
                    version2: spec.version2,
                    auto_start: spec.auto_start,
                },
                name: app.name,
                icon: app.icon,
            })
        })();
        providers.extend(provider);
    }
    let rank = |p: &Provider| settings.sort_order.iter().position(|id| id == p.id());
    providers.sort_by(|a, b| match (rank(a), rank(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => glib::CollationKey::from(&a.name).cmp(&glib::CollationKey::from(&b.name)),
    });
    providers
}

/// A result's icon, as the provider described it (`RemoteSearchProvider.createIcon`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultIcon {
    /// Theme names, best first.
    Themed(Vec<String>),
    /// An image file on disk.
    File(PathBuf),
    /// Pixels the provider sent, as straight-alpha RGBA rows.
    Pixels {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
}

/// One result, validated (`getResultMetas`, `search.js:236-257`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultMeta {
    pub id: String,
    pub name: String,
    /// Its first line only — the only line a row shows (`_highlightTerms`).
    pub description: Option<String>,
    pub icon: Option<ResultIcon>,
    /// Copied to the clipboard when the result is activated (Calculator's answer).
    pub clipboard_text: Option<String>,
}

/// What one provider found for one search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionResults {
    /// The rows to show, at most [`MAX_LIST_ROWS`] regular ones (`filterResults`).
    pub rows: Vec<ResultMeta>,
    /// How many results did not make it into `rows` — the header's "N more". Zero for a
    /// provider that cannot launch the search, as there is nowhere to see them.
    pub more: usize,
}

/// The compositor's requests to the provider client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SynoikToSearch {
    /// The provider set changed (startup, `installed-changed`, a settings change). Forgets
    /// every earlier result.
    SetProviders(Vec<ProviderEndpoint>),
    /// Search every provider. Replies carry `generation`, and a newer request makes an older
    /// one's replies stale. Whether it is a sub-search of the last completed one is the
    /// client's call — it holds the results that would be refined.
    Search { generation: u64, terms: Vec<String> },
    /// The search was cleared: drop the remembered results and metas (`_reset`, `clear`), and
    /// retire whatever is in flight — `generation` is the compositor's, past every search sent.
    Reset { generation: u64 },
    ActivateResult {
        provider: String,
        id: String,
        terms: Vec<String>,
        timestamp: u32,
    },
    LaunchSearch {
        provider: String,
        terms: Vec<String>,
        timestamp: u32,
    },
}

/// The provider client's replies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchToSynoik {
    /// One provider finished searching `generation`. `None` when it found nothing, failed, or
    /// answered something invalid — all of which show as no section.
    Results {
        generation: u64,
        provider: String,
        results: Option<SectionResults>,
    },
}

/// `filterResults` (`remoteSearch.js:224-232`): at most `max` regular results, and at most
/// `max` of the `special:` ones a provider can append (a "search the web" row, say) after them.
pub fn filter_results(results: &[String], max: usize) -> Vec<String> {
    if results.len() <= max {
        return results.to_vec();
    }
    let (special, regular): (Vec<&String>, Vec<&String>) =
        results.iter().partition(|r| r.starts_with("special:"));
    regular
        .into_iter()
        .take(max)
        .chain(special.into_iter().take(max))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyfile(text: &str) -> Option<ProviderSpec> {
        let kf = glib::KeyFile::new();
        kf.load_from_data(text, glib::KeyFileFlags::NONE).unwrap();
        parse_keyfile(&kf)
    }

    #[test]
    fn a_provider_file_reads_like_remote_search() {
        let spec = keyfile(
            "[Shell Search Provider]\nDesktopId=org.gnome.Nautilus.desktop\n\
             BusName=org.gnome.Nautilus\nObjectPath=/org/gnome/Nautilus/SearchProvider\n\
             Version=2\n",
        )
        .unwrap();
        assert!(spec.version2);
        assert!(spec.auto_start, "AutoStart defaults to true");
        assert!(spec.default_enabled, "DefaultDisabled defaults to false");

        let v1 = keyfile(
            "[Shell Search Provider]\nDesktopId=a.desktop\nBusName=a\nObjectPath=/a\n\
             AutoStart=false\nDefaultDisabled=true\n",
        )
        .unwrap();
        assert!(!v1.version2, "no Version is version 1");
        assert!(!v1.auto_start);
        assert!(!v1.default_enabled);

        assert_eq!(
            keyfile("[Shell Search Provider]\nBusName=a\nObjectPath=/a\n"),
            None,
            "a provider without a DesktopId is ignored"
        );
        assert_eq!(keyfile("[Desktop Entry]\nName=x\n"), None);
    }

    fn spec(id: &str, default_enabled: bool) -> ProviderSpec {
        ProviderSpec {
            desktop_id: id.to_owned(),
            bus_name: format!("bus.{id}"),
            object_path: format!("/{}", id.replace('.', "/")),
            version2: true,
            auto_start: true,
            default_enabled,
        }
    }

    fn catalog(id: &str) -> Option<AppEntry> {
        let name = match id {
            "files.desktop" => "Files",
            "calc.desktop" => "Calculator",
            "clocks.desktop" => "Clocks",
            "hidden.desktop" => {
                return Some(AppEntry {
                    should_show: false,
                    ..AppEntry::fake(id, "Hidden")
                })
            }
            _ => return None,
        };
        Some(AppEntry::fake(id, name))
    }

    fn ids(providers: &[Provider]) -> Vec<&str> {
        providers.iter().map(Provider::id).collect()
    }

    #[test]
    fn selection_follows_the_settings_then_the_app_names() {
        let specs = [
            spec("files.desktop", true),
            spec("calc.desktop", true),
            spec("clocks.desktop", false),
            spec("hidden.desktop", true),
            spec("missing.desktop", true),
        ];
        let mut settings = SearchProviderSettings::default();
        assert_eq!(
            ids(&select(&specs, &settings, catalog)),
            ["calc.desktop", "files.desktop"],
            "by name; a default-disabled, hidden or uninstalled app's provider does not run"
        );

        settings.enabled = vec!["clocks.desktop".into()];
        settings.disabled = vec!["calc.desktop".into()];
        settings.sort_order = vec!["files.desktop".into()];
        assert_eq!(
            ids(&select(&specs, &settings, catalog)),
            ["files.desktop", "clocks.desktop"],
            "sort-order first, the rest by name"
        );

        settings.disable_external = true;
        assert!(select(&specs, &settings, catalog).is_empty());
    }

    #[test]
    fn discovery_keeps_the_first_file_per_object_path() {
        let root =
            std::env::temp_dir().join(format!("synoik-search-providers-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let body = |id: &str| {
            format!("[Shell Search Provider]\nDesktopId={id}\nBusName=bus\nObjectPath=/same\n")
        };
        std::fs::write(a.join("one.search-provider.ini"), body("first.desktop")).unwrap();
        std::fs::write(b.join("two.search-provider.ini"), body("second.desktop")).unwrap();
        std::fs::write(b.join("junk.ini"), "not a key file [").unwrap();
        let found = discover(&[a, b]);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].desktop_id, "first.desktop");
    }

    #[test]
    fn filtering_keeps_special_results_after_the_regular_ones() {
        let results: Vec<String> = ["a", "special:web", "b", "c", "d", "e", "f", "g"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            filter_results(&results, 5),
            ["a", "b", "c", "d", "e", "special:web"]
        );
        assert_eq!(filter_results(&results[..3], 5), &results[..3]);
    }
}
