// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, missing_docs, clippy::all)]

//! Gnome search provider for VSCode editors.

use std::convert::TryInto;
use std::fs::File;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Error, Result};
use gio::AppInfoExt;
use log::{debug, error, info, warn};
use serde::Deserialize;

use gnome_search_provider_common::dbus::{acquire_bus_name, RecentItemSearchProvider};
use gnome_search_provider_common::*;

#[derive(Debug, Deserialize)]
struct StorageOpenedPathsListEntry {
    #[serde(rename = "folderUri")]
    folder_uri: Option<String>,
    #[serde(rename = "fileUri")]
    file_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StorageOpenedPathsList {
    /// Up to code 1.54
    workspaces3: Option<Vec<String>>,
    /// From code 1.55
    entries: Option<Vec<StorageOpenedPathsListEntry>>,
}

#[derive(Debug, Deserialize)]
struct Storage {
    #[serde(rename = "openedPathsList")]
    opened_paths_list: Option<StorageOpenedPathsList>,
}

impl Storage {
    /// Read a VSCode storage.json from the given `reader`.
    fn read<R: Read>(reader: R) -> Result<Self> {
        serde_json::from_reader(reader).map_err(Into::into)
    }

    /// Read the `storage.json` file in the given `config_dir`.
    fn from_dir<P: AsRef<Path>>(config_dir: P) -> Result<Self> {
        let path = config_dir.as_ref().join("storage.json");
        Self::read(
            File::open(&path)
                .with_context(|| format!("Failed to open {} for reading", path.display()))?,
        )
        .with_context(|| format!("Failed to parse storage from {}", path.display()))
    }

    /// Move this storage into workspace URLs.
    fn into_workspace_urls(self) -> Vec<String> {
        if let Some(paths) = self.opened_paths_list {
            let entries = paths.entries.unwrap_or_default();
            let workspaces3 = paths.workspaces3.unwrap_or_default();
            entries
                .into_iter()
                .filter_map(|entry| entry.folder_uri)
                .chain(workspaces3.into_iter())
                .collect()
        } else {
            Vec::new()
        }
    }
}

#[derive(Debug, Copy, Clone)]
struct ConfigLocation<'a> {
    dirname: &'a str,
}

/// A search provider to expose from this service.
struct ProviderDefinition<'a> {
    /// A human readable label for this provider.
    label: &'a str,
    /// The ID (that is, the filename) of the desktop file of the corresponding app.
    desktop_id: &'a str,
    /// The relative object path to expose this provider at.
    relative_obj_path: &'a str,
    /// The location of the configuration for this app.
    config: ConfigLocation<'a>,
}

impl ProviderDefinition<'_> {
    /// Gets the full object path for this provider.
    fn objpath(&self) -> String {
        format!("/de/swsnr/searchprovider/vscode/{}", self.relative_obj_path)
    }
}

/// Known search providers.
///
/// For each definition in this array a corresponding provider file must exist in
/// `providers/`; the file must refer to the same `desktop_id` and the same object path.
/// The object path must be unique for each desktop ID, to ensure that this service always
/// launches the right application associated with the search provider.
const PROVIDERS: &[ProviderDefinition] = &[
    ProviderDefinition {
        label: "Code OSS (Arch Linux)",
        desktop_id: "code-oss.desktop",
        relative_obj_path: "arch/codeoss",
        config: ConfigLocation {
            dirname: "Code - OSS",
        },
    },
    // The binary AUR package for visual studio code: https://aur.archlinux.org/packages/visual-studio-code-bin/
    ProviderDefinition {
        label: "Visual Studio Code (AUR package)",
        desktop_id: "visual-studio-code.desktop",
        relative_obj_path: "aur/visualstudiocode",
        config: ConfigLocation { dirname: "Code" },
    },
];

/// A recent workspace of a VSCode variant.
#[derive(Debug, PartialEq)]
struct RecentWorkspace {
    /// The human readable nfame.
    name: String,
    /// The workspace URL.
    url: String,
}

fn recent_item(url: String) -> Result<RecentFileSystemItem> {
    if let Some(name) = url.split('/').last() {
        Ok(RecentFileSystemItem {
            name: name.to_string(),
            path: url,
        })
    } else {
        Err(anyhow!("Failed to extract workspace name from URL {}", url))
    }
}

struct VscodeWorkspacesSource {
    app_id: String,
    /// The configuration directory.
    config_dir: PathBuf,
}

impl ItemsSource<RecentFileSystemItem> for VscodeWorkspacesSource {
    type Err = Error;

    fn find_recent_items(&self) -> Result<IdMap<RecentFileSystemItem>, Self::Err> {
        let mut items = IndexMap::new();
        info!("Finding recent workspaces for {}", self.app_id);
        let urls = Storage::from_dir(&self.config_dir)?.into_workspace_urls();
        for url in urls {
            match recent_item(url) {
                Ok(item) => {
                    let id = format!("vscode-search-provider-{}-{}", self.app_id, &item.path);
                    items.insert(id, item);
                }
                Err(err) => {
                    warn!("Skipping workspace: {}", err)
                }
            }
        }
        info!("Found {} workspace(s) for {}", items.len(), self.app_id);
        Ok(items)
    }
}

/// The name to request on the bus.
const BUSNAME: &str = "de.swsnr.searchprovider.VSCode";

/// Starts the DBUS service.
///
/// Connect to the session bus and register a new DBus object for every provider
/// whose underlying app is installed.
///
/// Then register the connection on the Glib main loop and install a callback to
/// handle incoming messages.
///
/// Return the connection and the source ID for the mainloop callback.
fn register_search_providers(object_server: &mut zbus::ObjectServer) -> Result<()> {
    let user_config_dir =
        dirs::config_dir().with_context(|| "No configuration directory for current user!")?;

    for provider in PROVIDERS {
        if let Some(app) = gio::DesktopAppInfo::new(provider.desktop_id) {
            info!(
                "Registering provider for {} at {}",
                provider.desktop_id,
                provider.objpath()
            );
            let source = VscodeWorkspacesSource {
                app_id: app.get_id().unwrap().to_string(),
                config_dir: user_config_dir.join(provider.config.dirname),
            };
            let dbus_provider = RecentItemSearchProvider::new(app, source);
            object_server.at(&provider.objpath().try_into()?, dbus_provider)?;
        }
    }
    Ok(())
}

/// Starts the DBUS service loop.
///
/// Register all providers whose underlying app is installed.
fn start_dbus_service() -> Result<()> {
    let context = glib::MainContext::default();
    if !context.acquire() {
        Err(anyhow!("Failed to acquire main context!"))
    } else {
        let mainloop = glib::MainLoop::new(Some(&context), false);
        let connection =
            zbus::Connection::new_session().with_context(|| "Failed to connect to session bus")?;

        let mut object_server = zbus::ObjectServer::new(&connection);
        register_search_providers(&mut object_server)?;

        info!("All providers registered, acquiring {}", BUSNAME);
        acquire_bus_name(&connection, BUSNAME)?;
        info!("Acquired name {}, handling DBus events", BUSNAME);

        glib::source::unix_fd_add_local(
            connection.as_raw_fd(),
            glib::IOCondition::IN | glib::IOCondition::PRI,
            move |_, condition| {
                debug!("Connection entered IO condition {:?}", condition);
                match object_server.try_handle_next() {
                    Ok(None) => debug!("Interface message processed"),
                    Ok(Some(message)) => warn!("Message not handled by interfaces: {:?}", message),
                    Err(err) => error!("Failed to process message: {:#}", err),
                };
                glib::Continue(true)
            },
        );

        glib::source::unix_signal_add(libc::SIGTERM, {
            let l = mainloop.clone();
            move || {
                debug!("Terminated, quitting mainloop");
                l.quit();
                glib::Continue(false)
            }
        });

        glib::source::unix_signal_add(libc::SIGINT, {
            let l = mainloop.clone();
            move || {
                debug!("Interrupted, quitting mainloop");
                l.quit();
                glib::Continue(false)
            }
        });

        mainloop.run();
        Ok(())
    }
}

fn main() {
    use clap::*;

    let app = app_from_crate!()
        .setting(AppSettings::UnifiedHelpMessage)
        .setting(AppSettings::DontCollapseArgsInUsage)
        .setting(AppSettings::DeriveDisplayOrder)
        .set_term_width(80)
        .after_help(
            "\
Set $RUST_LOG to control the log level",
        )
        .arg(
            Arg::with_name("providers")
                .long("--providers")
                .help("List all providers"),
        );
    let matches = app.get_matches();
    if matches.is_present("providers") {
        let mut labels: Vec<&'static str> = PROVIDERS.iter().map(|p| p.label).collect();
        labels.sort_unstable();
        for label in labels {
            println!("{}", label)
        }
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

        info!(
            "Started jetbrains search provider version: {}",
            env!("CARGO_PKG_VERSION")
        );

        if let Err(err) = start_dbus_service() {
            error!("Failed to start DBus event loop: {}", err);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Storage;

    #[test]
    fn read_recent_workspaces_code_1_54() {
        let data: &[u8] = include_bytes!("tests/code_1_54_storage.json");
        let storage = Storage::read(data).unwrap();
        assert!(
            &storage.opened_paths_list.is_some(),
            "opened paths list missing"
        );
        assert!(
            &storage
                .opened_paths_list
                .as_ref()
                .unwrap()
                .workspaces3
                .is_some(),
            "workspaces3 missing"
        );
        assert_eq!(
            storage.into_workspace_urls(),
            vec![
                "file:///home/foo//mdcat",
                "file:///home/foo//gnome-jetbrains-search-provider",
                "file:///home/foo//gnome-shell",
                "file:///home/foo//sbctl",
            ]
        )
    }

    #[test]
    fn read_recent_workspaces_code_1_55() {
        let data: &[u8] = include_bytes!("tests/code_1_55_storage.json");
        let storage = Storage::read(data).unwrap();
        assert!(
            &storage.opened_paths_list.is_some(),
            "opened paths list missing"
        );
        assert!(
            &storage
                .opened_paths_list
                .as_ref()
                .unwrap()
                .entries
                .is_some(),
            "entries missing"
        );

        assert_eq!(
            storage.into_workspace_urls(),
            vec![
                "file:///home/foo//mdcat",
                "file:///home/foo//gnome-jetbrains-search-provider",
                "file:///home/foo//gnome-shell",
                "file:///home/foo//sbctl",
            ]
        );
    }

    mod providers {
        use crate::{BUSNAME, PROVIDERS};
        use anyhow::{Context, Result};
        use ini::Ini;
        use std::collections::HashSet;
        use std::path::Path;

        struct ProviderFile {
            desktop_id: String,
            object_path: String,
            bus_name: String,
            version: String,
        }

        fn load_all_provider_files() -> Result<Vec<ProviderFile>> {
            let mut providers = Vec::new();
            let ini_files = globwalk::GlobWalkerBuilder::new(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("providers"),
                "*.ini",
            )
            .build()
            .unwrap();
            for entry in ini_files {
                let filepath = entry.unwrap().into_path();
                let ini = Ini::load_from_file(&filepath).with_context(|| {
                    format!("Failed to parse ini file at {}", filepath.display())
                })?;
                let provider = ProviderFile {
                    desktop_id: ini
                        .get_from(Some("Shell Search Provider"), "DesktopId")
                        .with_context(|| format!("DesktopId missing in {}", &filepath.display()))?
                        .to_string(),
                    object_path: ini
                        .get_from(Some("Shell Search Provider"), "ObjectPath")
                        .with_context(|| format!("ObjectPath missing in {}", &filepath.display()))?
                        .to_string(),
                    bus_name: ini
                        .get_from(Some("Shell Search Provider"), "BusName")
                        .with_context(|| format!("BusName missing in {}", &filepath.display()))?
                        .to_string(),
                    version: ini
                        .get_from(Some("Shell Search Provider"), "Version")
                        .with_context(|| format!("Version missing in {}", &filepath.display()))?
                        .to_string(),
                };
                providers.push(provider);
            }

            Ok(providers)
        }

        #[test]
        fn all_providers_have_a_correct_ini_file() {
            let provider_files = load_all_provider_files().unwrap();
            for provider in PROVIDERS {
                let provider_file = provider_files
                    .iter()
                    .find(|p| p.desktop_id == provider.desktop_id);
                assert!(
                    provider_file.is_some(),
                    "Provider INI missing for provider {} with desktop ID {}",
                    provider.label,
                    provider.desktop_id
                );

                assert_eq!(provider_file.unwrap().object_path, provider.objpath());
                assert_eq!(provider_file.unwrap().bus_name, BUSNAME);
                assert_eq!(provider_file.unwrap().version, "2");
            }
        }

        #[test]
        fn no_extra_ini_files_without_providers() {
            let provider_files = load_all_provider_files().unwrap();
            assert_eq!(PROVIDERS.len(), provider_files.len());
        }

        #[test]
        fn desktop_ids_are_unique() {
            let mut ids = HashSet::new();
            for provider in PROVIDERS {
                ids.insert(provider.desktop_id);
            }
            assert_eq!(PROVIDERS.len(), ids.len());
        }

        #[test]
        fn dbus_paths_are_unique() {
            let mut paths = HashSet::new();
            for provider in PROVIDERS {
                paths.insert(provider.objpath());
            }
            assert_eq!(PROVIDERS.len(), paths.len());
        }
    }
}
