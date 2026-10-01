use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MachineProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub workspace_roots: Vec<String>,
}
fn default_port() -> u16 {
    22
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub path: String,
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub dirty: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionReport {
    pub status: String,
    pub message: String,
    pub workspaces: Vec<Workspace>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSource {
    pub id: String,
    pub name: String,
    pub manifest_url: String,
    pub artifact_base_url: String,
    pub receipt_url: String,
    #[serde(default = "stable")]
    pub channel: String,
    pub adapter: String,
    #[serde(default)]
    pub extension_uuid: Option<String>,
    #[serde(default)]
    pub gnome_versions: Vec<u32>,
}
fn stable() -> String {
    "stable".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub target: String,
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRelease {
    pub app_id: String,
    pub name: String,
    pub version: String,
    pub tag: String,
    pub commit: String,
    pub repository: String,
    pub channel: String,
    pub verified: bool,
    pub artifacts: Vec<Artifact>,
    pub fetched_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledApp {
    pub app_id: String,
    pub version: String,
    pub adapter: String,
    pub extension_uuid: Option<String>,
    pub artifact_sha256: String,
    pub retained_artifact: String,
    pub installed_at: String,
    #[serde(default)]
    pub previous: Option<Box<InstalledApp>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationReceipt {
    pub id: String,
    pub app_id: String,
    pub action: String,
    pub status: String,
    pub version: String,
    pub message: String,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrokerConnection {
    pub url: String,
    pub account: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryMapping {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub owner: String,
    pub repository: String,
    pub workflow: String,
    pub default_ref: String,
    pub installation_id: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildRecord {
    pub id: String,
    pub repository_id: String,
    pub commit: String,
    pub reference: String,
    pub provider: String,
    pub status: String,
    pub run_url: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostInfo {
    pub os: String,
    pub architecture: String,
    pub gnome_version: Option<u32>,
    pub data_directory: String,
    #[serde(default)]
    pub extensions_directory: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub schema: u32,
    #[serde(default)]
    pub machines: Vec<MachineProfile>,
    #[serde(default)]
    pub sources: Vec<AppSource>,
    #[serde(default)]
    pub releases: Vec<AppRelease>,
    #[serde(default)]
    pub installed: Vec<InstalledApp>,
    #[serde(default)]
    pub operations: Vec<OperationReceipt>,
    #[serde(default)]
    pub broker: Option<BrokerConnection>,
    #[serde(default)]
    pub repositories: Vec<RepositoryMapping>,
    #[serde(default)]
    pub builds: Vec<BuildRecord>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}
impl Default for AppState {
    fn default() -> Self {
        Self {
            schema: 1,
            machines: vec![],
            sources: vec![],
            releases: vec![],
            installed: vec![],
            operations: vec![],
            broker: None,
            repositories: vec![],
            builds: vec![],
            extra: Default::default(),
        }
    }
}
