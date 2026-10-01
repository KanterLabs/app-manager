use app_manager_core::installer;
use app_manager_core::models::{AppSource, HostInfo};
use app_manager_core::releases;
use app_manager_core::store::Store;
use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

const APP_ID: &str = "demo";
const EXTENSION_UUID: &str = "demo@example.test";
const ARTIFACT_NAME: &str = "demo.shell-extension.zip";
const COMMIT_V1: &str = "1111111111111111111111111111111111111111";
const COMMIT_V2: &str = "2222222222222222222222222222222222222222";
const COMMIT_V3: &str = "3333333333333333333333333333333333333333";
const COMMIT_V4: &str = "4444444444444444444444444444444444444444";
const COMMIT_V5: &str = "5555555555555555555555555555555555555555";

#[derive(Clone)]
struct PublishedRelease {
    manifest: Vec<u8>,
    receipt: Vec<u8>,
    artifact: Vec<u8>,
}

type FixtureState = Arc<RwLock<PublishedRelease>>;

struct RunningFixture {
    base: String,
    state: FixtureState,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

async fn manifest(State(state): State<FixtureState>) -> Response {
    json_response(state.read().await.manifest.clone())
}

async fn receipt(State(state): State<FixtureState>) -> Response {
    json_response(state.read().await.receipt.clone())
}

async fn artifact_response(State(state): State<FixtureState>) -> Response {
    let mut response = state.read().await.artifact.clone().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    response
}

fn json_response(bytes: Vec<u8>) -> Response {
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn fixture(initial: PublishedRelease) -> RunningFixture {
    let state = Arc::new(RwLock::new(initial));
    let listener = tokio::net::TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let app = Router::new()
        .route("/manifest.json", get(manifest))
        .route("/receipt.json", get(receipt))
        .route("/assets/demo.shell-extension.zip", get(artifact_response))
        .with_state(state.clone());
    let (shutdown, shutdown_receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_receiver.await;
            })
            .await
            .expect("fixture server");
    });
    RunningFixture {
        base: format!("http://127.0.0.1:{}", address.port()),
        state,
        shutdown,
        server,
    }
}

fn source(base: &str) -> AppSource {
    AppSource {
        id: APP_ID.into(),
        name: "Demo extension".into(),
        manifest_url: format!("{base}/manifest.json"),
        artifact_base_url: format!("{base}/assets/"),
        receipt_url: format!("{base}/receipt.json"),
        channel: "stable".into(),
        adapter: "gnome-extension".into(),
        extension_uuid: Some(EXTENSION_UUID.into()),
        gnome_versions: vec![50],
    }
}

fn host(root: &Path) -> HostInfo {
    HostInfo {
        os: "linux".into(),
        architecture: "x86_64".into(),
        gnome_version: Some(50),
        data_directory: root.join("app-data").to_string_lossy().into_owned(),
        extensions_directory: root
            .join("gnome-shell/extensions")
            .to_string_lossy()
            .into_owned(),
    }
}

fn extension_zip(version: u64, malicious: bool) -> Vec<u8> {
    let mut output = std::io::Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut output);
        writer
            .start_file("metadata.json", SimpleFileOptions::default())
            .expect("metadata entry");
        writer
            .write_all(
                serde_json::to_string(&serde_json::json!({
                    "uuid": EXTENSION_UUID,
                    "version": version,
                    "shell-version": ["50"]
                }))
                .expect("metadata JSON")
                .as_bytes(),
            )
            .expect("metadata");
        writer
            .start_file("extension.js", SimpleFileOptions::default())
            .expect("extension entry");
        writer
            .write_all(format!("// version {version}\n").as_bytes())
            .expect("extension");
        if malicious {
            writer
                .start_file("../escape", SimpleFileOptions::default())
                .expect("malicious entry");
            writer.write_all(b"escape").expect("malicious content");
        }
        writer.finish().expect("ZIP finish");
    }
    output.into_inner()
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn published(
    version: &str,
    commit: &str,
    artifact_bytes: Vec<u8>,
    receipt_complete: bool,
    digest_override: Option<&str>,
) -> PublishedRelease {
    let artifact_hash = digest_override
        .map(str::to_owned)
        .unwrap_or_else(|| digest(&artifact_bytes));
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "project": APP_ID,
        "repository": "owner/demo",
        "release": {
            "version": version,
            "tag": format!("v{version}"),
            "commit": commit
        },
        "protected_assets": [ARTIFACT_NAME],
        "targets": [{
            "target": "gnome-extension",
            "path": format!("dist/{ARTIFACT_NAME}"),
            "name": ARTIFACT_NAME,
            "size": artifact_bytes.len(),
            "sha256": artifact_hash,
            "protected": true
        }]
    }))
    .expect("manifest JSON");
    let manifest_hash = digest(&manifest);
    let receipt = if receipt_complete {
        serde_json::json!({
            "ok": true,
            "command": "ensure-release",
            "dry_run": false,
            "created": false,
            "finalized": false,
            "draft": false,
            "public": true,
            "repository": "owner/demo",
            "tag": format!("v{version}"),
            "source_commit": commit,
            "manifest_sha256": manifest_hash,
            "assets": [{
                "name": ARTIFACT_NAME,
                "sha256": artifact_hash,
                "size": artifact_bytes.len(),
                "verified": true,
                "public": true,
                "verification": "download"
            }],
            "manifest_asset": {
                "name": "release-manifest.json",
                "sha256": manifest_hash,
                "size": manifest.len(),
                "verified": true,
                "public": true,
                "verification": "download"
            }
        })
    } else {
        serde_json::json!({
            "ok": true,
            "command": "ensure-release",
            "dry_run": false,
            "draft": false,
            "public": true,
            "repository": "owner/demo",
            "tag": format!("v{version}"),
            "source_commit": commit,
            "manifest_sha256": manifest_hash,
            "assets": []
        })
    };
    PublishedRelease {
        manifest,
        receipt: serde_json::to_vec(&receipt).expect("receipt JSON"),
        artifact: artifact_bytes,
    }
}

fn state_bytes(store: &Store) -> Vec<u8> {
    serde_json::to_vec(&store.snapshot().expect("state snapshot")).expect("state JSON")
}

fn directory_entries(path: &Path) -> Vec<String> {
    let mut entries = fs::read_dir(path)
        .map(|entries| {
            entries
                .map(|entry| {
                    entry
                        .expect("directory entry")
                        .file_name()
                        .to_string_lossy()
                        .into()
                })
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    entries.sort();
    entries
}

#[tokio::test]
async fn refresh_install_update_rollback_and_rejection_preserve_state() {
    let root = tempfile::tempdir().expect("fixture root");
    let initial = published("1.0.0", COMMIT_V1, extension_zip(1, false), true, None);
    let fixture = fixture(initial).await;
    let source = source(&fixture.base);
    let host = host(root.path());
    let store_root = root.path().join("store");
    let store = Store::open(&store_root).expect("store");
    let extension_dir = Path::new(&host.extensions_directory).join(EXTENSION_UUID);

    let release_v1 = releases::refresh(&source).await.expect("refresh v1");
    let installed_v1 = installer::install(&store, &source, &release_v1, &host)
        .await
        .expect("install v1");
    assert_eq!(installed_v1.version, "1.0.0");
    assert_eq!(store.snapshot().expect("state").installed.len(), 1);
    assert!(installed_v1.previous.is_none());
    assert_eq!(
        fs::read(extension_dir.join("extension.js")).expect("v1 extension"),
        b"// version 1\n"
    );
    assert!(Path::new(&installed_v1.retained_artifact).is_file());

    fs::write(extension_dir.join("settings.json"), b"{\"favorite\":true}").expect("user settings");
    let before_update_entries = directory_entries(extension_dir.parent().expect("extensions"));

    *fixture.state.write().await =
        published("2.0.0", COMMIT_V2, extension_zip(2, false), true, None);
    let release_v2 = releases::refresh(&source).await.expect("refresh v2");
    let installed_v2 = installer::install(&store, &source, &release_v2, &host)
        .await
        .expect("install v2");
    assert_eq!(installed_v2.version, "2.0.0");
    assert_eq!(
        installed_v2
            .previous
            .as_deref()
            .expect("previous v1")
            .version,
        "1.0.0"
    );
    assert!(installed_v2.previous.as_deref().unwrap().previous.is_none());
    assert_eq!(
        fs::read(extension_dir.join("extension.js")).expect("v2 extension"),
        b"// version 2\n"
    );
    assert_eq!(
        fs::read(extension_dir.join("settings.json")).expect("preserved settings"),
        b"{\"favorite\":true}"
    );
    assert!(Path::new(
        &installed_v2
            .previous
            .as_deref()
            .expect("previous v1")
            .retained_artifact
    )
    .is_file());
    assert_eq!(
        digest(
            &fs::read(
                &installed_v2
                    .previous
                    .as_deref()
                    .expect("previous v1")
                    .retained_artifact,
            )
            .expect("retained v1")
        ),
        installed_v2
            .previous
            .as_deref()
            .expect("previous v1")
            .artifact_sha256
    );

    let rolled_back = installer::rollback(&store, APP_ID, &host)
        .await
        .expect("rollback v1");
    assert_eq!(rolled_back.version, "1.0.0");
    assert_eq!(
        rolled_back
            .previous
            .as_deref()
            .expect("previous v2")
            .version,
        "2.0.0"
    );
    assert!(rolled_back.previous.as_deref().unwrap().previous.is_none());
    assert_eq!(
        fs::read(extension_dir.join("extension.js")).expect("rolled back extension"),
        b"// version 1\n"
    );
    assert_eq!(
        fs::read(extension_dir.join("settings.json")).expect("rolled back settings"),
        b"{\"favorite\":true}"
    );
    assert_eq!(
        directory_entries(extension_dir.parent().expect("extensions")),
        before_update_entries
    );

    let before_bad_state = state_bytes(&store);
    let before_bad_entries = directory_entries(extension_dir.parent().expect("extensions"));
    let before_bad_extension = fs::read(extension_dir.join("extension.js")).expect("extension");
    *fixture.state.write().await =
        published("3.0.0", COMMIT_V3, extension_zip(3, true), true, None);
    let release_v3 = releases::refresh(&source)
        .await
        .expect("refresh malicious ZIP");
    assert!(installer::install(&store, &source, &release_v3, &host)
        .await
        .is_err());
    assert_eq!(state_bytes(&store), before_bad_state);
    assert_eq!(
        directory_entries(extension_dir.parent().expect("extensions")),
        before_bad_entries
    );
    assert_eq!(
        fs::read(extension_dir.join("extension.js")).expect("unchanged extension"),
        before_bad_extension
    );

    *fixture.state.write().await =
        published("4.0.0", COMMIT_V4, extension_zip(4, false), false, None);
    assert!(releases::refresh(&source).await.is_err());
    assert_eq!(state_bytes(&store), before_bad_state);
    assert_eq!(
        directory_entries(extension_dir.parent().expect("extensions")),
        before_bad_entries
    );

    let wrong_hash = "0".repeat(64);
    let digest_mismatch = published(
        "5.0.0",
        COMMIT_V5,
        extension_zip(5, false),
        true,
        Some(&wrong_hash),
    );
    *fixture.state.write().await = digest_mismatch;
    let release_v5 = releases::refresh(&source)
        .await
        .expect("refresh digest mismatch");
    assert!(installer::install(&store, &source, &release_v5, &host)
        .await
        .is_err());
    assert_eq!(state_bytes(&store), before_bad_state);
    assert_eq!(
        directory_entries(extension_dir.parent().expect("extensions")),
        before_bad_entries
    );

    let _ = fixture.shutdown.send(());
    fixture.server.await.expect("fixture shutdown");
}
