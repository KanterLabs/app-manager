use crate::models::AppState;
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref().to_path_buf();
        reject_symlink(&root)?;
        fs::create_dir_all(&root).context("Cannot create application data directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let store = Self { root };
        let lock = store.lock()?;
        let path = store.state_path();
        if !path.exists() {
            if fs::read_dir(&store.root)?.any(|entry| match entry {
                Ok(entry) => entry.file_name() != "state.lock",
                Err(_) => true,
            }) {
                bail!("Saved state is missing from a populated app directory; explicit recovery is required")
            }
            store.write_atomic(&path, &serde_json::to_vec_pretty(&AppState::default())?)?;
        } else {
            reject_symlink(&path)?;
            let bytes = fs::read(&path).context("Cannot read saved application state")?;
            let mut state: AppState = serde_json::from_slice(&bytes).context(
                "Saved application state is invalid; it has been preserved for recovery",
            )?;
            if state.schema == 0 {
                store.backup(&bytes)?;
                state.schema = 1;
                let mut next = serde_json::to_value(&state)?;
                preserve_unknown(&serde_json::from_slice(&bytes)?, &mut next);
                store.write_atomic(&path, &serde_json::to_vec_pretty(&next)?)?;
            } else if state.schema != 1 {
                bail!(
                    "Unsupported state schema {}; saved data has not been changed",
                    state.schema
                );
            }
        }
        FileExt::unlock(&lock)?;
        Ok(store)
    }

    pub fn default_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("APP_MANAGER_DATA_DIR") {
            return Ok(path.into());
        }
        directories::ProjectDirs::from("dev", "KanterLabs", "AppManager")
            .map(|dirs| dirs.data_dir().to_path_buf())
            .context("Cannot locate the user application data directory")
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn extensions_directory() -> Result<PathBuf> {
        directories::BaseDirs::new()
            .map(|dirs| dirs.data_dir().join("gnome-shell/extensions"))
            .context("Cannot locate the user GNOME extension directory")
    }
    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    fn lock(&self) -> Result<File> {
        let path = self.root.join("state.lock");
        reject_symlink(&path)?;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(path)
            .context("Cannot open application state lock")?;
        file.lock_exclusive()
            .context("Cannot lock application state")?;
        Ok(file)
    }

    pub fn snapshot(&self) -> Result<AppState> {
        let lock = self.lock()?;
        let state = self.read();
        FileExt::unlock(&lock)?;
        state
    }

    fn read(&self) -> Result<AppState> {
        reject_symlink(&self.state_path())?;
        let state: AppState = serde_json::from_slice(&fs::read(self.state_path())?)
            .context("Saved state is invalid; refusing to replace it")?;
        if state.schema != 1 {
            bail!("Unsupported saved state schema")
        }
        Ok(state)
    }

    pub fn update<T>(&self, change: impl FnOnce(&mut AppState) -> Result<T>) -> Result<T> {
        let lock = self.lock()?;
        let result = (|| {
            let mut state = self.read()?;
            let value = change(&mut state)?;
            if state.schema != 1 {
                bail!("A transaction cannot change the state schema")
            }
            let old = fs::read(self.state_path())?;
            let mut next_value = serde_json::to_value(&state)?;
            preserve_unknown(&serde_json::from_slice(&old)?, &mut next_value);
            let next = serde_json::to_vec_pretty(&next_value)?;
            if old != next {
                self.backup(&old)?;
                self.write_atomic(&self.state_path(), &next)?;
            }
            Ok(value)
        })();
        FileExt::unlock(&lock)?;
        result
    }

    fn backup(&self, bytes: &[u8]) -> Result<()> {
        let dir = self.root.join("backups");
        reject_symlink(&dir)?;
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!(
            "state-{}-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%S"),
            uuid::Uuid::new_v4()
        ));
        self.write_atomic(&path, bytes)?;
        if fs::read(&path)? != bytes {
            bail!("State backup verification failed; update refused")
        }
        Ok(())
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        reject_symlink(path)?;
        let dir = path.parent().context("Missing destination directory")?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
        File::open(dir)?.sync_all()?;
        Ok(())
    }
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("Application data paths must not be symbolic links")
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

// Retained binaries must not erase fields added by a newer compatible release.
// Arrays keep only the caller's selected entries, preserving unknown fields for
// entries whose stable IDs remain present. Explicit removal still removes them.
fn preserve_unknown(old: &serde_json::Value, next: &mut serde_json::Value) {
    match (old, next) {
        (serde_json::Value::Object(old), serde_json::Value::Object(next)) => {
            for (key, value) in old {
                if let Some(current) = next.get_mut(key) {
                    preserve_unknown(value, current)
                } else {
                    next.insert(key.clone(), value.clone());
                }
            }
        }
        (serde_json::Value::Array(old), serde_json::Value::Array(next)) => {
            for entry in next {
                if let Some(key) = ["id", "appId"]
                    .into_iter()
                    .find(|key| entry.get(*key).is_some())
                {
                    let id = entry.get(key).cloned();
                    if let Some(previous) = old.iter().find(|v| v.get(key) == id.as_ref()) {
                        preserve_unknown(previous, entry)
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MachineProfile;
    #[test]
    fn populated_migration_preserves_ids_unknown_fields_and_verified_backup() {
        let root = tempfile::tempdir().unwrap();
        let initial = serde_json::json!({"schema":0,"machines":[{"id":"stable-id","name":"Dev VM","host":"dev-vm","port":22,"workspaceRoots":["~/projects"],"futureMachineOption":true}],"installed":[{"appId":"example","version":"1","adapter":"gnome-extension","extensionUuid":"example@example.invalid","artifactSha256":"a".repeat(64),"retainedArtifact":"artifacts/example.zip","installedAt":"2026-10-01T00:00:00Z","futureInstalledOption":true}],"futureSetting":{"enabled":true}});
        let bytes = serde_json::to_vec(&initial).unwrap();
        fs::write(root.path().join("state.json"), &bytes).unwrap();
        let store = Store::open(root.path()).unwrap();
        let state = store.snapshot().unwrap();
        assert_eq!(state.schema, 1);
        assert_eq!(state.machines[0].id, "stable-id");
        assert_eq!(state.extra["futureSetting"]["enabled"], true);
        let backup = fs::read_dir(root.path().join("backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(fs::read(backup).unwrap(), bytes);
        store
            .update(|state| {
                state.machines[0].name = "Renamed VM".into();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            Store::open(root.path()).unwrap().snapshot().unwrap().extra["futureSetting"],
            initial["futureSetting"]
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("state.json")).unwrap()).unwrap();
        assert_eq!(saved["machines"][0]["futureMachineOption"], true);
        assert_eq!(saved["installed"][0]["futureInstalledOption"], true);
    }
    #[test]
    fn corruption_and_future_schema_are_never_reset() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        for bytes in [
            b"corrupt".as_slice(),
            br#"{"schema":99,"precious":"keep"}"#.as_slice(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(Store::open(root.path()).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
    #[test]
    fn missing_state_in_populated_directory_is_not_recreated() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        store
            .update(|s| {
                s.extra.insert("precious".into(), serde_json::json!(true));
                Ok(())
            })
            .unwrap();
        fs::remove_file(store.state_path()).unwrap();
        assert!(Store::open(root.path()).is_err());
        assert!(!store.state_path().exists());
        assert!(fs::read_dir(root.path().join("backups"))
            .unwrap()
            .next()
            .is_some());
    }
    #[test]
    fn concurrent_instances_do_not_lose_profile_updates() {
        let root = tempfile::tempdir().unwrap();
        Store::open(root.path()).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let path = root.path().to_owned();
                std::thread::spawn(move || {
                    Store::open(path)
                        .unwrap()
                        .update(|s| {
                            s.machines.push(MachineProfile {
                                id: n.to_string(),
                                name: n.to_string(),
                                host: "dev-vm".into(),
                                user: None,
                                port: 22,
                                workspace_roots: vec![],
                            });
                            Ok(())
                        })
                        .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            Store::open(root.path())
                .unwrap()
                .snapshot()
                .unwrap()
                .machines
                .len(),
            8
        );
    }
    #[test]
    fn rejected_transaction_does_not_mutate_saved_data() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let before = fs::read(store.state_path()).unwrap();
        let failed = store.update::<()>(|s| {
            s.machines.clear();
            bail!("refused")
        });
        assert!(failed.is_err());
        assert_eq!(fs::read(store.state_path()).unwrap(), before);
    }
}
