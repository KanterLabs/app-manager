//! A small, self-hostable broker for GitHub App and Gitea Actions builds.
//!
//! The broker deliberately keeps the HTTP surface small.  The [`Broker`] type
//! owns configuration, durable request state, and provider adapters, which
//! makes the same implementation usable by the binary and by local fixture
//! tests without making real provider mutations.

use anyhow::{anyhow, bail, Context, Result};
use app_manager_core::models::{BuildRecord, RepositoryMapping};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use fs2::FileExt;
use hmac::{Hmac, Mac};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::redirect::Policy;
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::time::Duration;
use url::Url;
use uuid::Uuid;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type HmacSha256 = Hmac<Sha256>;

const STATE_SCHEMA: u32 = 1;
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 15;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_WEBHOOK_BODY_BYTES: usize = 1024 * 1024;

/// Provider selected by a repository mapping.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Github,
    Gitea,
}

/// A short alias for callers that prefer `Provider` to `ProviderKind`.
pub type Provider = ProviderKind;

impl ProviderKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitea => "gitea",
        }
    }
}

/// A repository the broker is allowed to operate on.
///
/// The workflow is configuration-owned.  It is never accepted from a build
/// request, which prevents a bearer credential from dispatching an arbitrary
/// workflow in an otherwise authorized repository.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub provider: ProviderKind,
    pub owner: String,
    pub repository: String,
    pub workflow: String,
    pub default_ref: String,
    /// The provider API base, for example `https://api.github.com` or a
    /// Gitea `/api/v1` base.  This is the only origin an adapter may contact.
    pub api_url: String,
    #[serde(default)]
    pub installation_id: Option<u64>,
    /// Name of an environment variable containing a scoped Gitea token.
    #[serde(default, alias = "tokenEnv")]
    pub gitea_token_env: Option<String>,
    /// Optional environment variable for a Gitea webhook secret.
    #[serde(default)]
    pub webhook_secret_env: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// GitHub App configuration.  The private key is intentionally represented
/// only by an environment variable or a protected file path.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GithubAppConfig {
    pub app_id: u64,
    #[serde(default)]
    pub private_key_env: Option<String>,
    #[serde(default)]
    pub private_key_file: Option<PathBuf>,
    /// Alias retained for deployments that call the setting a path.
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    #[serde(default)]
    pub webhook_secret_env: Option<String>,
}

/// Account credentials are SHA-256 digests of bearer tokens.  A raw bearer
/// token may be supplied through `token_env` when configuration is generated
/// entirely from protected process metadata; it is hashed in memory and never
/// persisted or logged.  `token_sha256`/`token_sha256_env` keep only the digest.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountConfig {
    pub account: String,
    #[serde(default, alias = "credentialSha256")]
    pub token_sha256: Option<String>,
    #[serde(default)]
    pub token_sha256_env: Option<String>,
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub repository_ids: Vec<String>,
}

/// Complete broker configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrokerConfig {
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub repositories: Vec<RepositoryConfig>,
    #[serde(default)]
    pub github: Option<GithubAppConfig>,
    #[serde(default = "default_http_timeout_secs")]
    pub http_timeout_secs: u64,
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    #[serde(default = "default_max_webhook_body_bytes")]
    pub max_webhook_body_bytes: usize,
}

fn default_true() -> bool {
    true
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("./broker-state")
}

fn default_http_timeout_secs() -> u64 {
    DEFAULT_HTTP_TIMEOUT_SECS
}

fn default_max_response_bytes() -> usize {
    DEFAULT_MAX_RESPONSE_BYTES
}

fn default_max_webhook_body_bytes() -> usize {
    DEFAULT_MAX_WEBHOOK_BODY_BYTES
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            accounts: Vec::new(),
            repositories: Vec::new(),
            github: None,
            http_timeout_secs: DEFAULT_HTTP_TIMEOUT_SECS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_webhook_body_bytes: DEFAULT_MAX_WEBHOOK_BODY_BYTES,
        }
    }
}

impl BrokerConfig {
    /// Validate configuration without reading any secret value.
    pub fn validate(&self) -> Result<()> {
        if self.http_timeout_secs == 0 {
            bail!("http_timeout_secs must be greater than zero");
        }
        if self.max_response_bytes == 0 {
            bail!("max_response_bytes must be greater than zero");
        }
        if self.max_webhook_body_bytes == 0 {
            bail!("max_webhook_body_bytes must be greater than zero");
        }

        let mut repository_ids = BTreeSet::new();
        let mut has_github = false;
        for repository in &self.repositories {
            repository.validate()?;
            if !repository_ids.insert(repository.id.clone()) {
                bail!("duplicate repository id");
            }
            has_github |= repository.provider == ProviderKind::Github;
        }

        let mut accounts = BTreeSet::new();
        for account in &self.accounts {
            account.validate()?;
            if !accounts.insert(account.account.clone()) {
                bail!("duplicate account name");
            }
            for repository_id in &account.repository_ids {
                if !repository_ids.contains(repository_id) {
                    bail!("account references an unknown repository id");
                }
            }
        }

        if has_github {
            let github = self
                .github
                .as_ref()
                .context("GitHub repositories require github App configuration")?;
            github.validate()?;
        } else if let Some(github) = &self.github {
            github.validate()?;
        }
        Ok(())
    }
}

impl RepositoryConfig {
    fn validate(&self) -> Result<()> {
        for value in [
            &self.id,
            &self.owner,
            &self.repository,
            &self.workflow,
            &self.default_ref,
        ] {
            if value.trim().is_empty() || value.chars().any(|character| character.is_control()) {
                bail!("repository mapping contains an empty or control-character field");
            }
        }
        for value in [&self.owner, &self.repository, &self.workflow] {
            if value
                .bytes()
                .any(|byte| matches!(byte, b'/' | b'\\' | b'?' | b'#' | b'%'))
            {
                bail!("repository owner, repository, and workflow must be one path component");
            }
        }
        validate_api_url(&self.api_url)?;
        match self.provider {
            ProviderKind::Github => {
                if self.installation_id.is_none() {
                    bail!("GitHub repository is missing installation_id");
                }
            }
            ProviderKind::Gitea => {
                let token_env = self
                    .gitea_token_env
                    .as_deref()
                    .context("Gitea repository is missing gitea_token_env")?;
                validate_env_name(token_env)?;
            }
        }
        if let Some(secret_env) = &self.webhook_secret_env {
            validate_env_name(secret_env)?;
        }
        Ok(())
    }

    fn mapping(&self) -> RepositoryMapping {
        RepositoryMapping {
            id: self.id.clone(),
            name: if self.name.trim().is_empty() {
                format!("{}/{}", self.owner, self.repository)
            } else {
                self.name.clone()
            },
            provider: self.provider.as_str().to_owned(),
            owner: self.owner.clone(),
            repository: self.repository.clone(),
            workflow: self.workflow.clone(),
            default_ref: self.default_ref.clone(),
            installation_id: self.installation_id,
        }
    }

    fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repository)
    }
}

impl GithubAppConfig {
    fn validate(&self) -> Result<()> {
        if self.app_id == 0 {
            bail!("GitHub app_id must be greater than zero");
        }
        if let Some(name) = &self.private_key_env {
            validate_env_name(name)?;
        }
        if self.private_key_env.is_none()
            && self.private_key_file.is_none()
            && self.private_key_path.is_none()
        {
            bail!("GitHub App requires private_key_env or private_key_file");
        }
        if self.private_key_env.is_some()
            && (self.private_key_file.is_some() || self.private_key_path.is_some())
        {
            bail!("configure only one GitHub private key source");
        }
        if self.private_key_file.is_some() && self.private_key_path.is_some() {
            bail!("configure only one GitHub private key file field");
        }
        if let Some(name) = &self.webhook_secret_env {
            validate_env_name(name)?;
        }
        Ok(())
    }

    fn private_key_file(&self) -> Option<&Path> {
        self.private_key_file
            .as_deref()
            .or(self.private_key_path.as_deref())
    }
}

impl AccountConfig {
    fn validate(&self) -> Result<()> {
        if self.account.trim().is_empty()
            || self.account.chars().any(|character| character.is_control())
        {
            bail!("account name is empty or contains a control character");
        }
        if self.token_sha256.is_none()
            && self.token_sha256_env.is_none()
            && self.token_env.is_none()
        {
            bail!("account requires token_sha256, token_sha256_env, or token_env");
        }
        if let Some(hash) = &self.token_sha256 {
            parse_hash(hash).context("invalid account token_sha256")?;
        }
        if let Some(name) = &self.token_sha256_env {
            validate_env_name(name)?;
        }
        if let Some(name) = &self.token_env {
            validate_env_name(name)?;
        }
        let mut ids = BTreeSet::new();
        for repository_id in &self.repository_ids {
            if repository_id.trim().is_empty() || !ids.insert(repository_id) {
                bail!("account contains an empty or duplicate repository id");
            }
        }
        Ok(())
    }

    fn expected_hash(&self) -> Option<[u8; 32]> {
        if let Some(hash) = &self.token_sha256 {
            return parse_hash(hash).ok();
        }
        if let Some(name) = &self.token_sha256_env {
            return env::var(name)
                .ok()
                .and_then(|value| parse_hash(&value).ok());
        }
        self.token_env.as_ref().and_then(|name| {
            env::var(name)
                .ok()
                .map(|value| sha256_bytes(value.as_bytes()))
        })
    }
}

fn validate_env_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name
            .chars()
            .any(|character| character.is_control() || character == '=')
    {
        bail!("invalid environment variable name");
    }
    Ok(())
}

fn parse_hash(value: &str) -> Result<[u8; 32]> {
    let value = value.trim();
    if value.len() != 64 {
        bail!("SHA-256 digest must contain exactly 64 hexadecimal characters");
    }
    let decoded = hex::decode(value).context("invalid hexadecimal SHA-256 digest")?;
    let mut result = [0u8; 32];
    result.copy_from_slice(&decoded);
    Ok(result)
}

fn sha256_bytes(value: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(value);
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    bytes
}

fn validate_api_url(value: &str) -> Result<()> {
    let url = Url::parse(value).context("invalid provider api_url")?;
    if url.query().is_some()
        || url.fragment().is_some()
        || url.username() != ""
        || url.password().is_some()
    {
        bail!("provider api_url must not contain credentials, query, or fragment");
    }
    let scheme = url.scheme();
    if scheme != "https" && !(scheme == "http" && is_loopback_url(&url)) {
        bail!("provider api_url must use HTTPS; HTTP is allowed only for loopback fixtures");
    }
    if url.host_str().is_none() {
        bail!("provider api_url must include a host");
    }
    Ok(())
}

fn is_loopback_url(url: &Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
        None => false,
    }
}

fn endpoint(base: &str, segments: &[String]) -> Result<Url> {
    let mut url = Url::parse(base).context("invalid configured provider URL")?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| anyhow!("provider URL cannot be used as a base"))?;
        for segment in segments {
            path.push(segment);
        }
    }
    Ok(url)
}

fn exact_sha(value: &str) -> Option<String> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

fn valid_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value
            .chars()
            .any(|character| character.is_control() || matches!(character, '\\' | '?' | '#' | '%'))
        && !value.starts_with("refs/")
        && exact_sha(value).is_none()
        && !value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
}

/// The state retained by the broker.  State is replaced atomically and the
/// previous state file is retained as a backup before each replacement.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerState {
    pub schema: u32,
    #[serde(default)]
    pub builds: BTreeMap<String, StoredBuild>,
    #[serde(default)]
    pub deliveries: BTreeMap<String, StoredDelivery>,
    #[serde(default)]
    pub disabled_repositories: BTreeSet<String>,
}

impl BrokerState {
    fn initial() -> Self {
        Self {
            schema: STATE_SCHEMA,
            builds: BTreeMap::new(),
            deliveries: BTreeMap::new(),
            disabled_repositories: BTreeSet::new(),
        }
    }
}

/// Durable representation of a build request and its provider correlation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct StoredBuild {
    pub record: BuildRecord,
    pub account: String,
    pub request_id: String,
    pub dispatch_state: DispatchState,
    pub provider_run_id: Option<String>,
}

/// Durable webhook delivery marker.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct StoredDelivery {
    pub received_at: String,
    pub event: String,
}

/// State of the one provider dispatch attempt for a request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DispatchState {
    Pending,
    Accepted,
    Rejected,
    Ambiguous,
}

#[derive(Clone)]
pub struct StateStore {
    inner: Arc<StateStoreInner>,
}

struct StateStoreInner {
    directory: PathBuf,
    state: Mutex<BrokerState>,
}

impl StateStore {
    /// Open an existing state directory without deleting or resetting it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let directory = path.as_ref().to_path_buf();
        fs::create_dir_all(&directory).context("create broker state directory")?;
        let state_path = directory.join("state.json");
        let backup_path = directory.join("state.json.bak");
        let lock_file = acquire_process_lock(&directory)?;
        let open_result: Result<BrokerState> = (|| {
            let state = load_state(&state_path, &backup_path)?;
            if !state_path.exists() {
                persist_state(&directory, &state)?;
            }
            Ok(state)
        })();
        let _ = lock_file.unlock();
        let state = open_result?;
        let store = Self {
            inner: Arc::new(StateStoreInner {
                directory,
                state: Mutex::new(state),
            }),
        };
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.inner.directory
    }

    pub fn snapshot(&self) -> Result<BrokerState> {
        let lock_file = acquire_process_lock(&self.inner.directory)?;
        let result = (|| {
            let state_path = self.inner.directory.join("state.json");
            let backup_path = self.inner.directory.join("state.json.bak");
            let state = load_state(&state_path, &backup_path)?;
            let mut guard = self
                .inner
                .state
                .lock()
                .map_err(|_| anyhow!("broker state lock poisoned"))?;
            *guard = state.clone();
            Ok(state)
        })();
        let _ = lock_file.unlock();
        result
    }

    /// Apply a transaction and replace state only after the new JSON and its
    /// backup have been durably written.
    pub fn update<T>(&self, mutate: impl FnOnce(&mut BrokerState) -> Result<T>) -> Result<T> {
        let lock_file = acquire_process_lock(&self.inner.directory)?;
        let result = (|| {
            let mut guard = self
                .inner
                .state
                .lock()
                .map_err(|_| anyhow!("broker state lock poisoned"))?;
            // Another broker process may have committed state since this
            // instance last observed it.  The file lock makes this reread
            // authoritative and prevents lost updates across processes.
            let state_path = self.inner.directory.join("state.json");
            let backup_path = self.inner.directory.join("state.json.bak");
            let mut next = load_state(&state_path, &backup_path)?;
            let result = mutate(&mut next)?;
            persist_state(&self.inner.directory, &next)?;
            *guard = next;
            Ok(result)
        })();
        let _ = lock_file.unlock();
        result
    }

    pub fn insert_build(&self, build: StoredBuild) -> Result<()> {
        let id = build.record.id.clone();
        self.update(|state| {
            if state.builds.contains_key(&id) {
                bail!("build request already exists");
            }
            state.builds.insert(id, build);
            Ok(())
        })
    }

    pub fn update_build(&self, build: StoredBuild) -> Result<()> {
        let id = build.record.id.clone();
        self.update(|state| {
            if !state.builds.contains_key(&id) {
                bail!("build request does not exist");
            }
            state.builds.insert(id, build);
            Ok(())
        })
    }

    pub fn build(&self, id: &str) -> Result<Option<StoredBuild>> {
        Ok(self.snapshot()?.builds.get(id).cloned())
    }

    pub fn builds_for_account(&self, account: &str) -> Result<Vec<StoredBuild>> {
        let mut builds: Vec<_> = self
            .snapshot()?
            .builds
            .into_values()
            .filter(|build| build.account == account)
            .collect();
        builds.sort_by(|left, right| left.record.created_at.cmp(&right.record.created_at));
        Ok(builds)
    }

    pub fn mark_delivery(&self, delivery_id: &str, event: &str) -> Result<bool> {
        let delivery_id = delivery_id.to_owned();
        let event = event.to_owned();
        self.update(|state| {
            if state.deliveries.contains_key(&delivery_id) {
                return Ok(false);
            }
            state.deliveries.insert(
                delivery_id,
                StoredDelivery {
                    received_at: now_string(),
                    event,
                },
            );
            Ok(true)
        })
    }

    pub fn disable_repositories(&self, ids: impl IntoIterator<Item = String>) -> Result<()> {
        self.update(|state| {
            state.disabled_repositories.extend(ids);
            Ok(())
        })
    }

    pub fn is_disabled(&self, id: &str) -> Result<bool> {
        Ok(self.snapshot()?.disabled_repositories.contains(id))
    }
}

fn acquire_process_lock(directory: &Path) -> Result<File> {
    let lock_path = directory.join("state.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .context("open broker state lock")?;
    lock_file.lock_exclusive().context("lock broker state")?;
    Ok(lock_file)
}

fn load_state(path: &Path, backup: &Path) -> Result<BrokerState> {
    if !path.exists() {
        if backup.exists() {
            bail!(
                "broker state.json is missing while state.json.bak exists; explicit operator repair is required"
            );
        }
        return Ok(BrokerState::initial());
    }
    let bytes = fs::read(path).with_context(|| format!("read broker state {}", path.display()))?;
    if bytes.is_empty() {
        bail!("broker state.json is empty; refusing automatic reset");
    }
    let mut state: BrokerState = serde_json::from_slice(&bytes).context("parse broker state")?;
    if state.schema > STATE_SCHEMA {
        bail!("unsupported future broker state schema; explicit operator migration is required");
    }
    validate_state(&state)?;
    // Schema zero is the versionless initial broker format.  It is migrated
    // in memory and will be upgraded only as part of the next transaction;
    // startup never rewrites the operator's original bytes.
    state.schema = STATE_SCHEMA;
    Ok(state)
}

fn validate_state(state: &BrokerState) -> Result<()> {
    for (id, build) in &state.builds {
        if id.is_empty()
            || build.record.id != *id
            || build.request_id != build.record.id
            || build.account.is_empty()
            || exact_sha(&build.record.commit).is_none()
        {
            bail!("broker state contains an invalid build record");
        }
    }
    for (id, delivery) in &state.deliveries {
        if id.is_empty() || delivery.event.is_empty() || delivery.received_at.is_empty() {
            bail!("broker state contains an invalid delivery record");
        }
    }
    if state.disabled_repositories.iter().any(|id| id.is_empty()) {
        bail!("broker state contains an empty disabled repository id");
    }
    Ok(())
}

fn persist_state(directory: &Path, state: &BrokerState) -> Result<()> {
    validate_state(state)?;
    let bytes = serde_json::to_vec_pretty(state).context("serialize broker state")?;
    let suffix = Uuid::new_v4().simple().to_string();
    let state_path = directory.join("state.json");
    let backup_path = directory.join("state.json.bak");
    let temp_path = directory.join(format!("state.json.tmp-{suffix}"));
    let backup_temp_path = directory.join(format!("state.json.bak.tmp-{suffix}"));

    let write_result = (|| {
        let previous_bytes = if state_path.exists() {
            let bytes = fs::read(&state_path).context("read current broker state")?;
            if bytes.is_empty() {
                bail!("current broker state is empty; refusing to replace it");
            }
            Some(bytes)
        } else {
            None
        };
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .context("create broker state temporary file")?;
        file.write_all(&bytes).context("write broker state")?;
        file.sync_all().context("sync broker state")?;

        if let Some(previous_bytes) = previous_bytes {
            fs::copy(&state_path, &backup_temp_path).context("copy broker state backup")?;
            let copied_bytes = fs::read(&backup_temp_path).context("verify broker state backup")?;
            if copied_bytes != previous_bytes {
                bail!("broker state backup differs from the exact prior state");
            }
            let backup_file = OpenOptions::new().read(true).open(&backup_temp_path)?;
            backup_file.sync_all().context("sync broker state backup")?;
            fs::rename(&backup_temp_path, &backup_path).context("replace broker state backup")?;
        }
        fs::rename(&temp_path, &state_path).context("replace broker state")?;
        sync_directory(directory)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
        let _ = fs::remove_file(&backup_temp_path);
    }
    write_result
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)
        .context("open broker state directory")?
        .sync_all()
        .context("sync broker state directory")
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> Result<()> {
    Ok(())
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Clone)]
pub struct Broker {
    inner: Arc<BrokerInner>,
}

struct BrokerInner {
    config: Arc<BrokerConfig>,
    store: StateStore,
    providers: ProviderClient,
}

impl Broker {
    /// Construct the library broker with a state directory override.
    pub fn new(mut config: BrokerConfig, state_dir: impl AsRef<Path>) -> Result<Self> {
        config.state_dir = state_dir.as_ref().to_path_buf();
        Self::from_config(config)
    }

    /// Construct the broker using `config.state_dir`.
    pub fn from_config(config: BrokerConfig) -> Result<Self> {
        config.validate()?;
        let store = StateStore::open(&config.state_dir)?;
        let providers = ProviderClient::new(Arc::new(config.clone()))?;
        Ok(Self {
            inner: Arc::new(BrokerInner {
                config: Arc::new(config),
                store,
                providers,
            }),
        })
    }

    pub fn config(&self) -> &BrokerConfig {
        &self.inner.config
    }

    pub fn store(&self) -> &StateStore {
        &self.inner.store
    }

    /// Build an Axum router for the binary or a local fixture test.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .route("/api/repositories", get(api_repositories))
            .route("/api/status", get(api_status))
            .route("/api/builds", get(api_builds).post(api_build))
            .route("/webhooks/github", post(webhook_github))
            .layer(DefaultBodyLimit::max(
                self.inner.config.max_webhook_body_bytes,
            ))
            .with_state(self.clone())
    }

    pub async fn listen(self, listener: tokio::net::TcpListener) -> Result<()> {
        axum::serve(listener, self.router())
            .await
            .context("serve broker HTTP")
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<AccountConfig, ApiError> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                if scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() {
                    Some(token.as_bytes())
                } else {
                    None
                }
            })
            .ok_or_else(ApiError::unauthorized)?;
        let actual = sha256_bytes(token);
        let mut matched: Option<usize> = None;
        for (index, account) in self.inner.config.accounts.iter().enumerate() {
            let expected = account.expected_hash().unwrap_or([0u8; 32]);
            let equal = expected.ct_eq(&actual).unwrap_u8() == 1;
            if equal && matched.is_none() {
                matched = Some(index);
            }
        }
        matched
            .and_then(|index| self.inner.config.accounts.get(index).cloned())
            .ok_or_else(ApiError::unauthorized)
    }

    fn repository_for_account(
        &self,
        account: &AccountConfig,
        id: &str,
    ) -> Result<RepositoryConfig, ApiError> {
        if !account.repository_ids.iter().any(|allowed| allowed == id) {
            return Err(ApiError::forbidden(
                "repository is not authorized for this account",
            ));
        }
        let repository = self
            .inner
            .config
            .repositories
            .iter()
            .find(|repository| repository.id == id && repository.enabled)
            .cloned()
            .ok_or_else(|| ApiError::not_found("repository is not configured"))?;
        if self
            .inner
            .store
            .is_disabled(id)
            .map_err(ApiError::internal)?
        {
            return Err(ApiError::forbidden("repository access is disabled"));
        }
        Ok(repository)
    }

    fn repositories_for_account(
        &self,
        account: &AccountConfig,
    ) -> Result<Vec<RepositoryConfig>, ApiError> {
        let state = self.inner.store.snapshot().map_err(ApiError::internal)?;
        Ok(self
            .inner
            .config
            .repositories
            .iter()
            .filter(|repository| {
                repository.enabled
                    && account.repository_ids.iter().any(|id| id == &repository.id)
                    && !state.disabled_repositories.contains(&repository.id)
            })
            .cloned()
            .collect())
    }

    async fn refresh_builds(&self, account: &AccountConfig) -> Result<(), ApiError> {
        let builds = self
            .inner
            .store
            .builds_for_account(&account.account)
            .map_err(ApiError::internal)?;
        for mut build in builds {
            if build.dispatch_state != DispatchState::Accepted {
                continue;
            }
            let repository = match self.repository_for_account(account, &build.record.repository_id)
            {
                Ok(repository) => repository,
                Err(_) => continue,
            };
            let run = self
                .inner
                .providers
                .status(&repository, &build)
                .await
                .ok()
                .flatten();
            if let Some(run) = run {
                build.provider_run_id = Some(run.id.clone());
                build.record.status = run.status;
                build.record.run_url = run.url.or(build.record.run_url);
                self.inner
                    .store
                    .update_build(build)
                    .map_err(ApiError::internal)?;
            }
        }
        Ok(())
    }

    fn record_github_delivery(
        &self,
        delivery_id: &str,
        event: &str,
        payload: &Value,
    ) -> Result<bool, ApiError> {
        let mut disabled = Vec::new();
        match event {
            "installation" => {
                let action = payload
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(action, "deleted" | "suspended") {
                    if let Some(installation_id) = payload
                        .get("installation")
                        .and_then(|installation| installation.get("id"))
                        .and_then(Value::as_u64)
                    {
                        disabled = self
                            .inner
                            .config
                            .repositories
                            .iter()
                            .filter(|repository| {
                                repository.provider == ProviderKind::Github
                                    && repository.installation_id == Some(installation_id)
                            })
                            .map(|repository| repository.id.clone())
                            .collect::<Vec<_>>();
                    }
                }
            }
            "installation_repositories" => {
                if payload.get("action").and_then(Value::as_str) == Some("removed") {
                    let removed = payload
                        .get("repositories_removed")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    disabled = self
                        .inner
                        .config
                        .repositories
                        .iter()
                        .filter(|repository| {
                            repository.provider == ProviderKind::Github
                                && removed
                                    .iter()
                                    .any(|item| repository_matches_value(repository, item))
                        })
                        .map(|repository| repository.id.clone())
                        .collect::<Vec<_>>();
                }
            }
            "repository" => {
                if matches!(
                    payload.get("action").and_then(Value::as_str),
                    Some("deleted" | "archived")
                ) {
                    disabled = self
                        .inner
                        .config
                        .repositories
                        .iter()
                        .filter(|repository| {
                            repository.provider == ProviderKind::Github
                                && payload
                                    .get("repository")
                                    .map(|item| repository_matches_value(repository, item))
                                    .unwrap_or(false)
                        })
                        .map(|repository| repository.id.clone())
                        .collect::<Vec<_>>();
                }
            }
            // Push and mirror events are intentionally ignored.  Receiving a
            // webhook never creates or dispatches a build request.
            _ => {}
        }
        let delivery_id = delivery_id.to_owned();
        let event = event.to_owned();
        self.inner
            .store
            .update(|state| {
                if state.deliveries.contains_key(&delivery_id) {
                    return Ok(false);
                }
                state.deliveries.insert(
                    delivery_id,
                    StoredDelivery {
                        received_at: now_string(),
                        event,
                    },
                );
                state.disabled_repositories.extend(disabled);
                Ok(true)
            })
            .map_err(ApiError::internal)
    }
}

fn repository_matches_value(repository: &RepositoryConfig, value: &Value) -> bool {
    let full_name = value
        .get("full_name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            let owner = value
                .get("owner")
                .and_then(|owner| owner.get("login").or_else(|| owner.get("username")))
                .and_then(Value::as_str)?;
            let name = value.get("name").and_then(Value::as_str)?;
            Some(format!("{owner}/{name}"))
        });
    full_name
        .map(|name| name.eq_ignore_ascii_case(&repository.full_name()))
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildRequest {
    repository_id: String,
    reference: String,
    commit: String,
}

#[derive(Serialize)]
struct StatusResponse {
    #[serde(rename = "githubConfigured")]
    github_configured: bool,
    #[serde(rename = "repositoryCount")]
    repository_count: usize,
}

#[derive(Serialize)]
struct WebhookResponse {
    accepted: bool,
    duplicate: bool,
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn api_repositories(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Json<Vec<RepositoryMapping>>, ApiError> {
    let account = broker.authenticate(&headers)?;
    let repositories = broker.repositories_for_account(&account)?;
    Ok(Json(
        repositories
            .into_iter()
            .map(|repository| repository.mapping())
            .collect(),
    ))
}

async fn api_status(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Json<StatusResponse>, ApiError> {
    let account = broker.authenticate(&headers)?;
    let repositories = broker.repositories_for_account(&account)?;
    Ok(Json(StatusResponse {
        github_configured: broker.inner.config.github.is_some(),
        repository_count: repositories.len(),
    }))
}

async fn api_builds(
    State(broker): State<Broker>,
    headers: HeaderMap,
) -> Result<Json<Vec<BuildRecord>>, ApiError> {
    let account = broker.authenticate(&headers)?;
    broker.refresh_builds(&account).await?;
    let active: BTreeSet<_> = broker
        .repositories_for_account(&account)?
        .into_iter()
        .map(|repository| repository.id)
        .collect();
    let builds = broker
        .inner
        .store
        .builds_for_account(&account.account)
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|build| active.contains(&build.record.repository_id))
        .map(|build| build.record)
        .collect();
    Ok(Json(builds))
}

async fn api_build(
    State(broker): State<Broker>,
    headers: HeaderMap,
    Json(request): Json<BuildRequest>,
) -> Result<Response, ApiError> {
    let account = broker.authenticate(&headers)?;
    let repository = broker.repository_for_account(&account, &request.repository_id)?;
    let expected_commit = exact_sha(&request.commit).ok_or_else(|| {
        ApiError::bad_request("commit must be an exact 40-character hexadecimal SHA")
    })?;
    if !valid_reference(&request.reference) {
        return Err(ApiError::bad_request("reference must name a branch or tag"));
    }

    let resolved = broker
        .inner
        .providers
        .resolve_reference(&repository, &request.reference)
        .await
        .map_err(|error| ApiError::provider(format_provider_error("resolve reference", error)))?;
    if resolved != expected_commit {
        return Err(ApiError::conflict("reference does not resolve to commit"));
    }

    // A webhook can revoke an installation while the canonical reference
    // lookup is in flight. Re-read the mapping immediately before creating
    // the durable request or dispatching so a stale pre-lookup authorization
    // decision cannot send a build after revocation.
    let repository = broker.repository_for_account(&account, &request.repository_id)?;

    let request_id = Uuid::new_v4().simple().to_string();
    let build = StoredBuild {
        record: BuildRecord {
            id: request_id.clone(),
            repository_id: repository.id.clone(),
            commit: expected_commit.clone(),
            reference: request.reference.clone(),
            provider: repository.provider.as_str().to_owned(),
            status: "pending".to_owned(),
            run_url: None,
            created_at: now_string(),
        },
        account: account.account.clone(),
        request_id: request_id.clone(),
        dispatch_state: DispatchState::Pending,
        provider_run_id: None,
    };

    // This write is deliberately before the provider request.  A timeout or
    // connection reset after the provider accepted the request is therefore
    // represented as `ambiguous` and cannot be retried blindly.
    broker
        .inner
        .store
        .insert_build(build)
        .map_err(ApiError::internal)?;

    let outcome = broker
        .inner
        .providers
        .dispatch(
            &repository,
            &request.reference,
            &expected_commit,
            &request_id,
        )
        .await;
    let mut stored = broker
        .inner
        .store
        .build(&request_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal(anyhow!("persisted build request disappeared")))?;
    match outcome {
        Ok(DispatchOutcome::Accepted { run_id, url }) => {
            stored.dispatch_state = DispatchState::Accepted;
            stored.provider_run_id = run_id;
            stored.record.status = "queued".to_owned();
            stored.record.run_url = url;
        }
        Ok(DispatchOutcome::Rejected) => {
            stored.dispatch_state = DispatchState::Rejected;
            stored.record.status = "failed".to_owned();
        }
        Err(_) => {
            stored.dispatch_state = DispatchState::Ambiguous;
            stored.record.status = "ambiguous".to_owned();
        }
    }
    broker
        .inner
        .store
        .update_build(stored.clone())
        .map_err(ApiError::internal)?;
    let status = if stored.dispatch_state == DispatchState::Ambiguous {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(stored.record)).into_response())
}

async fn webhook_github(
    State(broker): State<Broker>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<WebhookResponse>, ApiError> {
    let github = broker
        .inner
        .config
        .github
        .as_ref()
        .ok_or_else(|| ApiError::not_found("GitHub webhooks are not configured"))?;
    let secret_env = github
        .webhook_secret_env
        .as_ref()
        .ok_or_else(|| ApiError::internal(anyhow!("GitHub webhook secret is not configured")))?;
    let secret = env::var(secret_env)
        .map_err(|_| ApiError::internal(anyhow!("GitHub webhook secret is unavailable")))?;
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::unauthorized)?;
    if !verify_signature(secret.as_bytes(), &body, signature) {
        return Err(ApiError::unauthorized());
    }
    let delivery_id = headers
        .get("x-github-delivery")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing X-GitHub-Delivery"))?;
    let event = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown");
    let payload: Value =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("invalid webhook JSON"))?;
    let inserted = broker.record_github_delivery(delivery_id, event, &payload)?;
    if !inserted {
        return Ok(Json(WebhookResponse {
            accepted: true,
            duplicate: true,
        }));
    }
    Ok(Json(WebhookResponse {
        accepted: true,
        duplicate: false,
    }))
}

fn verify_signature(secret: &[u8], body: &[u8], header_value: &str) -> bool {
    let Some(encoded) = header_value.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(encoded) else {
        return false;
    };
    if expected.len() != 32 {
        return false;
    }
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl ApiError {
    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: "authentication required",
        }
    }

    fn forbidden(message: &'static str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message,
        }
    }

    fn not_found(message: &'static str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message,
        }
    }

    fn bad_request(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }

    fn conflict(message: &'static str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message,
        }
    }

    fn provider(_message: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: "provider request failed",
        }
    }

    fn internal(_error: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "broker operation failed",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

fn format_provider_error(action: &str, error: anyhow::Error) -> String {
    let _ = error;
    action.to_owned()
}

#[derive(Clone)]
struct ProviderClient {
    http: Client,
    github: GithubAuthenticator,
    max_response_bytes: usize,
}

impl ProviderClient {
    fn new(config: Arc<BrokerConfig>) -> Result<Self> {
        let timeout = Duration::from_secs(config.http_timeout_secs);
        let http = Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .redirect(Policy::none())
            .build()
            .context("create provider HTTP client")?;
        Ok(Self {
            max_response_bytes: config.max_response_bytes,
            github: GithubAuthenticator::new(config.github.clone()),
            http,
        })
    }

    async fn auth(&self, repository: &RepositoryConfig) -> Result<AuthHeader> {
        match repository.provider {
            ProviderKind::Github => Ok(AuthHeader::Bearer(
                self.github
                    .installation_token(repository, &self.http, self.max_response_bytes)
                    .await?,
            )),
            ProviderKind::Gitea => {
                let env_name = repository
                    .gitea_token_env
                    .as_deref()
                    .context("Gitea token environment variable is not configured")?;
                let token = env::var(env_name)
                    .context("Gitea token environment variable is unavailable")?;
                if token.is_empty() {
                    bail!("Gitea token environment variable is empty");
                }
                Ok(AuthHeader::Token(token))
            }
        }
    }

    async fn resolve_reference(
        &self,
        repository: &RepositoryConfig,
        reference: &str,
    ) -> Result<String> {
        let url = endpoint(
            &repository.api_url,
            &[
                "repos".to_owned(),
                repository.owner.clone(),
                repository.repository.clone(),
                "commits".to_owned(),
                reference.to_owned(),
            ],
        )?;
        let auth = self.auth(repository).await?;
        let response = self.request(Method::GET, url, Some(&auth), None).await?;
        if !response.status.is_success() {
            bail!("provider rejected reference lookup");
        }
        let value: Value = serde_json::from_slice(&response.body)
            .context("invalid provider reference response")?;
        let sha = value
            .get("sha")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .get("commit")
                    .and_then(|commit| commit.get("id"))
                    .and_then(Value::as_str)
            })
            .and_then(exact_sha)
            .context("provider reference response did not contain a canonical commit SHA")?;
        Ok(sha)
    }

    async fn dispatch(
        &self,
        repository: &RepositoryConfig,
        reference: &str,
        commit: &str,
        request_id: &str,
    ) -> Result<DispatchOutcome> {
        let url = endpoint(
            &repository.api_url,
            &[
                "repos".to_owned(),
                repository.owner.clone(),
                repository.repository.clone(),
                "actions".to_owned(),
                "workflows".to_owned(),
                repository.workflow.clone(),
                "dispatches".to_owned(),
            ],
        )?;
        let auth = self.auth(repository).await?;
        let body = json!({
            "ref": reference,
            "inputs": {
                "expected_commit": commit,
                "request_id": request_id,
            }
        });
        let response = match self
            .request(Method::POST, url, Some(&auth), Some(body))
            .await
        {
            Ok(response) => response,
            // A network failure after sending a dispatch is ambiguous.  The
            // caller records this and must not retry automatically.
            Err(error) => return Err(error),
        };
        if !response.status.is_success() {
            return Ok(DispatchOutcome::Rejected);
        }
        let value: Option<Value> = if response.body.is_empty() {
            None
        } else {
            serde_json::from_slice(&response.body).ok()
        };
        let run_id = value
            .as_ref()
            .and_then(|value| {
                value
                    .get("run_id")
                    .or_else(|| value.get("runId"))
                    .or_else(|| value.get("id"))
            })
            .and_then(value_to_id);
        let url = value.as_ref().and_then(run_url);
        Ok(DispatchOutcome::Accepted { run_id, url })
    }

    async fn status(
        &self,
        repository: &RepositoryConfig,
        build: &StoredBuild,
    ) -> Result<Option<ProviderRun>> {
        let auth = self.auth(repository).await?;
        let url = if let Some(run_id) = &build.provider_run_id {
            endpoint(
                &repository.api_url,
                &[
                    "repos".to_owned(),
                    repository.owner.clone(),
                    repository.repository.clone(),
                    "actions".to_owned(),
                    "runs".to_owned(),
                    run_id.clone(),
                ],
            )?
        } else {
            let run_segments = if repository.provider == ProviderKind::Github {
                vec![
                    "repos".to_owned(),
                    repository.owner.clone(),
                    repository.repository.clone(),
                    "actions".to_owned(),
                    "workflows".to_owned(),
                    repository.workflow.clone(),
                    "runs".to_owned(),
                ]
            } else {
                vec![
                    "repos".to_owned(),
                    repository.owner.clone(),
                    repository.repository.clone(),
                    "actions".to_owned(),
                    "runs".to_owned(),
                ]
            };
            let mut url = endpoint(&repository.api_url, &run_segments)?;
            if repository.provider == ProviderKind::Github {
                url.query_pairs_mut()
                    .append_pair("event", "workflow_dispatch");
                url.query_pairs_mut().append_pair("per_page", "100");
            } else {
                url.query_pairs_mut().append_pair("limit", "50");
            }
            url
        };
        let response = self.request(Method::GET, url, Some(&auth), None).await?;
        if !response.status.is_success() {
            return Ok(None);
        }
        let value: Value =
            serde_json::from_slice(&response.body).context("invalid provider status response")?;
        if build.provider_run_id.is_some() {
            Ok(run_from_value(
                &value,
                &build.record.commit,
                &build.request_id,
                false,
            ))
        } else {
            let runs = value
                .get("workflow_runs")
                .or_else(|| value.get("workflowRuns"))
                .or_else(|| value.get("runs"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(runs
                .iter()
                .find_map(|run| run_from_value(run, &build.record.commit, &build.request_id, true)))
        }
    }

    async fn request(
        &self,
        method: Method,
        url: Url,
        auth: Option<&AuthHeader>,
        body: Option<Value>,
    ) -> Result<ProviderResponse> {
        let mut request = self
            .http
            .request(method, url)
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "app-manager-broker/0.1");
        if let Some(auth) = auth {
            match auth {
                AuthHeader::Bearer(token) => {
                    request = request.bearer_auth(token);
                }
                AuthHeader::Token(token) => {
                    request = request.header(header::AUTHORIZATION, format!("token {token}"));
                }
            }
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.context("provider HTTP request")?;
        let status = response.status();
        let body = read_limited(response, self.max_response_bytes).await?;
        Ok(ProviderResponse { status, body })
    }
}

enum AuthHeader {
    Bearer(String),
    Token(String),
}

struct ProviderResponse {
    status: reqwest::StatusCode,
    body: Vec<u8>,
}

enum DispatchOutcome {
    Accepted {
        run_id: Option<String>,
        url: Option<String>,
    },
    Rejected,
}

#[derive(Clone, Debug)]
struct ProviderRun {
    id: String,
    status: String,
    url: Option<String>,
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        bail!("provider response exceeds configured limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read provider response")? {
        if body.len().saturating_add(chunk.len()) > limit {
            bail!("provider response exceeds configured limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Clone)]
struct GithubAuthenticator {
    config: Option<GithubAppConfig>,
    cache: Arc<Mutex<HashMap<(String, u64), CachedInstallationToken>>>,
}

#[derive(Clone)]
struct CachedInstallationToken {
    token: String,
    expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct GithubClaims {
    iat: i64,
    exp: i64,
    iss: u64,
}

impl GithubAuthenticator {
    fn new(config: Option<GithubAppConfig>) -> Self {
        Self {
            config,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn app_jwt(&self) -> Result<String> {
        let config = self
            .config
            .as_ref()
            .context("GitHub App configuration is unavailable")?;
        let pem = if let Some(name) = &config.private_key_env {
            env::var(name).context("GitHub private key environment variable is unavailable")?
        } else if let Some(path) = config.private_key_file() {
            validate_private_key_metadata(path)?;
            fs::read_to_string(path).context("read GitHub private key file")?
        } else {
            bail!("GitHub private key source is unavailable");
        };
        if pem.trim().is_empty() {
            bail!("GitHub private key is empty");
        }
        let key =
            EncodingKey::from_rsa_pem(pem.as_bytes()).context("parse GitHub RSA private key")?;
        let now = Utc::now().timestamp();
        let claims = GithubClaims {
            iat: now - 60,
            exp: now + 9 * 60,
            iss: config.app_id,
        };
        encode(&Header::new(Algorithm::RS256), &claims, &key).context("sign GitHub App JWT")
    }

    async fn installation_token(
        &self,
        repository: &RepositoryConfig,
        http: &Client,
        max_response_bytes: usize,
    ) -> Result<String> {
        let installation_id = repository
            .installation_id
            .context("GitHub repository is missing installation_id")?;
        if let Ok(cache) = self.cache.lock() {
            let cache_key = (repository.api_url.clone(), installation_id);
            if let Some(entry) = cache.get(&cache_key) {
                if entry.expires_at > Utc::now() + ChronoDuration::seconds(60) {
                    return Ok(entry.token.clone());
                }
            }
        }
        let jwt = self.app_jwt()?;
        let url = endpoint(
            &repository.api_url,
            &[
                "app".to_owned(),
                "installations".to_owned(),
                installation_id.to_string(),
                "access_tokens".to_owned(),
            ],
        )?;
        let response = http
            .post(url)
            .header(header::ACCEPT, "application/json")
            .header(header::USER_AGENT, "app-manager-broker/0.1")
            .bearer_auth(jwt)
            .send()
            .await
            .context("request GitHub installation token")?;
        let status = response.status();
        let body = read_limited(response, max_response_bytes).await?;
        if !status.is_success() {
            bail!("GitHub installation token request was rejected");
        }
        let value: Value =
            serde_json::from_slice(&body).context("invalid GitHub installation token response")?;
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .context("GitHub installation token response is missing token")?
            .to_owned();
        let expires_at = value
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|| Utc::now() + ChronoDuration::minutes(9));
        if let Ok(mut cache) = self.cache.lock() {
            let cache_key = (repository.api_url.clone(), installation_id);
            cache.insert(
                cache_key,
                CachedInstallationToken {
                    token: token.clone(),
                    expires_at,
                },
            );
        }
        Ok(token)
    }
}

#[cfg(unix)]
fn validate_private_key_metadata(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let link_metadata = fs::symlink_metadata(path).context("stat GitHub private key file")?;
    if link_metadata.file_type().is_symlink() {
        bail!("GitHub private key path must not be a symlink");
    }
    let metadata = fs::metadata(path).context("stat GitHub private key file")?;
    if !metadata.is_file() {
        bail!("GitHub private key path is not a regular file");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("GitHub private key file must be owner-readable only");
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_key_metadata(path: &Path) -> Result<()> {
    let link_metadata = fs::symlink_metadata(path).context("stat GitHub private key file")?;
    if link_metadata.file_type().is_symlink() {
        bail!("GitHub private key path must not be a symlink");
    }
    let metadata = fs::metadata(path).context("stat GitHub private key file")?;
    if !metadata.is_file() {
        bail!("GitHub private key path is not a regular file");
    }
    Ok(())
}

fn value_to_id(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn run_url(value: &Value) -> Option<String> {
    ["html_url", "web_url", "url"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_owned))
}

fn run_from_value(
    value: &Value,
    expected_commit: &str,
    request_id: &str,
    require_correlation: bool,
) -> Option<ProviderRun> {
    let object = value.as_object()?;
    let run_commit = ["head_sha", "headSha", "sha", "commit_sha", "commitSha"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str).and_then(exact_sha))
        .or_else(|| {
            object
                .get("head_commit")
                .or_else(|| object.get("headCommit"))
                .and_then(|commit| {
                    ["id", "sha"].iter().find_map(|key| {
                        commit.get(*key).and_then(Value::as_str).and_then(exact_sha)
                    })
                })
        })?;
    if run_commit != expected_commit {
        return None;
    }
    if require_correlation && !json_contains_string(value, request_id) {
        return None;
    }
    let id = ["id", "run_id", "runId"]
        .iter()
        .find_map(|key| object.get(*key).and_then(value_to_id))?;
    let status = provider_status(
        object.get("status").and_then(Value::as_str),
        object.get("conclusion").and_then(Value::as_str),
    );
    Some(ProviderRun {
        id,
        status,
        url: run_url(value),
    })
}

fn json_contains_string(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(string) => string == needle || string.contains(needle),
        Value::Array(values) => values
            .iter()
            .any(|value| json_contains_string(value, needle)),
        Value::Object(values) => values
            .values()
            .any(|value| json_contains_string(value, needle)),
        _ => false,
    }
}

fn provider_status(status: Option<&str>, conclusion: Option<&str>) -> String {
    if let Some(conclusion) = conclusion {
        match conclusion.to_ascii_lowercase().as_str() {
            "success" => return "success".to_owned(),
            "failure" | "timed_out" | "startup_failure" => return "failure".to_owned(),
            "cancelled" | "canceled" => return "cancelled".to_owned(),
            "skipped" => return "skipped".to_owned(),
            _ => {}
        }
    }
    match status.unwrap_or_default().to_ascii_lowercase().as_str() {
        "queued" | "requested" | "waiting" | "pending" => "queued".to_owned(),
        "in_progress" | "in-progress" | "running" => "running".to_owned(),
        "success" => "success".to_owned(),
        "failure" | "failed" | "timed_out" | "startup_failure" => "failure".to_owned(),
        "cancelled" | "canceled" => "cancelled".to_owned(),
        "completed" => "unknown".to_owned(),
        _ => "pending".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_sha_requires_lower_or_upper_hex_and_exact_length() {
        let value = exact_sha(&"A".repeat(40)).expect("valid SHA");
        assert_eq!(value, "a".repeat(40));
        assert!(exact_sha("abc").is_none());
        assert!(exact_sha(&"g".repeat(40)).is_none());
    }

    #[test]
    fn signatures_are_hmac_sha256_and_constant_time_verified() {
        let body = br#"{"action":"push"}"#;
        let mut mac = HmacSha256::new_from_slice(b"secret").expect("HMAC key");
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        assert!(verify_signature(b"secret", body, &signature));
        assert!(!verify_signature(b"wrong", body, &signature));
    }

    #[test]
    fn state_transactions_keep_previous_backup() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(directory.path()).expect("open store");
        let previous = std::fs::read(directory.path().join("state.json")).expect("prior state");
        store
            .update(|state| {
                state.disabled_repositories.insert("repo".to_owned());
                Ok(())
            })
            .expect("write state");
        assert!(directory.path().join("state.json").exists());
        assert!(directory.path().join("state.json.bak").exists());
        assert_eq!(
            std::fs::read(directory.path().join("state.json.bak")).expect("backup state"),
            previous
        );
        assert!(store.is_disabled("repo").expect("read state"));
    }

    #[test]
    fn versionless_prior_state_migrates_on_next_transaction_without_startup_rewrite() {
        let directory = tempfile::tempdir().expect("tempdir");
        let legacy = br#"{"schema":0,"disabled_repositories":["legacy"]}"#;
        std::fs::write(directory.path().join("state.json"), legacy).expect("legacy state");
        let store = StateStore::open(directory.path()).expect("open legacy state");
        assert_eq!(store.snapshot().expect("snapshot").schema, STATE_SCHEMA);
        assert!(store.is_disabled("legacy").expect("legacy repository"));
        assert_eq!(
            std::fs::read(directory.path().join("state.json")).expect("original state"),
            legacy
        );
        store
            .update(|state| {
                state.disabled_repositories.insert("new".to_owned());
                Ok(())
            })
            .expect("migrate state transaction");
        let reopened = StateStore::open(directory.path()).expect("reopen migrated state");
        assert_eq!(
            reopened.snapshot().expect("migrated snapshot").schema,
            STATE_SCHEMA
        );
        assert!(reopened.is_disabled("new").expect("new repository"));
    }

    #[test]
    fn concurrent_store_instances_reread_latest_primary_under_process_lock() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = StateStore::open(directory.path()).expect("first store");
        let second = StateStore::open(directory.path()).expect("second store");
        first
            .update(|state| {
                state.disabled_repositories.insert("first".to_owned());
                Ok(())
            })
            .expect("first update");
        second
            .update(|state| {
                state.disabled_repositories.insert("second".to_owned());
                Ok(())
            })
            .expect("second update");
        let reopened = StateStore::open(directory.path()).expect("reopen store");
        let snapshot = reopened.snapshot().expect("snapshot");
        assert!(snapshot.disabled_repositories.contains("first"));
        assert!(snapshot.disabled_repositories.contains("second"));
    }

    #[test]
    fn corrupt_or_future_primary_is_rejected_without_backup_restore() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StateStore::open(directory.path()).expect("open store");
        store
            .update(|state| {
                state.disabled_repositories.insert("kept".to_owned());
                Ok(())
            })
            .expect("write state");
        let primary = directory.path().join("state.json");
        let backup = directory.path().join("state.json.bak");
        let backup_bytes = std::fs::read(&backup).expect("backup");
        let future = br#"{"schema":99,"disabled_repositories":[]}"#;
        std::fs::write(&primary, future).expect("future state");
        assert!(StateStore::open(directory.path()).is_err());
        assert_eq!(std::fs::read(&primary).expect("primary bytes"), future);
        assert_eq!(std::fs::read(&backup).expect("backup bytes"), backup_bytes);
        std::fs::remove_file(&primary).expect("remove primary for repair check");
        assert!(StateStore::open(directory.path()).is_err());
        assert_eq!(
            std::fs::read(&backup).expect("preserved backup"),
            backup_bytes
        );
    }

    #[test]
    fn unknown_top_level_state_fields_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let future = br#"{"schema":1,"unknownFutureField":true}"#;
        std::fs::write(directory.path().join("state.json"), future).expect("future state");
        assert!(StateStore::open(directory.path()).is_err());
        assert_eq!(
            std::fs::read(directory.path().join("state.json")).expect("state bytes"),
            future
        );
    }

    #[test]
    fn provider_status_never_turns_unknown_conclusion_into_success() {
        assert_eq!(
            provider_status(Some("completed"), Some("failure")),
            "failure"
        );
        assert_eq!(provider_status(Some("in_progress"), None), "running");
    }
}
