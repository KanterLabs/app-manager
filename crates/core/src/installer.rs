//! Verified GNOME Shell extension installation and rollback.
//!
//! Installation is deliberately limited to the reviewed `gnome-extension`
//! adapter.  A downloaded ZIP is fully inspected before it reaches the user
//! extension directory, and the directory swap is paired with a durable
//! operation receipt so a process crash cannot be reported as a successful
//! install without reconciliation.

use crate::models::{AppRelease, AppSource, HostInfo, InstalledApp, OperationReceipt};
use crate::releases;
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::Builder;
use uuid::Uuid;
use zip::ZipArchive;

const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 4096;
const MAX_ZIP_UNCOMPRESSED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ZIP_MEMBER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MARKER_NAME: &str = ".app-manager-install.json";

/// Download, verify, inspect, stage, and install the selected GNOME extension.
pub async fn install(
    store: &Store,
    source: &AppSource,
    release: &AppRelease,
    host: &HostInfo,
) -> Result<InstalledApp> {
    let _operation_lock = acquire_operation_lock(store)?;
    releases::validate_source(source)?;
    let artifact = releases::select_artifact(release, source, host)?;
    reconcile_pending(store, host)?;

    let bytes = releases::download_bounded(
        &releases::validate_remote_url(&artifact.url, "artifact URL")?,
        artifact
            .size
            .checked_add(1)
            .ok_or_else(|| anyhow!("artifact size overflows bounded download"))?
            .min(MAX_ARTIFACT_BYTES),
    )
    .await?;
    verify_artifact_bytes(&artifact, &bytes)?;
    let extension_uuid = source
        .extension_uuid
        .as_deref()
        .ok_or_else(|| anyhow!("GNOME extension source is missing extension UUID"))?;
    inspect_zip(&bytes, extension_uuid, host.gnome_version)?;

    let retained_artifact = persist_artifact(store.root(), &artifact.sha256, &bytes)?;
    let state = store.snapshot()?;
    let current = state
        .installed
        .iter()
        .find(|item| item.app_id == release.app_id)
        .cloned();
    if let Some(previous) = &current {
        if previous.adapter != source.adapter
            || previous.extension_uuid.as_deref() != Some(extension_uuid)
        {
            bail!("existing installation uses a different reviewed adapter or extension UUID");
        }
        let previous_artifact =
            resolve_retained_artifact(store.root(), &previous.retained_artifact)?;
        let previous_bytes = read_bounded_file(&previous_artifact, MAX_ARTIFACT_BYTES)?;
        if sha256_hex(&previous_bytes) != previous.artifact_sha256 {
            bail!("existing installation's retained artifact is missing or has the wrong digest");
        }
    }
    if state.installed.iter().any(|item| {
        item.app_id != release.app_id
            && item.adapter == source.adapter
            && item.extension_uuid.as_deref() == Some(extension_uuid)
    }) {
        bail!("extension UUID is already tracked for a different app");
    }

    let target_dir = extension_directory(host, extension_uuid)?;
    if path_present(&target_dir)? {
        if current.is_none() {
            bail!("existing extension is not tracked by this app manager store");
        }
        ensure_managed_directory(&target_dir, release.app_id.as_str(), extension_uuid)?;
        if let Some(current) = &current {
            let marker = read_marker(&target_dir)?;
            if marker.artifact_sha256 != current.artifact_sha256
                || marker.version != current.version
                || marker.retained_artifact != current.retained_artifact
            {
                bail!("saved installation state does not match the managed extension");
            }
        }
    }
    let stage_dir = stage_archive(&target_dir, &bytes, extension_uuid, host.gnome_version)?;
    if let Err(error) = copy_user_settings(&target_dir, &stage_dir) {
        let _ = fs::remove_dir_all(&stage_dir);
        return Err(error);
    }

    let installed = InstalledApp {
        app_id: release.app_id.clone(),
        version: release.version.clone(),
        adapter: source.adapter.clone(),
        extension_uuid: Some(extension_uuid.to_string()),
        artifact_sha256: artifact.sha256.clone(),
        retained_artifact: retained_artifact.to_string_lossy().into_owned(),
        installed_at: Utc::now().to_rfc3339(),
        previous: current.map(|previous| Box::new(single_previous(previous))),
    };
    if let Err(error) = write_marker(&stage_dir, &marker_for(&installed)?) {
        let _ = fs::remove_dir_all(&stage_dir);
        return Err(error);
    }
    let operation = match begin_operation(store, &installed, "install", &target_dir, &stage_dir) {
        Ok(operation) => operation,
        Err(error) => {
            let _ = fs::remove_dir_all(&stage_dir);
            return Err(error);
        }
    };
    if let Err(error) = swap_into_place(&stage_dir, &target_dir, &operation) {
        let _ = remove_owned_directory(&stage_dir);
        let _ = mark_operation_failed(
            store,
            &operation.id,
            &format!("installation failed: {error:#}"),
        );
        return Err(error);
    }
    complete_operation(store, &operation.id, &installed)?;
    Ok(installed)
}

/// Roll back an app to its explicitly retained previous artifact.
pub async fn rollback(store: &Store, app_id: &str, host: &HostInfo) -> Result<InstalledApp> {
    let _operation_lock = acquire_operation_lock(store)?;
    if app_id.trim().is_empty() {
        bail!("app id must be non-empty");
    }
    reconcile_pending(store, host)?;
    let state = store.snapshot()?;
    let current = state
        .installed
        .iter()
        .find(|item| item.app_id == app_id)
        .cloned()
        .ok_or_else(|| anyhow!("app is not installed"))?;
    let previous = current
        .previous
        .as_deref()
        .cloned()
        .ok_or_else(|| anyhow!("no compatible retained previous installation exists"))?;
    if previous.adapter != "gnome-extension"
        || current.adapter != previous.adapter
        || previous.extension_uuid != current.extension_uuid
    {
        bail!("retained installation is not compatible with the active extension");
    }
    let extension_uuid = previous
        .extension_uuid
        .as_deref()
        .ok_or_else(|| anyhow!("retained installation is missing extension UUID"))?;
    if !host.os.eq_ignore_ascii_case("linux") {
        bail!("GNOME extension rollback requires a Linux host");
    }
    if !matches!(
        host.architecture.to_ascii_lowercase().as_str(),
        "x86_64" | "amd64" | "x64" | "aarch64" | "arm64"
    ) {
        bail!("unsupported host architecture for GNOME extension rollback");
    }
    let gnome_version = host
        .gnome_version
        .ok_or_else(|| anyhow!("GNOME Shell version is unavailable for rollback"))?;
    let retained_path = resolve_retained_artifact(store.root(), &previous.retained_artifact)?;
    let bytes = read_bounded_file(&retained_path, MAX_ARTIFACT_BYTES)?;
    if sha256_hex(&bytes) != previous.artifact_sha256 {
        bail!("retained rollback artifact digest does not match saved state");
    }
    inspect_zip(&bytes, extension_uuid, Some(gnome_version))?;

    let target_dir = extension_directory(host, extension_uuid)?;
    if path_present(&target_dir)? {
        ensure_managed_directory(&target_dir, app_id, extension_uuid)?;
        let marker = read_marker(&target_dir)?;
        if marker.artifact_sha256 != current.artifact_sha256
            || marker.version != current.version
            || marker.retained_artifact != current.retained_artifact
        {
            bail!("saved installation state does not match the managed extension");
        }
    }
    let stage_dir = stage_archive(&target_dir, &bytes, extension_uuid, host.gnome_version)?;
    if let Err(error) = copy_user_settings(&target_dir, &stage_dir) {
        let _ = fs::remove_dir_all(&stage_dir);
        return Err(error);
    }
    let mut installed = single_previous(previous);
    installed.previous = Some(Box::new(single_previous(current)));
    installed.installed_at = Utc::now().to_rfc3339();
    if let Err(error) = write_marker(&stage_dir, &marker_for(&installed)?) {
        let _ = fs::remove_dir_all(&stage_dir);
        return Err(error);
    }
    let operation = match begin_operation(store, &installed, "rollback", &target_dir, &stage_dir) {
        Ok(operation) => operation,
        Err(error) => {
            let _ = fs::remove_dir_all(&stage_dir);
            return Err(error);
        }
    };
    if let Err(error) = swap_into_place(&stage_dir, &target_dir, &operation) {
        let _ = remove_owned_directory(&stage_dir);
        let _ = mark_operation_failed(store, &operation.id, &format!("rollback failed: {error:#}"));
        return Err(error);
    }
    complete_operation(store, &operation.id, &installed)?;
    Ok(installed)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct InstallMarker {
    app_id: String,
    version: String,
    adapter: String,
    extension_uuid: Option<String>,
    artifact_sha256: String,
    retained_artifact: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct OperationContext {
    target_dir: String,
    stage_dir: String,
    backup_dir: String,
    next_installed: InstalledApp,
    marker: InstallMarker,
}

fn begin_operation(
    store: &Store,
    installed: &InstalledApp,
    action: &str,
    target_dir: &Path,
    stage_dir: &Path,
) -> Result<OperationReceipt> {
    let id = Uuid::new_v4().to_string();
    let backup_dir = target_dir
        .parent()
        .ok_or_else(|| anyhow!("extension directory has no parent"))?
        .join(format!(".app-manager-backup-{id}"));
    let marker = marker_for(installed)?;
    let context = OperationContext {
        target_dir: target_dir.to_string_lossy().into_owned(),
        stage_dir: stage_dir.to_string_lossy().into_owned(),
        backup_dir: backup_dir.to_string_lossy().into_owned(),
        next_installed: installed.clone(),
        marker,
    };
    let operation = OperationReceipt {
        id,
        app_id: installed.app_id.clone(),
        action: action.to_string(),
        status: "started".to_string(),
        version: installed.version.clone(),
        message: serde_json::to_string(&context).context("could not encode operation context")?,
        started_at: Utc::now().to_rfc3339(),
        completed_at: None,
    };
    let saved = operation.clone();
    store.update(|state| {
        state.operations.push(saved);
        Ok(())
    })?;
    Ok(operation)
}

fn complete_operation(store: &Store, operation_id: &str, installed: &InstalledApp) -> Result<()> {
    let completed_at = Utc::now().to_rfc3339();
    store.update(|state| {
        state
            .installed
            .retain(|item| item.app_id != installed.app_id);
        state.installed.push(installed.clone());
        let operation = state
            .operations
            .iter_mut()
            .find(|item| item.id == operation_id)
            .ok_or_else(|| anyhow!("durable operation receipt disappeared"))?;
        operation.status = "completed".to_string();
        operation.completed_at = Some(completed_at);
        Ok(())
    })
}

fn mark_operation_failed(store: &Store, operation_id: &str, message: &str) -> Result<()> {
    let completed_at = Utc::now().to_rfc3339();
    store.update(|state| {
        let operation = state
            .operations
            .iter_mut()
            .find(|item| item.id == operation_id)
            .ok_or_else(|| anyhow!("durable operation receipt disappeared"))?;
        operation.status = "failed".to_string();
        operation.message = message.to_string();
        operation.completed_at = Some(completed_at);
        Ok(())
    })
}

fn reconcile_pending(store: &Store, host: &HostInfo) -> Result<()> {
    let state = store.snapshot()?;
    for operation in state.operations {
        if matches!(operation.status.as_str(), "completed" | "failed") {
            continue;
        }
        if operation.action != "install" && operation.action != "rollback" {
            bail!(
                "operation {} has an unsupported recovery action",
                operation.id
            );
        }
        let context: OperationContext =
            serde_json::from_str(&operation.message).with_context(|| {
                format!("operation {} has an invalid recovery context", operation.id)
            })?;
        if operation.app_id != context.next_installed.app_id
            || marker_for(&context.next_installed)? != context.marker
        {
            bail!(
                "operation {} recovery context does not match its receipt",
                operation.id
            );
        }
        let target = PathBuf::from(&context.target_dir);
        let stage = PathBuf::from(&context.stage_dir);
        let backup = PathBuf::from(&context.backup_dir);
        let expected_target = extension_directory(
            host,
            context
                .marker
                .extension_uuid
                .as_deref()
                .ok_or_else(|| anyhow!("operation context is missing extension UUID"))?,
        )?;
        if target != expected_target {
            bail!(
                "operation {} targets an unexpected extension path",
                operation.id
            );
        }
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("extension path has no parent"))?;
        ensure_owned_internal_path(&stage, parent, ".app-manager-stage-")?;
        ensure_owned_internal_path(&backup, parent, ".app-manager-backup-")?;
        let target_marker = if path_present(&target)? {
            read_marker(&target).ok()
        } else {
            None
        };
        if target_marker.as_ref() == Some(&context.marker) {
            if path_present(&backup)? {
                remove_owned_directory(&backup)?;
            }
            if path_present(&stage)? {
                remove_owned_directory(&stage)?;
            }
            complete_operation(store, &operation.id, &context.next_installed)?;
            continue;
        }
        if !path_present(&target)? && path_present(&backup)? {
            bail!(
                "operation {} requires explicit recovery; active extension is absent and a backup remains",
                operation.id
            );
        }
        if !path_present(&target)? && path_present(&stage)? && !path_present(&backup)? {
            bail!(
                "operation {} requires explicit recovery; staged extension was never activated",
                operation.id
            );
        }
        if path_present(&target)? && path_present(&stage)? && !path_present(&backup)? {
            remove_owned_directory(&stage)?;
            mark_operation_failed(
                store,
                &operation.id,
                "interrupted before activation; existing extension retained",
            )?;
            continue;
        }
        bail!(
            "operation {} has ambiguous filesystem state; manual recovery required",
            operation.id
        );
    }
    Ok(())
}

fn swap_into_place(stage: &Path, target: &Path, operation: &OperationReceipt) -> Result<()> {
    let context: OperationContext =
        serde_json::from_str(&operation.message).context("could not decode operation context")?;
    let backup = PathBuf::from(context.backup_dir);
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("extension directory has no parent"))?;
    ensure_owned_internal_path(&backup, parent, ".app-manager-backup-")?;
    ensure_owned_internal_path(stage, parent, ".app-manager-stage-")?;
    if path_present(&backup)? {
        bail!("stale extension backup path already exists");
    }
    let had_existing = path_present(target)?;
    if had_existing {
        if target.is_symlink() || !target.is_dir() {
            bail!("existing extension path is not a directory");
        }
        fs::rename(target, &backup).context("could not move existing extension aside")?;
    }
    if let Err(error) = fs::rename(stage, target) {
        if had_existing {
            let _ = fs::rename(&backup, target);
        }
        return Err(anyhow!(error).context("could not activate staged extension"));
    }
    if had_existing {
        // Activation is complete once the new directory has been renamed into
        // place. A leftover backup is safe and can be cleaned during receipt
        // reconciliation if this best-effort cleanup encounters an error.
        let _ = remove_owned_directory(&backup);
    }
    Ok(())
}

fn stage_archive(
    target_dir: &Path,
    bytes: &[u8],
    expected_uuid: &str,
    gnome_version: Option<u32>,
) -> Result<PathBuf> {
    let parent = target_dir
        .parent()
        .ok_or_else(|| anyhow!("extension directory has no parent"))?;
    fs::create_dir_all(parent).context("could not create GNOME extension directory")?;
    ensure_real_directory(parent)?;
    let stage = Builder::new()
        .prefix(".app-manager-stage-")
        .tempdir_in(parent)
        .context("could not create extension staging directory")?
        .keep();
    if let Err(error) = extract_zip(bytes, &stage, expected_uuid, gnome_version) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    Ok(stage)
}

fn extract_zip(
    bytes: &[u8],
    destination: &Path,
    expected_uuid: &str,
    gnome_version: Option<u32>,
) -> Result<()> {
    inspect_zip(bytes, expected_uuid, gnome_version)?;
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).context("artifact is not a ZIP archive")?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("could not read ZIP member")?;
        if entry.is_dir() {
            continue;
        }
        let relative = safe_zip_path(entry.name())?;
        let destination_file = destination.join(&relative);
        if !destination_file.starts_with(destination) {
            bail!("ZIP member escapes staging directory");
        }
        if let Some(parent) = destination_file.parent() {
            fs::create_dir_all(parent).context("could not create extension staging parent")?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination_file)
            .with_context(|| {
                format!("could not create staged ZIP member {}", relative.display())
            })?;
        std::io::copy(&mut entry, &mut output).context("could not extract ZIP member")?;
        output
            .sync_all()
            .context("could not flush staged ZIP member")?;
    }
    Ok(())
}

fn inspect_zip(bytes: &[u8], expected_uuid: &str, gnome_version: Option<u32>) -> Result<()> {
    if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
        bail!("artifact exceeds bounded size");
    }
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).context("artifact is not a ZIP archive")?;
    if archive.is_empty() || archive.len() > MAX_ZIP_ENTRIES {
        bail!("artifact has an invalid number of ZIP members");
    }
    let mut names = HashSet::new();
    let mut total = 0u64;
    let mut metadata_bytes = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("could not inspect ZIP member")?;
        let name = entry.name().to_string();
        let relative = safe_zip_path(&name)?;
        if !names.insert(relative.clone()) {
            bail!("artifact contains duplicate ZIP member");
        }
        if entry.encrypted() || entry.is_symlink() {
            bail!("artifact contains an encrypted or symbolic-link ZIP member");
        }
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                bail!("artifact contains a ZIP symlink");
            }
            if mode & 0o170000 != 0 && mode & 0o170000 != 0o100000 && !entry.is_dir() {
                bail!("artifact contains a non-regular ZIP member");
            }
        }
        if entry.is_dir() {
            continue;
        }
        let member_size = entry.size();
        if member_size > MAX_ZIP_MEMBER_BYTES {
            bail!("artifact ZIP member exceeds bounded size");
        }
        total = total
            .checked_add(member_size)
            .ok_or_else(|| anyhow!("artifact ZIP size overflow"))?;
        if total > MAX_ZIP_UNCOMPRESSED_BYTES {
            bail!("artifact ZIP exceeds bounded uncompressed size");
        }
        let mut content = Vec::with_capacity(member_size.min(MAX_METADATA_BYTES) as usize);
        let mut limited = (&mut entry).take(member_size.saturating_add(1));
        limited
            .read_to_end(&mut content)
            .context("could not read ZIP member")?;
        if content.len() as u64 != member_size {
            bail!("artifact ZIP member size changed while reading");
        }
        if relative == Path::new("metadata.json") {
            if content.len() as u64 > MAX_METADATA_BYTES {
                bail!("extension metadata exceeds bounded size");
            }
            metadata_bytes = Some(content);
        }
    }
    let metadata_bytes =
        metadata_bytes.ok_or_else(|| anyhow!("extension metadata.json is missing"))?;
    let metadata: Value =
        serde_json::from_slice(&metadata_bytes).context("extension metadata.json is invalid")?;
    let metadata = metadata
        .as_object()
        .ok_or_else(|| anyhow!("extension metadata must be an object"))?;
    let uuid = metadata
        .get("uuid")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("extension metadata UUID is missing"))?;
    if uuid != expected_uuid {
        bail!("extension metadata UUID does not match source");
    }
    let version = metadata
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("extension metadata version is missing"))?;
    if version == 0 {
        bail!("extension metadata version must be positive");
    }
    let shells = metadata
        .get("shell-version")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("extension metadata shell-version is missing"))?;
    let shells = shells
        .iter()
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| anyhow!("extension shell-version must be text"))
        })
        .collect::<Result<Vec<_>>>()?;
    if shells.is_empty() {
        bail!("extension metadata shell-version is empty");
    }
    if let Some(gnome) = gnome_version {
        let expected = gnome.to_string();
        if !shells.iter().any(|item| *item == expected) {
            bail!("extension does not support the local GNOME Shell version");
        }
    }
    Ok(())
}

fn safe_zip_path(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('\\') || name.contains('\0') {
        bail!("artifact contains an unsafe ZIP member path");
    }
    let path = Path::new(name);
    if path.is_absolute() {
        bail!("artifact contains an absolute ZIP member path");
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!("artifact contains a traversal ZIP member path")
            }
        }
    }
    if safe.as_os_str().is_empty() {
        bail!("artifact contains an empty ZIP member path");
    }
    Ok(safe)
}

fn verify_artifact_bytes(artifact: &crate::models::Artifact, bytes: &[u8]) -> Result<()> {
    if artifact.size > MAX_ARTIFACT_BYTES {
        bail!("artifact size exceeds bounded download limit");
    }
    if bytes.len() as u64 != artifact.size {
        bail!("downloaded artifact size does not match manifest");
    }
    if sha256_hex(bytes) != artifact.sha256 {
        bail!("downloaded artifact digest does not match manifest");
    }
    Ok(())
}

fn persist_artifact(root: &Path, digest: &str, bytes: &[u8]) -> Result<PathBuf> {
    let artifacts = root.join("artifacts");
    fs::create_dir_all(&artifacts).context("could not create retained artifact directory")?;
    ensure_real_directory(&artifacts)?;
    let path = artifacts.join(format!("{digest}.zip"));
    if path_present(&path)? {
        if path.is_symlink() || !path.is_file() {
            bail!("retained artifact path is not a regular file");
        }
        let existing = read_bounded_file(&path, MAX_ARTIFACT_BYTES)?;
        if sha256_hex(&existing) != digest {
            bail!("retained artifact path contains a different digest");
        }
        return Ok(path);
    }
    let temporary = artifacts.join(format!(".{digest}.{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .context("could not create retained artifact staging file")?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("could not write retained artifact");
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("could not activate retained artifact");
    }
    Ok(path)
}

fn read_bounded_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    if path.is_symlink() || !path.is_file() {
        bail!("retained artifact is not a regular file");
    }
    let mut file = File::open(path).context("could not open retained artifact")?;
    let mut bytes = Vec::new();
    let mut limited = (&mut file).take(max_bytes.saturating_add(1));
    limited
        .read_to_end(&mut bytes)
        .context("could not read retained artifact")?;
    if bytes.len() as u64 > max_bytes {
        bail!("retained artifact exceeds bounded size");
    }
    Ok(bytes)
}

fn extension_directory(host: &HostInfo, extension_uuid: &str) -> Result<PathBuf> {
    if host.os != "linux" && !host.os.eq_ignore_ascii_case("linux") {
        bail!("GNOME extension installation requires Linux");
    }
    if host.extensions_directory.trim().is_empty() {
        bail!("host extension directory is empty");
    }
    let extensions = PathBuf::from(&host.extensions_directory);
    if !extensions.is_absolute() {
        bail!("host extension directory must be absolute");
    }
    if extensions
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        bail!("host extension directory contains path traversal components");
    }
    ensure_no_symlink_ancestors(&extensions)?;
    fs::create_dir_all(&extensions).context("could not create GNOME extension directory")?;
    ensure_no_symlink_ancestors(&extensions)?;
    ensure_real_directory(&extensions)?;
    if extension_uuid.is_empty()
        || extension_uuid == "."
        || extension_uuid == ".."
        || extension_uuid.contains('/')
        || extension_uuid.contains('\\')
        || extension_uuid.contains('\0')
    {
        bail!("extension UUID is unsafe");
    }
    Ok(extensions.join(extension_uuid))
}

fn acquire_operation_lock(store: &Store) -> Result<File> {
    let path = store.root().join("install.lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("installation lock must not be a symbolic link")
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("could not inspect installation lock"),
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .context("could not open installation lock")?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(error) if error.kind() == ErrorKind::WouldBlock => {
            bail!("another install or rollback operation is already in progress")
        }
        Err(error) => Err(error).context("could not lock installation operation"),
    }
}

fn ensure_no_symlink_ancestors(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("host extension path contains a symbolic-link ancestor")
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => break,
            Err(error) => return Err(error).context("could not inspect host extension path"),
        }
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    if path.is_symlink() || !path.is_dir() {
        bail!("path is not a real directory: {}", path.display());
    }
    Ok(())
}

fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("could not inspect path {}", path.display()))
        }
    }
}

fn ensure_managed_directory(path: &Path, app_id: &str, extension_uuid: &str) -> Result<()> {
    ensure_real_directory(path)?;
    let marker = read_marker(path)
        .map_err(|_| anyhow!("existing extension is unmanaged or has an invalid marker"))?;
    if marker.app_id != app_id
        || marker.adapter != "gnome-extension"
        || marker.extension_uuid.as_deref() != Some(extension_uuid)
    {
        bail!("existing extension belongs to a different app manager installation");
    }
    Ok(())
}

fn marker_for(installed: &InstalledApp) -> Result<InstallMarker> {
    if installed.adapter != "gnome-extension" {
        bail!("unsupported installed adapter");
    }
    Ok(InstallMarker {
        app_id: installed.app_id.clone(),
        version: installed.version.clone(),
        adapter: installed.adapter.clone(),
        extension_uuid: installed.extension_uuid.clone(),
        artifact_sha256: installed.artifact_sha256.clone(),
        retained_artifact: installed.retained_artifact.clone(),
    })
}

fn single_previous(mut installed: InstalledApp) -> InstalledApp {
    installed.previous = None;
    installed
}

fn read_marker(path: &Path) -> Result<InstallMarker> {
    let marker = path.join(MARKER_NAME);
    if marker.is_symlink() || !marker.is_file() {
        bail!("installation marker is missing");
    }
    let bytes = read_bounded_file(&marker, 64 * 1024)?;
    let marker: InstallMarker =
        serde_json::from_slice(&bytes).context("installation marker is invalid")?;
    if marker.app_id.is_empty()
        || marker.version.is_empty()
        || marker.adapter != "gnome-extension"
        || marker.artifact_sha256.len() != 64
    {
        bail!("installation marker is incomplete");
    }
    Ok(marker)
}

fn write_marker(path: &Path, marker: &InstallMarker) -> Result<()> {
    let temporary = path.join(format!(".{MARKER_NAME}.{}", Uuid::new_v4()));
    let bytes = serde_json::to_vec(marker).context("could not encode installation marker")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .context("could not create installation marker")?;
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(error).context("could not write installation marker");
    }
    fs::rename(&temporary, path.join(MARKER_NAME))
        .context("could not activate installation marker")?;
    Ok(())
}

fn copy_user_settings(existing: &Path, destination: &Path) -> Result<()> {
    if !existing.exists() {
        return Ok(());
    }
    ensure_real_directory(existing)?;
    for entry in fs::read_dir(existing).context("could not inspect existing extension")? {
        let entry = entry.context("could not inspect existing extension entry")?;
        let name = entry.file_name();
        let name_string = name.to_string_lossy().to_ascii_lowercase();
        if name_string != "settings"
            && name_string != "settings.json"
            && !name_string.starts_with("settings.")
            && !name_string.starts_with(".settings")
        {
            continue;
        }
        let source = entry.path();
        let target = destination.join(&name);
        copy_tree(&source, &target)?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).context("could not inspect settings entry")?;
    if metadata.file_type().is_symlink() {
        bail!("settings entry is a symlink");
    }
    if metadata.is_dir() {
        fs::create_dir_all(destination).context("could not create settings directory")?;
        for entry in fs::read_dir(source).context("could not read settings directory")? {
            let entry = entry.context("could not inspect settings directory entry")?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).context("could not create settings parent")?;
        }
        fs::copy(source, destination).context("could not preserve extension settings")?;
    } else {
        bail!("settings entry is not a regular file or directory");
    }
    Ok(())
}

fn resolve_retained_artifact(root: &Path, saved: &str) -> Result<PathBuf> {
    let artifacts = root.join("artifacts");
    let candidate = PathBuf::from(saved);
    let path = if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    };
    if !path.starts_with(&artifacts)
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        bail!("retained artifact path escapes the app manager artifact directory");
    }
    Ok(path)
}

fn remove_owned_directory(path: &Path) -> Result<()> {
    if path.is_symlink() {
        bail!("owned staging path is a symlink");
    }
    if path.is_dir() {
        fs::remove_dir_all(path).context("could not remove owned staging directory")?;
    } else if path.exists() {
        fs::remove_file(path).context("could not remove owned staging path")?;
    }
    Ok(())
}

fn ensure_owned_internal_path(path: &Path, parent: &Path, prefix: &str) -> Result<()> {
    if path.parent() != Some(parent) {
        bail!("internal operation path has an unexpected parent");
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("internal operation path has an invalid name"))?;
    if !name.starts_with(prefix) {
        bail!("internal operation path has an unexpected name");
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut bytes);
            for (name, contents) in entries {
                writer
                    .start_file(*name, SimpleFileOptions::default())
                    .expect("ZIP entry");
                writer.write_all(contents).expect("ZIP contents");
            }
            writer.finish().expect("ZIP finish");
        }
        bytes.into_inner()
    }

    fn metadata(uuid: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "uuid": uuid,
            "version": 1,
            "shell-version": ["49", "50"]
        }))
        .expect("metadata")
    }

    #[test]
    fn zip_traversal_is_rejected() {
        let bytes = archive(&[
            ("metadata.json", &metadata("demo@example.test")),
            ("../escape", b"x"),
        ]);
        assert!(inspect_zip(&bytes, "demo@example.test", Some(50)).is_err());
    }

    #[test]
    fn wrong_uuid_is_rejected() {
        let bytes = archive(&[("metadata.json", &metadata("other@example.test"))]);
        assert!(inspect_zip(&bytes, "demo@example.test", Some(50)).is_err());
    }

    #[test]
    fn populated_settings_are_copied() {
        let root = tempfile::tempdir().expect("tempdir");
        let existing = root.path().join("existing");
        let destination = root.path().join("destination");
        fs::create_dir_all(&existing).expect("existing");
        fs::create_dir_all(&destination).expect("destination");
        fs::write(existing.join("settings.json"), b"{\"favorite\":true}").expect("settings");
        copy_user_settings(&existing, &destination).expect("copy settings");
        assert_eq!(
            fs::read(destination.join("settings.json")).expect("read settings"),
            b"{\"favorite\":true}"
        );
    }

    #[test]
    fn marker_round_trip_identifies_managed_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let installed = InstalledApp {
            app_id: "demo".into(),
            version: "1.0.0".into(),
            adapter: "gnome-extension".into(),
            extension_uuid: Some("demo@example.test".into()),
            artifact_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            retained_artifact: root
                .path()
                .join("artifacts/a.zip")
                .to_string_lossy()
                .into_owned(),
            installed_at: "2026-10-01T00:00:00Z".into(),
            previous: None,
        };
        write_marker(root.path(), &marker_for(&installed).expect("marker")).expect("write marker");
        ensure_managed_directory(root.path(), "demo", "demo@example.test").expect("managed");
    }

    #[test]
    fn digest_and_size_mismatches_are_rejected_before_install() {
        let artifact = crate::models::Artifact {
            target: "gnome-extension".into(),
            name: "demo.shell-extension.zip".into(),
            url: "https://example.test/demo.zip".into(),
            sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            size: 3,
        };
        assert!(verify_artifact_bytes(&artifact, b"no").is_err());
        assert!(verify_artifact_bytes(&artifact, b"bad").is_err());
    }

    #[test]
    fn rollback_history_keeps_one_explicit_previous_entry() {
        let oldest = InstalledApp {
            app_id: "demo".into(),
            version: "1.0.0".into(),
            adapter: "gnome-extension".into(),
            extension_uuid: Some("demo@example.test".into()),
            artifact_sha256: "a".repeat(64),
            retained_artifact: "artifacts/a.zip".into(),
            installed_at: "2026-10-01T00:00:00Z".into(),
            previous: None,
        };
        let mut current = oldest.clone();
        current.version = "2.0.0".into();
        current.previous = Some(Box::new(oldest));
        let bounded = single_previous(current);
        assert!(bounded.previous.is_none());
    }

    #[test]
    fn settings_survive_both_update_and_rollback_staging() {
        let root = tempfile::tempdir().expect("tempdir");
        let update_old = root.path().join("update-old");
        let update_stage = root.path().join("update-stage");
        let rollback_old = root.path().join("rollback-old");
        let rollback_stage = root.path().join("rollback-stage");
        for path in [&update_old, &update_stage, &rollback_old, &rollback_stage] {
            fs::create_dir_all(path).expect("directory");
        }
        fs::write(update_old.join("settings.json"), b"{\"favorite\":true}")
            .expect("update settings");
        copy_user_settings(&update_old, &update_stage).expect("update preservation");
        assert_eq!(
            fs::read(update_stage.join("settings.json")).expect("updated settings"),
            b"{\"favorite\":true}"
        );

        fs::write(rollback_old.join("settings.json"), b"{\"favorite\":false}")
            .expect("rollback settings");
        copy_user_settings(&rollback_old, &rollback_stage).expect("rollback preservation");
        assert_eq!(
            fs::read(rollback_stage.join("settings.json")).expect("rolled-back settings"),
            b"{\"favorite\":false}"
        );
    }

    #[test]
    fn interrupted_staging_receipt_is_reconciled_as_failed_without_activation() {
        let root = tempfile::tempdir().expect("store root");
        let store = Store::open(root.path()).expect("store");
        let data = root.path().join("data");
        let host = HostInfo {
            os: "linux".into(),
            architecture: "x86_64".into(),
            gnome_version: Some(50),
            extensions_directory: data
                .join("gnome-shell/extensions")
                .to_string_lossy()
                .into_owned(),
            data_directory: data.to_string_lossy().into_owned(),
        };
        let target = extension_directory(&host, "demo@example.test").expect("target");
        let stage = target
            .parent()
            .expect("parent")
            .join(format!(".app-manager-stage-{}", Uuid::new_v4()));
        fs::create_dir_all(&stage).expect("stage");
        let installed = InstalledApp {
            app_id: "demo".into(),
            version: "1.0.0".into(),
            adapter: "gnome-extension".into(),
            extension_uuid: Some("demo@example.test".into()),
            artifact_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            retained_artifact: root
                .path()
                .join("artifacts/a.zip")
                .to_string_lossy()
                .into_owned(),
            installed_at: "2026-10-01T00:00:00Z".into(),
            previous: None,
        };
        let operation =
            begin_operation(&store, &installed, "install", &target, &stage).expect("receipt");
        assert!(reconcile_pending(&store, &host).is_err());
        assert!(path_present(&stage).expect("stage state"));
        let state = store.snapshot().expect("state");
        let saved = state
            .operations
            .iter()
            .find(|item| item.id == operation.id)
            .expect("operation");
        assert_eq!(saved.status, "started");
        assert!(state.installed.is_empty());
    }

    #[test]
    fn matching_active_marker_reconciles_as_completed() {
        let root = tempfile::tempdir().expect("store root");
        let store = Store::open(root.path()).expect("store");
        let extensions = root.path().join("extensions");
        let host = HostInfo {
            os: "linux".into(),
            architecture: "x86_64".into(),
            gnome_version: Some(50),
            extensions_directory: extensions.to_string_lossy().into_owned(),
            data_directory: root.path().to_string_lossy().into_owned(),
        };
        let target = extension_directory(&host, "demo@example.test").expect("target");
        fs::create_dir_all(&target).expect("target directory");
        let installed = InstalledApp {
            app_id: "demo".into(),
            version: "1.0.0".into(),
            adapter: "gnome-extension".into(),
            extension_uuid: Some("demo@example.test".into()),
            artifact_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            retained_artifact: root
                .path()
                .join("artifacts/a.zip")
                .to_string_lossy()
                .into_owned(),
            installed_at: "2026-10-01T00:00:00Z".into(),
            previous: None,
        };
        write_marker(&target, &marker_for(&installed).expect("marker")).expect("marker");
        let stage = extensions.join(format!(".app-manager-stage-{}", Uuid::new_v4()));
        let operation =
            begin_operation(&store, &installed, "install", &target, &stage).expect("receipt");
        reconcile_pending(&store, &host).expect("reconcile");
        let state = store.snapshot().expect("state");
        let saved = state
            .operations
            .iter()
            .find(|item| item.id == operation.id)
            .expect("operation");
        assert_eq!(saved.status, "completed");
        assert_eq!(state.installed.len(), 1);
        assert_eq!(state.installed[0].app_id, installed.app_id);
        assert_eq!(state.installed[0].version, installed.version);
        assert_eq!(
            state.installed[0].artifact_sha256,
            installed.artifact_sha256
        );
    }
}
