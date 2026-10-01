#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{bail, Context, Result};
use app_manager_core::{
    broker_client::BrokerClient, installer, machines, models::*, releases, store::Store,
};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tauri::Manager;

#[derive(Clone)]
struct Desktop {
    store: Store,
    active_installs: Arc<AtomicUsize>,
    operations: Arc<tokio::sync::Mutex<()>>,
    broker_operations: Arc<tokio::sync::Mutex<()>>,
}
#[derive(Serialize)]
struct Snapshot {
    state: AppState,
    host: HostInfo,
}
type CommandResult<T> = std::result::Result<T, String>;
fn error(err: anyhow::Error) -> String {
    err.to_string()
}

fn host_info(store: &Store) -> HostInfo {
    let version = std::process::Command::new("gnome-shell")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| {
            text.split_whitespace()
                .find_map(|part| part.split('.').next()?.parse().ok())
        });
    HostInfo {
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        gnome_version: version,
        data_directory: store.root().display().to_string(),
        extensions_directory: Store::extensions_directory()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}
#[tauri::command]
fn get_snapshot(desktop: tauri::State<'_, Desktop>) -> CommandResult<Snapshot> {
    Ok(Snapshot {
        state: desktop.store.snapshot().map_err(error)?,
        host: host_info(&desktop.store),
    })
}
#[tauri::command]
fn save_machine(
    desktop: tauri::State<'_, Desktop>,
    mut profile: MachineProfile,
) -> CommandResult<AppState> {
    if profile.id.is_empty() {
        profile.id = uuid::Uuid::new_v4().to_string()
    }
    machines::validate_profile(&profile).map_err(error)?;
    desktop
        .store
        .update(|s| {
            if let Some(old) = s.machines.iter_mut().find(|m| m.id == profile.id) {
                *old = profile
            } else {
                s.machines.push(profile)
            }
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
fn remove_machine(desktop: tauri::State<'_, Desktop>, id: String) -> CommandResult<AppState> {
    desktop
        .store
        .update(|s| {
            s.machines.retain(|m| m.id != id);
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
async fn test_machine(
    desktop: tauri::State<'_, Desktop>,
    id: String,
) -> CommandResult<ConnectionReport> {
    let profile = desktop
        .store
        .snapshot()
        .map_err(error)?
        .machines
        .into_iter()
        .find(|m| m.id == id)
        .context("Saved machine not found")
        .map_err(error)?;
    machines::test_connection(&profile).await.map_err(error)
}
#[tauri::command]
async fn discover_workspaces(
    desktop: tauri::State<'_, Desktop>,
    id: String,
) -> CommandResult<Vec<Workspace>> {
    let profile = desktop
        .store
        .snapshot()
        .map_err(error)?
        .machines
        .into_iter()
        .find(|m| m.id == id)
        .context("Saved machine not found")
        .map_err(error)?;
    machines::discover_workspaces(&profile).await.map_err(error)
}
#[tauri::command]
fn save_source(desktop: tauri::State<'_, Desktop>, source: AppSource) -> CommandResult<AppState> {
    releases::validate_source(&source).map_err(error)?;
    desktop
        .store
        .update(|s| {
            if let Some(old) = s.sources.iter_mut().find(|a| a.id == source.id) {
                *old = source
            } else {
                s.sources.push(source)
            }
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
fn remove_source(desktop: tauri::State<'_, Desktop>, id: String) -> CommandResult<AppState> {
    desktop
        .store
        .update(|s| {
            s.sources.retain(|a| a.id != id);
            s.releases.retain(|r| r.app_id != id);
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
async fn refresh_releases(desktop: tauri::State<'_, Desktop>) -> CommandResult<AppState> {
    let sources = desktop.store.snapshot().map_err(error)?.sources;
    let mut fresh = vec![];
    let mut failures = vec![];
    for source in sources {
        match releases::refresh(&source).await {
            Ok(release) => fresh.push(release),
            Err(_) => failures.push(source.name),
        }
    }
    desktop
        .store
        .update(|s| {
            for release in fresh {
                s.releases.retain(|r| r.app_id != release.app_id);
                s.releases.push(release)
            }
            Ok(())
        })
        .map_err(error)?;
    if !failures.is_empty() {
        return Err(format!("Could not verify releases for: {}. Previous cached releases are unchanged; check the source URLs and publication receipts.",failures.join(", ")));
    }
    desktop.store.snapshot().map_err(error)
}
struct InstallGuard(Arc<AtomicUsize>);
impl Drop for InstallGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[tauri::command]
async fn install_app(
    desktop: tauri::State<'_, Desktop>,
    app_id: String,
) -> CommandResult<AppState> {
    let _lock = desktop.operations.lock().await;
    desktop.active_installs.fetch_add(1, Ordering::SeqCst);
    let _guard = InstallGuard(desktop.active_installs.clone());
    let state = desktop.store.snapshot().map_err(error)?;
    let source = state
        .sources
        .iter()
        .find(|s| s.id == app_id)
        .context("App source not found")
        .map_err(error)?;
    // Revalidate the receipt immediately before changing installed files.
    let release = releases::refresh(source).await.map_err(error)?;
    desktop
        .store
        .update(|s| {
            s.releases.retain(|r| r.app_id != release.app_id);
            s.releases.push(release.clone());
            Ok(())
        })
        .map_err(error)?;
    installer::install(&desktop.store, source, &release, &host_info(&desktop.store))
        .await
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
async fn rollback_app(
    desktop: tauri::State<'_, Desktop>,
    app_id: String,
) -> CommandResult<AppState> {
    let _lock = desktop.operations.lock().await;
    desktop.active_installs.fetch_add(1, Ordering::SeqCst);
    let _guard = InstallGuard(desktop.active_installs.clone());
    installer::rollback(&desktop.store, &app_id, &host_info(&desktop.store))
        .await
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
fn credential(connection: &BrokerConnection) -> Result<keyring::Entry> {
    keyring::Entry::new(
        "dev.kanterlabs.appmanager.broker",
        &format!("{}@{}", connection.account, connection.url),
    )
    .map_err(|_| {
        anyhow::anyhow!("Native credential store is unavailable; unlock your desktop keyring")
    })
}
async fn broker_client(store: &Store) -> Result<BrokerClient> {
    let connection = store
        .snapshot()?
        .broker
        .context("Connect your integration broker first")?;
    let base = connection.url.clone();
    let token = tokio::task::spawn_blocking(move || {
        credential(&connection)?.get_password().map_err(|_| {
            anyhow::anyhow!("Broker credential is unavailable; unlock your keyring or reconnect")
        })
    })
    .await??;
    BrokerClient::new(&base, &token)
}
#[tauri::command]
async fn connect_broker(
    desktop: tauri::State<'_, Desktop>,
    url: String,
    account: String,
    token: String,
) -> CommandResult<AppState> {
    let _broker_lock = desktop.broker_operations.lock().await;
    if account.trim().is_empty() || account.len() > 120 || account.chars().any(char::is_control) {
        return Err("Enter a valid account label".into());
    }
    let client = BrokerClient::new(&url, &token).map_err(error)?;
    let repositories = client.repositories().await.map_err(error)?;
    let builds = client.builds().await.map_err(error)?;
    let connection = BrokerConnection { url, account };
    let save_connection = connection.clone();
    tokio::task::spawn_blocking(move || {
        credential(&save_connection)?
            .set_password(&token)
            .map_err(|_| anyhow::anyhow!("Cannot save credential; unlock your desktop keyring"))
    })
    .await
    .map_err(|_| "Credential operation failed".to_string())?
    .map_err(error)?;
    desktop
        .store
        .update(|s| {
            s.broker = Some(connection);
            s.repositories = repositories;
            s.builds = builds;
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
async fn disconnect_broker(desktop: tauri::State<'_, Desktop>) -> CommandResult<AppState> {
    let _broker_lock = desktop.broker_operations.lock().await;
    if let Some(connection) = desktop.store.snapshot().map_err(error)?.broker {
        tokio::task::spawn_blocking(move || match credential(&connection)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => bail!(
                "Could not remove the native broker credential; unlock your keyring and retry"
            ),
        })
        .await
        .map_err(|_| "Credential operation failed".to_string())?
        .map_err(error)?;
    }
    desktop
        .store
        .update(|s| {
            s.broker = None;
            s.repositories.clear();
            s.builds.clear();
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
async fn refresh_broker(desktop: tauri::State<'_, Desktop>) -> CommandResult<AppState> {
    let _broker_lock = desktop.broker_operations.lock().await;
    let client = broker_client(&desktop.store).await.map_err(error)?;
    let repositories = client.repositories().await.map_err(error)?;
    let builds = client.builds().await.map_err(error)?;
    desktop
        .store
        .update(|s| {
            s.repositories = repositories;
            s.builds = builds;
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}
#[tauri::command]
async fn request_build(
    desktop: tauri::State<'_, Desktop>,
    repository_id: String,
    reference: String,
    commit: String,
) -> CommandResult<AppState> {
    let _broker_lock = desktop.broker_operations.lock().await;
    let client = broker_client(&desktop.store).await.map_err(error)?;
    let build = client
        .request_build(&repository_id, &reference, &commit)
        .await
        .map_err(error)?;
    desktop
        .store
        .update(|s| {
            s.builds.retain(|b| b.id != build.id);
            s.builds.insert(0, build);
            Ok(())
        })
        .map_err(error)?;
    desktop.store.snapshot().map_err(error)
}

fn main() {
    let result = (|| -> Result<()> {
        let store = Store::open(Store::default_path()?)?;
        // Interrupted changes need inspection, never an automatic reset/restore.
        store.update(|s| {
            for receipt in &mut s.operations {
                if matches!(
                    receipt.status.as_str(),
                    "pending" | "started" | "staging" | "installing"
                ) {
                    receipt.status = "recovery_required".into();
                    // Keep message: the installer needs its durable recovery context.
                }
            }
            Ok(())
        })?;
        let desktop = Desktop {
            store,
            active_installs: Arc::new(AtomicUsize::new(0)),
            operations: Arc::new(tokio::sync::Mutex::new(())),
            broker_operations: Arc::new(tokio::sync::Mutex::new(())),
        };
        tauri::Builder::default()
            .manage(desktop)
            .invoke_handler(tauri::generate_handler![
                get_snapshot,
                save_machine,
                remove_machine,
                test_machine,
                discover_workspaces,
                save_source,
                remove_source,
                refresh_releases,
                install_app,
                rollback_app,
                connect_broker,
                disconnect_broker,
                refresh_broker,
                request_build
            ])
            .on_window_event(|window, event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    if window
                        .state::<Desktop>()
                        .active_installs
                        .load(Ordering::SeqCst)
                        > 0
                    {
                        api.prevent_close();
                    }
                }
            })
            .run(tauri::generate_context!())
            .context("Cannot start the desktop application")?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("App Manager: {err}");
        std::process::exit(1)
    }
}
