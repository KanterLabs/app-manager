use anyhow::{bail, Context, Result};
use app_manager_core::{installer, machines, models::*, releases, store::Store};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "App Manager local operations")]
struct Args {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Status,
    ExportProfiles {
        file: PathBuf,
    },
    ImportProfiles {
        file: PathBuf,
    },
    MachineAdd {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        user: Option<String>,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long)]
        root: Vec<String>,
    },
    MachineTest {
        id: String,
    },
    Workspaces {
        id: String,
    },
    MachineRemove {
        id: String,
    },
    SourceAdd {
        file: PathBuf,
    },
    Refresh,
    Install {
        app_id: String,
    },
    Rollback {
        app_id: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profiles {
    schema: u32,
    machines: Vec<MachineProfile>,
    sources: Vec<AppSource>,
}
fn host(store: &Store) -> HostInfo {
    let gnome_version = std::process::Command::new("gnome-shell")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.split_whitespace()
                .find_map(|v| v.split('.').next()?.parse().ok())
        });
    HostInfo {
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        gnome_version,
        data_directory: store.root().display().to_string(),
        extensions_directory: Store::extensions_directory()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    }
}
#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("appctl: {err}");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    let args = Args::parse();
    let path = match args.data_dir {
        Some(path) => path,
        None => Store::default_path()?,
    };
    let store = Store::open(path)?;
    let result = match args.command {
        Command::Status => serde_json::to_value(store.snapshot()?)?,
        Command::ExportProfiles { file } => {
            let state = store.snapshot()?;
            let profiles = Profiles {
                schema: 1,
                machines: state.machines,
                sources: state.sources,
            };
            for profile in &profiles.machines {
                machines::validate_profile(profile)?
            }
            for source in &profiles.sources {
                releases::validate_source(source)?
            }
            let bytes = serde_json::to_vec_pretty(&profiles)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut output = options
                .open(&file)
                .context("Cannot create profile export; choose a new file")?;
            if let Err(err) = output.write_all(&bytes).and_then(|_| output.sync_all()) {
                drop(output);
                let _ = std::fs::remove_file(&file);
                return Err(err.into());
            }
            serde_json::json!({"exported":true,"machines":profiles.machines.len(),"sources":profiles.sources.len()})
        }
        Command::ImportProfiles { file } => {
            let mut bytes = Vec::new();
            std::fs::File::open(file)?
                .take(1_048_577)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 1_048_576 {
                bail!("Profile import exceeds 1 MiB")
            }
            let profiles: Profiles = serde_json::from_slice(&bytes)?;
            if profiles.schema != 1 {
                bail!("Unsupported profile export schema")
            }
            for profile in &profiles.machines {
                machines::validate_profile(profile)?
            }
            for source in &profiles.sources {
                releases::validate_source(source)?
            }
            let mut ids = std::collections::HashSet::new();
            if profiles.machines.iter().any(|m| !ids.insert(m.id.clone())) {
                bail!("Duplicate machine IDs in import")
            }
            ids.clear();
            if profiles.sources.iter().any(|s| !ids.insert(s.id.clone())) {
                bail!("Duplicate source IDs in import")
            }
            let count = (profiles.machines.len(), profiles.sources.len());
            store.update(|s| {
                if profiles
                    .machines
                    .iter()
                    .any(|m| s.machines.iter().any(|old| old.id == m.id))
                    || profiles
                        .sources
                        .iter()
                        .any(|a| s.sources.iter().any(|old| old.id == a.id))
                {
                    bail!("Import conflicts with saved IDs; existing profiles were preserved")
                }
                s.machines.extend(profiles.machines);
                s.sources.extend(profiles.sources);
                Ok(())
            })?;
            serde_json::json!({"imported":true,"machines":count.0,"sources":count.1})
        }
        Command::MachineAdd {
            name,
            host,
            user,
            port,
            root,
        } => {
            let profile = MachineProfile {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                host,
                user,
                port,
                workspace_roots: root,
            };
            machines::validate_profile(&profile)?;
            store.update(|s| {
                s.machines.push(profile.clone());
                Ok(())
            })?;
            serde_json::to_value(profile)?
        }
        Command::MachineTest { id } => {
            let profile = store
                .snapshot()?
                .machines
                .into_iter()
                .find(|m| m.id == id)
                .context("Saved machine not found")?;
            serde_json::to_value(machines::test_connection(&profile).await?)?
        }
        Command::Workspaces { id } => {
            let profile = store
                .snapshot()?
                .machines
                .into_iter()
                .find(|m| m.id == id)
                .context("Saved machine not found")?;
            serde_json::to_value(machines::discover_workspaces(&profile).await?)?
        }
        Command::MachineRemove { id } => {
            store.update(|s| {
                s.machines.retain(|m| m.id != id);
                Ok(())
            })?;
            serde_json::json!({"removed":true})
        }
        Command::SourceAdd { file } => {
            let source: AppSource = serde_json::from_slice(&std::fs::read(file)?)?;
            releases::validate_source(&source)?;
            store.update(|s| {
                s.sources.retain(|a| a.id != source.id);
                s.sources.push(source.clone());
                Ok(())
            })?;
            serde_json::to_value(source)?
        }
        Command::Refresh => {
            let mut fresh = vec![];
            for source in store.snapshot()?.sources {
                fresh.push(releases::refresh(&source).await?)
            }
            store.update(|s| {
                s.releases = fresh;
                Ok(())
            })?;
            serde_json::to_value(store.snapshot()?)?
        }
        Command::Install { app_id } => {
            let source = store
                .snapshot()?
                .sources
                .into_iter()
                .find(|a| a.id == app_id)
                .context("App source not found")?;
            let release = releases::refresh(&source).await?;
            store.update(|s| {
                s.releases.retain(|r| r.app_id != release.app_id);
                s.releases.push(release.clone());
                Ok(())
            })?;
            serde_json::to_value(
                installer::install(&store, &source, &release, &host(&store)).await?,
            )?
        }
        Command::Rollback { app_id } => {
            serde_json::to_value(installer::rollback(&store, &app_id, &host(&store)).await?)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
