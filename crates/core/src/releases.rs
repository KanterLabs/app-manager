//! Release manifest consumption for the desktop app manager.
//!
//! The release-tools publisher is intentionally treated as an untrusted HTTP
//! endpoint.  This module validates the producer's schema before exposing a
//! release to the installer and requires the matching, public verification
//! receipt as the proof that every asset was checked after publication.

use crate::models::{AppRelease, AppSource, Artifact, HostInfo};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;
use url::{Host, Url};

const SCHEMA: u64 = 1;
const SHA1_LEN: usize = 40;
const SHA256_LEN: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RECEIPT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;
const GITHUB_RELEASE_HOST: &str = "github.com";
const GITHUB_PUBLIC_CDN_HOSTS: &[&str] = &[
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];
const GITHUB_CDN_QUERY_KEYS: &[&str] = &[
    "sp",
    "sv",
    "sr",
    "spr",
    "se",
    "rscd",
    "rsct",
    "skoid",
    "sktid",
    "skt",
    "ske",
    "sks",
    "skv",
    "sig",
    "jwt",
    "response-content-disposition",
    "response-content-type",
];
const SIGNED_QUERY_KEYS: &[&str] = &[
    "sp",
    "sv",
    "sr",
    "spr",
    "se",
    "rscd",
    "rsct",
    "skoid",
    "sktid",
    "skt",
    "ske",
    "sks",
    "skv",
    "sig",
    "jwt",
    "x-amz-signature",
    "response-content-disposition",
    "response-content-type",
];

/// Validate a saved source before it is used for a network request.
pub fn validate_source(source: &AppSource) -> Result<()> {
    validate_component(&source.id, "source id")?;
    validate_component(&source.name, "source name")?;
    if source.channel.trim().is_empty() || has_control(&source.channel) {
        bail!("source channel must be non-empty text");
    }
    if source.adapter != "gnome-extension" {
        bail!("unsupported release adapter: {}", source.adapter);
    }
    let uuid = source
        .extension_uuid
        .as_deref()
        .ok_or_else(|| anyhow!("GNOME extension source is missing extension UUID"))?;
    validate_extension_uuid(uuid)?;
    if source.gnome_versions.is_empty() {
        bail!("GNOME extension source has no supported GNOME versions");
    }
    let mut versions = HashSet::new();
    for version in &source.gnome_versions {
        if *version == 0 || *version > 200 || !versions.insert(*version) {
            bail!("source GNOME versions must be unique positive values");
        }
    }
    validate_remote_url(&source.manifest_url, "manifest URL")?;
    validate_remote_url(&source.receipt_url, "receipt URL")?;
    validate_remote_url(&source.artifact_base_url, "artifact base URL")?;
    Ok(())
}

/// Fetch and validate a schema-1 release manifest and its matching published
/// receipt.  A manifest without a complete receipt is never returned as a
/// verified release.
pub async fn refresh(source: &AppSource) -> Result<AppRelease> {
    validate_source(source)?;
    let manifest_url = validate_remote_url(&source.manifest_url, "manifest URL")?;
    let receipt_url = validate_remote_url(&source.receipt_url, "receipt URL")?;
    let artifact_base = validate_artifact_base(&source.artifact_base_url)?;

    let manifest_bytes = download_bounded(&manifest_url, MAX_MANIFEST_BYTES).await?;
    let manifest_hash = sha256_hex(&manifest_bytes);
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).context("release manifest is not valid JSON")?;
    let normalized = parse_manifest(&manifest)?;
    if normalized.project != source.id {
        bail!("release manifest project does not match source identity");
    }

    let receipt_bytes = download_bounded(&receipt_url, MAX_RECEIPT_BYTES).await?;
    let receipt: Value = serde_json::from_slice(&receipt_bytes)
        .context("release verification receipt is not valid JSON")?;
    validate_receipt(
        &receipt,
        &normalized,
        &manifest_hash,
        manifest_bytes.len() as u64,
    )?;

    let mut artifacts = Vec::with_capacity(normalized.targets.len());
    for target in normalized.targets {
        let url = artifact_url(&artifact_base, &target.name)?;
        artifacts.push(Artifact {
            target: target.target,
            name: target.name,
            url: url.to_string(),
            sha256: target.sha256,
            size: target.size,
        });
    }

    Ok(AppRelease {
        app_id: source.id.clone(),
        name: source.name.clone(),
        version: normalized.version,
        tag: normalized.tag,
        commit: normalized.commit,
        repository: normalized.repository,
        channel: source.channel.clone(),
        verified: true,
        artifacts,
        fetched_at: Utc::now().to_rfc3339(),
    })
}

/// Select the GNOME extension package compatible with this host.
pub fn select_artifact(
    release: &AppRelease,
    source: &AppSource,
    host: &HostInfo,
) -> Result<Artifact> {
    validate_source(source)?;
    if !release.verified {
        bail!("release has not been verified by a published receipt");
    }
    validate_component(&release.app_id, "release app id")?;
    validate_version(&release.version)?;
    validate_tag(&release.tag)?;
    validate_sha1(&release.commit, "release commit")?;
    validate_repository(&release.repository)?;
    if release.name.trim().is_empty() || has_control(&release.name) {
        bail!("release name must be non-empty text");
    }
    if release.app_id != source.id {
        bail!("release app identity does not match source");
    }
    if release.channel.is_empty() || has_control(&release.channel) {
        bail!("release channel must be non-empty text");
    }
    if release.channel != source.channel {
        bail!("release channel does not match source");
    }
    if !host.os.eq_ignore_ascii_case("linux") {
        bail!("GNOME extension installation requires a Linux host");
    }
    normalize_architecture(&host.architecture)?;
    let gnome = host
        .gnome_version
        .ok_or_else(|| anyhow!("GNOME Shell version is unavailable"))?;
    if !source.gnome_versions.contains(&gnome) {
        bail!("GNOME Shell version is not supported by this source");
    }

    let mut candidates = release
        .artifacts
        .iter()
        .filter(|artifact| artifact_target_matches(artifact, source, &host.architecture))
        .cloned()
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        bail!("release does not contain exactly one GNOME extension artifact");
    }
    let artifact = candidates.remove(0);
    validate_artifact(&artifact)?;
    if !artifact.name.to_ascii_lowercase().ends_with(".zip") {
        bail!("GNOME extension artifact is not a ZIP archive");
    }
    Ok(artifact)
}

/// Validate a URL and return its parsed form.  HTTP is reserved for local
/// loopback fixtures; production sources must use HTTPS.
pub(crate) fn validate_remote_url(raw: &str, label: &str) -> Result<Url> {
    let url = parse_remote_url(raw, label)?;
    if has_credential_query(&url) || has_signed_query(&url) {
        bail!("{label} contains credential-like query data");
    }
    Ok(url)
}

fn validate_redirect_url(raw: &str, label: &str, allow_github_cdn_query: bool) -> Result<Url> {
    let url = parse_remote_url(raw, label)?;
    if has_credential_query(&url)
        || (!allow_github_cdn_query && has_signed_query(&url))
        || (allow_github_cdn_query && !github_cdn_query_is_allowed(&url))
    {
        bail!("{label} contains credential-like query data");
    }
    Ok(url)
}

fn parse_remote_url(raw: &str, label: &str) -> Result<Url> {
    let url = Url::parse(raw).with_context(|| format!("{label} is not a valid URL"))?;
    if url.username() != "" || url.password().is_some() {
        bail!("{label} contains credentials");
    }
    if url.fragment().is_some() {
        bail!("{label} must not contain a fragment");
    }
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&url) => {}
        "http" => bail!("{label} must use HTTPS outside loopback fixtures"),
        _ => bail!("{label} uses an unsupported URL scheme"),
    }
    if url.host().is_none() {
        bail!("{label} is missing a host");
    }
    Ok(url)
}

/// Download a bounded response while refusing untrusted cross-origin
/// redirects. GitHub's public release download CDN is the only explicit
/// exception. The helper is shared with the installer so artifact downloads
/// have the same URL and redirect policy as manifests and receipts.
pub(crate) async fn download_bounded(url: &Url, max_bytes: u64) -> Result<Vec<u8>> {
    let url = validate_remote_url(url.as_str(), "release URL")?;
    let client = Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .context("could not create release HTTP client")?;
    let initial_origin = origin(&url)?;
    let mut current = url.clone();
    for _attempt in 0..=MAX_REDIRECTS {
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|error| anyhow!("release download failed: {}", error.without_url()))?;
        if is_redirect(response.status()) {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| anyhow!("release redirect has no location"))?
                .to_str()
                .context("release redirect location is invalid")?;
            let next = current
                .join(location)
                .context("release redirect location is invalid")?;
            let allow_github_cdn_query = is_allowed_github_cdn_redirect(&url, &next);
            validate_redirect_url(
                next.as_str(),
                "release redirect URL",
                allow_github_cdn_query,
            )?;
            if origin(&next)? != origin(&current)?
                && !(origin(&current)? == initial_origin && allow_github_cdn_query)
            {
                bail!("release redirect changed origin");
            }
            current = next;
            continue;
        }
        if response.status() != StatusCode::OK {
            bail!("release endpoint returned HTTP {}", response.status());
        }
        if let Some(length) = response.content_length() {
            if length > max_bytes {
                bail!("release response exceeds bounded size");
            }
        }
        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| anyhow!("release response stream failed: {}", error.without_url()))?
        {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or_else(|| anyhow!("release response size overflow"))?;
            if next_len as u64 > max_bytes {
                bail!("release response exceeds bounded size");
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(bytes);
    }
    bail!("release endpoint redirected too many times")
}

#[derive(Debug)]
struct ManifestTarget {
    target: String,
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Debug)]
struct ManifestData {
    project: String,
    repository: String,
    version: String,
    tag: String,
    commit: String,
    targets: Vec<ManifestTarget>,
}

fn parse_manifest(value: &Value) -> Result<ManifestData> {
    let root = object(value, "manifest")?;
    ensure_keys(
        root,
        &[
            "schema",
            "project",
            "repository",
            "release",
            "protected_assets",
            "targets",
        ],
        "manifest",
    )?;
    expect_u64(root.get("schema"), SCHEMA, "manifest schema")?;
    let project = safe_component(
        expect_string(root.get("project"), "manifest project")?,
        "project",
    )?;
    let repository = validate_repository(expect_string(
        root.get("repository"),
        "manifest repository",
    )?)?;
    let release = object(
        root.get("release")
            .ok_or_else(|| anyhow!("manifest release is missing"))?,
        "manifest release",
    )?;
    ensure_keys(release, &["version", "tag", "commit"], "manifest release")?;
    let version = validate_version(expect_string(release.get("version"), "release version")?)?;
    let tag = validate_tag(expect_string(release.get("tag"), "release tag")?)?;
    let commit = validate_sha1(
        expect_string(release.get("commit"), "release commit")?,
        "release commit",
    )?;

    let protected = string_list(
        root.get("protected_assets")
            .ok_or_else(|| anyhow!("manifest protected_assets is missing"))?,
        "protected_assets",
    )?;
    let protected_set = protected.iter().cloned().collect::<HashSet<_>>();
    if protected_set.len() != protected.len() {
        bail!("manifest protected_assets contains duplicates");
    }
    for name in &protected {
        validate_asset_name(name, "protected asset")?;
    }

    let targets_value = root
        .get("targets")
        .ok_or_else(|| anyhow!("manifest targets is missing"))?;
    let targets_array = targets_value
        .as_array()
        .ok_or_else(|| anyhow!("manifest targets must be an array"))?;
    if targets_array.is_empty() {
        bail!("manifest targets must not be empty");
    }
    let mut targets = Vec::with_capacity(targets_array.len());
    let mut target_keys = HashSet::new();
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    for (index, raw) in targets_array.iter().enumerate() {
        let target = object(raw, &format!("manifest target {index}"))?;
        ensure_keys(
            target,
            &["target", "path", "name", "size", "sha256", "protected"],
            "manifest target",
        )?;
        let target_name = safe_component(
            expect_string(target.get("target"), "target key")?,
            "target key",
        )?;
        let path = validate_relative_path(expect_string(target.get("path"), "target path")?)?;
        let name = validate_asset_name(
            expect_string(target.get("name"), "target name")?,
            "target name",
        )?;
        let size = expect_u64_value(target.get("size"), "target size")?;
        if size > MAX_ARTIFACT_BYTES {
            bail!("manifest target exceeds bounded artifact size");
        }
        let sha256 = validate_sha256(
            expect_string(target.get("sha256"), "target SHA-256")?,
            "target SHA-256",
        )?;
        let is_protected = target
            .get("protected")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("target protected must be boolean"))?;
        if is_protected != protected_set.contains(&name) {
            bail!("target protected flag disagrees with protected_assets");
        }
        if !target_keys.insert(target_name.clone())
            || !names.insert(name.clone())
            || !paths.insert(path)
        {
            bail!("manifest target identities must be unique");
        }
        targets.push(ManifestTarget {
            target: target_name,
            name,
            size,
            sha256,
        });
    }
    if protected_set.iter().any(|name| !names.contains(name)) {
        bail!("manifest protected asset has no target");
    }
    Ok(ManifestData {
        project,
        repository,
        version,
        tag,
        commit,
        targets,
    })
}

fn validate_receipt(
    value: &Value,
    manifest: &ManifestData,
    manifest_hash: &str,
    manifest_size: u64,
) -> Result<()> {
    let root = object(value, "release receipt")?;
    expect_bool(root.get("ok"), true, "receipt ok")?;
    expect_string_eq(root.get("command"), "ensure-release", "receipt command")?;
    expect_bool(root.get("dry_run"), false, "receipt dry_run")?;
    expect_bool(root.get("public"), true, "receipt public")?;
    expect_bool(root.get("draft"), false, "receipt draft")?;
    if let Some(finalized) = root.get("finalized") {
        if !finalized.is_boolean() {
            bail!("receipt finalized must be boolean when present");
        }
    }
    expect_string_eq(
        root.get("repository"),
        &manifest.repository,
        "receipt repository",
    )?;
    expect_string_eq(root.get("tag"), &manifest.tag, "receipt tag")?;
    expect_string_eq(
        root.get("source_commit"),
        &manifest.commit,
        "receipt source_commit",
    )?;
    expect_string_eq(
        root.get("manifest_sha256"),
        manifest_hash,
        "receipt manifest_sha256",
    )?;
    let manifest_asset = object(
        root.get("manifest_asset")
            .ok_or_else(|| anyhow!("receipt manifest_asset is missing"))?,
        "receipt manifest_asset",
    )?;
    expect_string_eq(
        manifest_asset.get("name"),
        "release-manifest.json",
        "receipt manifest_asset name",
    )?;
    expect_bool(
        manifest_asset.get("verified"),
        true,
        "receipt manifest_asset verified",
    )?;
    expect_bool(
        manifest_asset.get("public"),
        true,
        "receipt manifest_asset public",
    )?;
    let manifest_verification = expect_string(
        manifest_asset.get("verification"),
        "receipt manifest_asset verification",
    )?;
    if manifest_verification != "digest" && manifest_verification != "download" {
        bail!("receipt manifest_asset verification method is unsupported");
    }
    let manifest_asset_hash = validate_sha256(
        expect_string(
            manifest_asset.get("sha256"),
            "receipt manifest_asset SHA-256",
        )?,
        "receipt manifest_asset SHA-256",
    )?;
    if manifest_asset_hash != manifest_hash {
        bail!("receipt manifest_asset hash does not match manifest");
    }
    if expect_u64_value(manifest_asset.get("size"), "receipt manifest_asset size")? != manifest_size
    {
        bail!("receipt manifest_asset size does not match manifest");
    }

    let assets = root
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("receipt assets must be an array"))?;
    if assets.len() != manifest.targets.len() {
        bail!("receipt does not contain every manifest target");
    }
    let mut by_name = std::collections::HashMap::new();
    for asset in assets {
        let item = object(asset, "receipt asset")?;
        let name = expect_string(item.get("name"), "receipt asset name")?;
        if by_name.insert(name.to_string(), item).is_some() {
            bail!("receipt assets contain duplicate names");
        }
        expect_bool(item.get("verified"), true, "receipt asset verified")?;
        expect_bool(item.get("public"), true, "receipt asset public")?;
        let verification = expect_string(item.get("verification"), "receipt asset verification")?;
        if verification != "digest" && verification != "download" {
            bail!("receipt asset verification method is unsupported");
        }
    }
    for target in &manifest.targets {
        let item = by_name
            .get(&target.name)
            .ok_or_else(|| anyhow!("receipt is missing manifest target"))?;
        let hash = validate_sha256(
            expect_string(item.get("sha256"), "receipt asset SHA-256")?,
            "receipt asset SHA-256",
        )?;
        if hash != target.sha256 {
            bail!("receipt asset hash does not match manifest");
        }
        if expect_u64_value(item.get("size"), "receipt asset size")? != target.size {
            bail!("receipt asset size does not match manifest");
        }
    }
    Ok(())
}

fn validate_artifact(artifact: &Artifact) -> Result<()> {
    safe_component(&artifact.target, "artifact target")?;
    validate_asset_name(&artifact.name, "artifact name")?;
    validate_sha256(&artifact.sha256, "artifact SHA-256")?;
    if artifact.size > MAX_ARTIFACT_BYTES {
        bail!("artifact exceeds the bounded download size");
    }
    let url = validate_remote_url(&artifact.url, "artifact URL")?;
    if url.query().is_some() && has_credential_query(&url) {
        bail!("artifact URL contains credentials");
    }
    Ok(())
}

fn artifact_target_matches(artifact: &Artifact, source: &AppSource, architecture: &str) -> bool {
    if artifact.target == source.adapter {
        return true;
    }
    let canonical = match normalize_architecture(architecture) {
        Ok(value) => value,
        Err(_) => return false,
    };
    matches!(
        artifact.target.as_str(),
        "linux-amd64" | "linux-x86_64" | "linux-x64" if canonical == "amd64"
    ) || matches!(
        artifact.target.as_str(),
        "linux-arm64" | "linux-aarch64" if canonical == "arm64"
    )
}

fn validate_artifact_base(raw: &str) -> Result<Url> {
    let mut url = validate_remote_url(raw, "artifact base URL")?;
    let path = if url.path().ends_with('/') {
        url.path().to_string()
    } else {
        format!("{}/", url.path())
    };
    url.set_path(&path);
    Ok(url)
}

fn artifact_url(base: &Url, name: &str) -> Result<Url> {
    let url = base
        .join(name)
        .context("could not construct artifact URL")?;
    validate_remote_url(url.as_str(), "artifact URL")
}

fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| anyhow!("{label} must be an object"))
}

fn ensure_keys(value: &Map<String, Value>, allowed: &[&str], label: &str) -> Result<()> {
    if value.keys().any(|key| !allowed.contains(&key.as_str())) {
        bail!("{label} contains unknown fields");
    }
    Ok(())
}

fn expect_string<'a>(value: Option<&'a Value>, label: &str) -> Result<&'a str> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && !has_control(text))
        .ok_or_else(|| anyhow!("{label} must be non-empty text"))
}

fn expect_string_eq(value: Option<&Value>, expected: &str, label: &str) -> Result<()> {
    if expect_string(value, label)? != expected {
        bail!("{label} does not match expected value");
    }
    Ok(())
}

fn expect_bool(value: Option<&Value>, expected: bool, label: &str) -> Result<()> {
    if value.and_then(Value::as_bool) != Some(expected) {
        bail!("{label} does not have the required value");
    }
    Ok(())
}

fn expect_u64(value: Option<&Value>, expected: u64, label: &str) -> Result<()> {
    if expect_u64_value(value, label)? != expected {
        bail!("{label} does not have the required value");
    }
    Ok(())
}

fn expect_u64_value(value: Option<&Value>, label: &str) -> Result<u64> {
    value
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("{label} must be a non-negative integer"))
}

fn string_list(value: &Value, label: &str) -> Result<Vec<String>> {
    value
        .as_array()
        .ok_or_else(|| anyhow!("{label} must be an array"))?
        .iter()
        .map(|item| Ok(expect_string(Some(item), label)?.to_string()))
        .collect()
}

fn safe_component(value: &str, label: &str) -> Result<String> {
    if value == "." || value == ".." || value.chars().any(|char| char == '/' || char == '\\') {
        bail!("{label} contains a path separator");
    }
    Ok(value.to_string())
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty()
        || has_control(value)
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
    {
        bail!("{label} must be safe non-empty text");
    }
    Ok(())
}

fn validate_repository(value: &str) -> Result<String> {
    let mut parts = value.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        bail!("repository must be owner/name");
    }
    validate_component(owner, "repository owner")?;
    validate_component(name, "repository name")?;
    Ok(value.to_string())
}

fn validate_version(value: &str) -> Result<String> {
    validate_component(value, "version")?;
    if value.trim() != value
        || value.contains('{')
        || value.contains('}')
        || value.contains('/')
        || value.contains('\\')
    {
        bail!("version contains unsafe characters");
    }
    Ok(value.to_string())
}

fn validate_tag(value: &str) -> Result<String> {
    validate_component(value, "tag")?;
    if value.trim() != value
        || value.contains('{')
        || value.contains('}')
        || value.contains('/')
        || value.contains('\\')
    {
        bail!("tag contains unsafe characters");
    }
    Ok(value.to_string())
}

fn validate_relative_path(value: &str) -> Result<String> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value.contains('\0')
    {
        bail!("manifest target path is unsafe");
    }
    let mut parts = Vec::new();
    for part in value.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            bail!("manifest target path escapes its artifact directory");
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn validate_asset_name(value: &str, label: &str) -> Result<String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || has_control(value)
    {
        bail!("{label} must be a flat safe asset name");
    }
    Ok(value.to_string())
}

fn validate_sha1(value: &str, label: &str) -> Result<String> {
    if value.len() != SHA1_LEN
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{label} must be a lower-case 40-character SHA-1");
    }
    Ok(value.to_string())
}

fn validate_sha256(value: &str, label: &str) -> Result<String> {
    if value.len() != SHA256_LEN
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{label} must be a lower-case 64-character SHA-256");
    }
    Ok(value.to_string())
}

fn validate_extension_uuid(value: &str) -> Result<()> {
    if value.is_empty()
        || has_control(value)
        || value.len() > 200
        || value
            .chars()
            .any(|char| !(char.is_ascii_alphanumeric() || matches!(char, '.' | '_' | '-' | '@')))
        || value.matches('@').count() != 1
    {
        bail!("extension UUID is unsafe");
    }
    Ok(())
}

fn normalize_architecture(value: &str) -> Result<&'static str> {
    match value.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok("amd64"),
        "aarch64" | "arm64" => Ok("arm64"),
        other => bail!("unsupported host architecture: {other}"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

fn has_control(value: &str) -> bool {
    value.chars().any(|char| char.is_control())
}

fn has_credential_query(url: &Url) -> bool {
    url.query_pairs().any(|(key, _)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "token"
                | "access_token"
                | "authorization"
                | "password"
                | "passwd"
                | "secret"
                | "api_key"
                | "apikey"
        )
    })
}

fn has_signed_query(url: &Url) -> bool {
    url.query_pairs().any(|(key, _)| {
        SIGNED_QUERY_KEYS
            .iter()
            .any(|signed| key.eq_ignore_ascii_case(signed))
    })
}

fn github_cdn_query_is_allowed(url: &Url) -> bool {
    let mut has_query = false;
    for (key, _) in url.query_pairs() {
        has_query = true;
        if !GITHUB_CDN_QUERY_KEYS
            .iter()
            .any(|allowed| key.eq_ignore_ascii_case(allowed))
        {
            return false;
        }
    }
    has_query
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn origin(url: &Url) -> Result<(String, String, Option<u16>)> {
    let host = match url.host() {
        Some(Host::Domain(domain)) => domain.to_ascii_lowercase(),
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Ipv6(address)) => address.to_string(),
        None => bail!("URL has no host"),
    };
    Ok((
        url.scheme().to_ascii_lowercase(),
        host,
        url.port_or_known_default(),
    ))
}

fn is_allowed_github_cdn_redirect(initial: &Url, next: &Url) -> bool {
    if initial.scheme() != "https"
        || initial
            .host_str()
            .is_none_or(|host| !host.eq_ignore_ascii_case(GITHUB_RELEASE_HOST))
        || initial.port_or_known_default() != Some(443)
    {
        return false;
    }
    if next.scheme() != "https"
        || next.port_or_known_default() != Some(443)
        || next.username() != ""
        || next.password().is_some()
        || next.fragment().is_some()
        || has_credential_query(next)
        || !github_cdn_query_is_allowed(next)
    {
        return false;
    }
    next.host_str().is_some_and(|host| {
        GITHUB_PUBLIC_CDN_HOSTS
            .iter()
            .any(|allowed| host.eq_ignore_ascii_case(allowed))
    })
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppRelease, AppSource, Artifact, HostInfo};
    use axum::{response::Redirect, routing::get, Router};
    use serde_json::Value;
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::sync::oneshot;

    fn source() -> AppSource {
        AppSource {
            id: "demo".into(),
            name: "Demo".into(),
            manifest_url: "https://example.test/manifest.json".into(),
            artifact_base_url: "https://example.test/assets/".into(),
            receipt_url: "https://example.test/receipt.json".into(),
            channel: "stable".into(),
            adapter: "gnome-extension".into(),
            extension_uuid: Some("demo@example.test".into()),
            gnome_versions: vec![49, 50],
        }
    }

    #[test]
    fn source_rejects_credentials_and_unsupported_adapter() {
        let mut value = source();
        value.manifest_url = "https://user:secret@example.test/manifest.json".into();
        assert!(validate_source(&value).is_err());
        for key in [
            "sig",
            "jwt",
            "X-Amz-Signature",
            "sp",
            "response-content-type",
        ] {
            value = source();
            value.manifest_url = format!("https://example.test/manifest.json?{key}=opaque");
            assert!(
                validate_source(&value).is_err(),
                "configured {key} must be refused"
            );
        }
        let mut value = source();
        value.adapter = "binary".into();
        assert!(validate_source(&value).is_err());
    }

    #[test]
    fn github_cdn_redirect_allowlist_is_exact() {
        let initial = Url::parse("https://github.com/owner/repo/releases/download/v1/app.zip")
            .expect("initial URL");
        for host in GITHUB_PUBLIC_CDN_HOSTS {
            let next = Url::parse(&format!("https://{host}/asset?sig=opaque")).expect("CDN URL");
            assert!(is_allowed_github_cdn_redirect(&initial, &next));
        }
        for key in GITHUB_CDN_QUERY_KEYS {
            let next = Url::parse(&format!(
                "https://release-assets.githubusercontent.com/asset?{key}=opaque"
            ))
            .expect("CDN query URL");
            assert!(is_allowed_github_cdn_redirect(&initial, &next));
        }

        for candidate in [
            "https://api.github.com/asset",
            "https://release-assets.githubusercontent.com.evil.test/asset",
            "http://release-assets.githubusercontent.com/asset",
            "https://release-assets.githubusercontent.com:8443/asset",
            "https://user@release-assets.githubusercontent.com/asset",
            "https://release-assets.githubusercontent.com/asset#fragment",
            "https://release-assets.githubusercontent.com/asset?token=secret",
            "https://release-assets.githubusercontent.com/asset?X-Amz-Signature=opaque",
            "https://release-assets.githubusercontent.com/asset?unknown=opaque",
        ] {
            let next = Url::parse(candidate).expect("candidate URL");
            assert!(!is_allowed_github_cdn_redirect(&initial, &next));
        }
        assert!(!is_allowed_github_cdn_redirect(
            &Url::parse("https://github.com:8443/asset").expect("non-default port"),
            &Url::parse("https://objects.githubusercontent.com/asset").expect("CDN URL"),
        ));
        assert!(!is_allowed_github_cdn_redirect(
            &Url::parse("https://github.example/asset").expect("wrong initial host"),
            &Url::parse("https://objects.githubusercontent.com/asset").expect("CDN URL"),
        ));
        assert!(!is_allowed_github_cdn_redirect(
            &Url::parse("http://github.com/asset").expect("HTTP initial URL"),
            &Url::parse("https://objects.githubusercontent.com/asset").expect("CDN URL"),
        ));
        assert!(!is_allowed_github_cdn_redirect(
            &Url::parse("https://release-assets.githubusercontent.com/asset")
                .expect("CDN initial URL"),
            &Url::parse("https://objects.githubusercontent.com/asset").expect("CDN URL"),
        ));
        assert!(validate_redirect_url(
            "https://example.test/asset?sig=opaque",
            "same-origin redirect",
            false,
        )
        .is_err());
    }

    #[tokio::test]
    async fn loopback_redirects_are_same_origin_and_bounded() {
        let listener = tokio::net::TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .await
            .expect("fixture listener");
        let address = listener.local_addr().expect("fixture address");
        let same_origin_target = format!("http://127.0.0.1:{}/small", address.port());
        let changed_origin_target = format!("http://localhost:{}/small", address.port());
        let app = Router::new()
            .route("/small", get(|| async { "bounded response" }))
            .route(
                "/redirect",
                get(move || {
                    let target = same_origin_target.clone();
                    async move { Redirect::temporary(&target) }
                }),
            )
            .route(
                "/changed-origin",
                get(move || {
                    let target = changed_origin_target.clone();
                    async move { Redirect::temporary(&target) }
                }),
            )
            .route("/large", get(|| async { vec![b'x'; 16] }));
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_receiver.await;
                })
                .await
                .expect("fixture server");
        });

        let base = format!("http://127.0.0.1:{}", address.port());
        let redirected = download_bounded(
            &Url::parse(&format!("{base}/redirect")).expect("redirect URL"),
            1024,
        )
        .await
        .expect("same-origin redirect");
        assert_eq!(redirected, b"bounded response");

        let changed_origin = download_bounded(
            &Url::parse(&format!("{base}/changed-origin")).expect("changed-origin URL"),
            1024,
        )
        .await;
        assert!(changed_origin.is_err());

        let too_large =
            download_bounded(&Url::parse(&format!("{base}/large")).expect("large URL"), 4).await;
        assert!(too_large.is_err());

        let _ = shutdown.send(());
        server.await.expect("fixture shutdown");
    }

    #[test]
    fn artifact_selection_checks_runtime_and_target() {
        let source = source();
        let release = AppRelease {
            app_id: source.id.clone(),
            name: source.name.clone(),
            version: "1.0.0".into(),
            tag: "v1.0.0".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            repository: "owner/demo".into(),
            channel: "stable".into(),
            verified: true,
            artifacts: vec![Artifact {
                target: "gnome-extension".into(),
                name: "demo.shell-extension.zip".into(),
                url: "https://example.test/assets/demo.shell-extension.zip".into(),
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                size: 12,
            }],
            fetched_at: "2026-10-01T00:00:00Z".into(),
        };
        let host = HostInfo {
            os: "linux".into(),
            architecture: "x86_64".into(),
            gnome_version: Some(50),
            extensions_directory: "/tmp/data/gnome-shell/extensions".into(),
            data_directory: "/tmp/data".into(),
        };
        assert!(select_artifact(&release, &source, &host).is_ok());
        let mut wrong = host.clone();
        wrong.os = "windows".into();
        assert!(select_artifact(&release, &source, &wrong).is_err());
    }

    #[test]
    fn manifest_rejects_duplicate_or_traversal_targets() {
        let manifest = serde_json::json!({
            "schema": 1,
            "project": "demo",
            "repository": "owner/demo",
            "release": {"version": "1.0.0", "tag": "v1.0.0", "commit": "0123456789abcdef0123456789abcdef01234567"},
            "protected_assets": ["demo.zip"],
            "targets": [{"target": "gnome-extension", "path": "../demo.zip", "name": "demo.zip", "size": 1, "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "protected": true}]
        });
        assert!(parse_manifest(&manifest).is_err());
    }

    #[test]
    fn receipt_requires_complete_matching_public_asset_evidence() {
        let manifest = serde_json::json!({
            "schema": 1,
            "project": "demo",
            "repository": "owner/demo",
            "release": {"version": "1.0.0", "tag": "v1.0.0", "commit": "0123456789abcdef0123456789abcdef01234567"},
            "protected_assets": ["demo.shell-extension.zip"],
            "targets": [{"target": "gnome-extension", "path": "demo.shell-extension.zip", "name": "demo.shell-extension.zip", "size": 3, "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "protected": true}]
        });
        let normalized = parse_manifest(&manifest).expect("manifest");
        let receipt = serde_json::json!({
            "ok": true,
            "command": "ensure-release",
            "dry_run": false,
            "draft": false,
            "public": true,
            "finalized": false,
            "repository": "owner/demo",
            "tag": "v1.0.0",
            "source_commit": "0123456789abcdef0123456789abcdef01234567",
            "manifest_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "manifest_asset": {"name": "release-manifest.json", "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", "size": 1, "verified": true, "verification": "download", "public": true},
            "assets": [{"name": "demo.shell-extension.zip", "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "size": 3, "verified": true, "verification": "download", "public": true}]
        });
        assert!(validate_receipt(
            &receipt,
            &normalized,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            1,
        )
        .is_err());

        let mut complete = receipt;
        complete["manifest_sha256"] = Value::String(
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
        );
        assert!(validate_receipt(
            &complete,
            &normalized,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            1,
        )
        .is_ok());
        complete["public"] = Value::Bool(false);
        assert!(validate_receipt(
            &complete,
            &normalized,
            "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            1,
        )
        .is_err());
    }

    #[test]
    fn platform_target_must_match_host_architecture() {
        let source = source();
        let release = AppRelease {
            app_id: source.id.clone(),
            name: source.name.clone(),
            version: "1.0.0".into(),
            tag: "v1.0.0".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            repository: "owner/demo".into(),
            channel: "stable".into(),
            verified: true,
            artifacts: vec![Artifact {
                target: "linux-arm64".into(),
                name: "demo.shell-extension.zip".into(),
                url: "https://example.test/assets/demo.shell-extension.zip".into(),
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                size: 12,
            }],
            fetched_at: "2026-10-01T00:00:00Z".into(),
        };
        let host = HostInfo {
            os: "linux".into(),
            architecture: "x86_64".into(),
            gnome_version: Some(50),
            extensions_directory: "/tmp/data/gnome-shell/extensions".into(),
            data_directory: "/tmp/data".into(),
        };
        assert!(select_artifact(&release, &source, &host).is_err());
    }
}
