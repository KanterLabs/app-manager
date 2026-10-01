use app_manager_broker::{
    AccountConfig, Broker, BrokerConfig, GithubAppConfig, ProviderKind, RepositoryConfig,
};
use app_manager_core::models::BuildRecord;
use axum::body::{to_bytes, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use chrono::{Duration as ChronoDuration, Utc};
use hmac::Mac;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::time::{timeout, Duration};
use tower::ServiceExt;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const WRONG_COMMIT: &str = "ffffffffffffffffffffffffffffffffffffffff";

#[derive(Clone, Default)]
struct Barrier {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[derive(Clone, Default)]
struct FixtureState {
    verification_key: Arc<Vec<u8>>,
    token_calls: Arc<AtomicUsize>,
    dispatches: Arc<Mutex<Vec<Value>>>,
    barrier: Option<Barrier>,
    expire_tokens: bool,
}

#[derive(Debug, Deserialize)]
struct AppClaims {
    iat: i64,
    exp: i64,
    iss: u64,
}

fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn disposable_rsa_key() -> (String, String) {
    let output = Command::new("openssl")
        .args(["genrsa", "2048"])
        .output()
        .expect("openssl is required for the GitHub fixture key");
    assert!(
        output.status.success(),
        "openssl genrsa failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let private_key = String::from_utf8(output.stdout).expect("openssl emitted PEM");
    let directory = TempDir::new().expect("key tempdir");
    let private_path = directory.path().join("fixture-key.pem");
    fs::write(&private_path, &private_key).expect("write fixture key");
    let output = Command::new("openssl")
        .args(["rsa", "-in"])
        .arg(&private_path)
        .args(["-pubout"])
        .output()
        .expect("openssl is required for the GitHub fixture public key");
    assert!(
        output.status.success(),
        "openssl rsa -pubout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        private_key,
        String::from_utf8(output.stdout).expect("openssl emitted public PEM"),
    )
}

fn config(
    base: String,
    state_dir: &std::path::Path,
    private_key_env: &str,
    webhook_secret_env: &str,
) -> BrokerConfig {
    BrokerConfig {
        state_dir: state_dir.to_path_buf(),
        accounts: vec![
            AccountConfig {
                account: "operator".to_owned(),
                token_sha256: Some(digest("desktop-token")),
                token_sha256_env: None,
                token_env: None,
                repository_ids: vec!["github-fixture".to_owned()],
            },
            AccountConfig {
                account: "other-account".to_owned(),
                token_sha256: Some(digest("other-token")),
                token_sha256_env: None,
                token_env: None,
                repository_ids: Vec::new(),
            },
        ],
        repositories: vec![RepositoryConfig {
            id: "github-fixture".to_owned(),
            name: "GitHub fixture".to_owned(),
            provider: ProviderKind::Github,
            owner: "acme".to_owned(),
            repository: "widget".to_owned(),
            workflow: "release.yml".to_owned(),
            default_ref: "main".to_owned(),
            api_url: base,
            installation_id: Some(17),
            gitea_token_env: None,
            webhook_secret_env: None,
            enabled: true,
        }],
        github: Some(GithubAppConfig {
            app_id: 42,
            private_key_env: Some(private_key_env.to_owned()),
            private_key_file: None,
            private_key_path: None,
            webhook_secret_env: Some(webhook_secret_env.to_owned()),
        }),
        http_timeout_secs: 5,
        max_response_bytes: 1024 * 1024,
        max_webhook_body_bytes: 1024 * 1024,
    }
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_else(|| panic!("missing provider bearer authorization"))
        .to_owned()
}

fn verify_app_jwt(state: &FixtureState, token: &str) {
    let validation = Validation::new(Algorithm::RS256);
    let claims = decode::<AppClaims>(
        token,
        &DecodingKey::from_rsa_pem(&state.verification_key).expect("fixture RSA key"),
        &validation,
    )
    .expect("valid GitHub App JWT")
    .claims;
    let now = Utc::now().timestamp();
    assert_eq!(claims.iss, 42);
    assert!(claims.iat <= now);
    assert!(claims.exp > now);
    assert!(claims.exp - claims.iat <= 10 * 60);
}

fn verify_installation_token(headers: &HeaderMap) {
    assert!(bearer(headers).starts_with("installation-token-"));
}

async fn fixture(
    State(state): State<FixtureState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match (method, uri.path()) {
        (Method::POST, "/app/installations/17/access_tokens") => {
            verify_app_jwt(&state, &bearer(&headers));
            let call = state.token_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let expires_at = if state.expire_tokens {
                Utc::now() - ChronoDuration::seconds(1)
            } else {
                Utc::now() + ChronoDuration::minutes(5)
            };
            (
                StatusCode::OK,
                Json(json!({
                    "token": format!("installation-token-{call}"),
                    "expires_at": expires_at.to_rfc3339()
                })),
            )
                .into_response()
        }
        (Method::GET, "/repos/acme/widget/commits/main") => {
            verify_installation_token(&headers);
            if let Some(barrier) = &state.barrier {
                barrier.started.notify_one();
                if timeout(Duration::from_secs(10), barrier.release.notified())
                    .await
                    .is_err()
                {
                    return StatusCode::GATEWAY_TIMEOUT.into_response();
                }
            }
            Json(json!({"sha": COMMIT})).into_response()
        }
        (Method::POST, "/repos/acme/widget/actions/workflows/release.yml/dispatches") => {
            verify_installation_token(&headers);
            let payload: Value = serde_json::from_slice(&body).expect("dispatch JSON");
            state.dispatches.lock().expect("fixture lock").push(payload);
            StatusCode::NO_CONTENT.into_response()
        }
        (Method::GET, "/repos/acme/widget/actions/workflows/release.yml/runs") => {
            verify_installation_token(&headers);
            let request_id = state
                .dispatches
                .lock()
                .expect("fixture lock")
                .last()
                .and_then(|dispatch| dispatch.get("inputs"))
                .and_then(|inputs| inputs.get("request_id"))
                .and_then(Value::as_str)
                .expect("request id in dispatch")
                .to_owned();
            Json(json!({
                "workflow_runs": [
                    {
                        "id": 40,
                        "head_sha": WRONG_COMMIT,
                        "status": "completed",
                        "conclusion": "success",
                        "run_name": format!("release {request_id}"),
                        "html_url": "http://fixture/run/40"
                    },
                    {
                        "id": 41,
                        "head_sha": COMMIT,
                        "status": "completed",
                        "conclusion": "success",
                        "run_name": format!("release {request_id}"),
                        "html_url": "http://fixture/run/41"
                    }
                ]
            }))
            .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn spawn_fixture(state: FixtureState) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture bind");
    let address = listener.local_addr().expect("fixture address");
    let router = Router::new().fallback(any(fixture)).with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("fixture serve");
    });
    (format!("http://{address}"), task)
}

fn build_request(token: &str, commit: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/api/builds")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({
                "repositoryId": "github-fixture",
                "reference": "main",
                "commit": commit
            })
            .to_string(),
        ))
        .expect("build request")
}

#[tokio::test]
async fn github_fixture_verifies_jwt_caches_installation_token_and_tracks_correlated_run() {
    let private_key_env = "APP_MANAGER_BROKER_GITHUB_FIXTURE_KEY_A";
    let webhook_secret_env = "APP_MANAGER_BROKER_GITHUB_FIXTURE_SECRET_A";
    let (private_key, public_key) = disposable_rsa_key();
    std::env::set_var(private_key_env, private_key);
    std::env::set_var(webhook_secret_env, "webhook-secret-a");
    let state = FixtureState {
        verification_key: Arc::new(public_key.into_bytes()),
        ..FixtureState::default()
    };
    let (base, task) = spawn_fixture(state.clone()).await;
    let directory = TempDir::new().expect("state dir");
    let broker = Broker::from_config(config(
        base,
        directory.path(),
        private_key_env,
        webhook_secret_env,
    ))
    .expect("broker");
    let app = broker.router();

    let wrong = app
        .clone()
        .oneshot(build_request("desktop-token", WRONG_COMMIT))
        .await
        .expect("wrong SHA response");
    assert_eq!(wrong.status(), StatusCode::CONFLICT);
    assert!(state.dispatches.lock().expect("fixture lock").is_empty());

    let denied = app
        .clone()
        .oneshot(build_request("other-token", COMMIT))
        .await
        .expect("denied response");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert!(state.dispatches.lock().expect("fixture lock").is_empty());

    let response = app
        .clone()
        .oneshot(build_request("desktop-token", COMMIT))
        .await
        .expect("build response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("build body");
    let record: BuildRecord = serde_json::from_slice(&body).expect("build record");
    assert_eq!(record.status, "queued");
    {
        let dispatches = state.dispatches.lock().expect("fixture lock");
        assert_eq!(dispatches.len(), 1);
        assert_eq!(dispatches[0]["ref"], "main");
        assert_eq!(dispatches[0]["inputs"]["expected_commit"], COMMIT);
        assert_eq!(dispatches[0]["inputs"]["request_id"], record.id);
    }

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/builds")
                .header("authorization", "Bearer desktop-token")
                .body(axum::body::Body::empty())
                .expect("build list request"),
        )
        .await
        .expect("build list response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("build list body");
    let records: Vec<BuildRecord> = serde_json::from_slice(&body).expect("build list");
    assert_eq!(records[0].status, "success");
    assert_eq!(records[0].run_url.as_deref(), Some("http://fixture/run/41"));
    assert_eq!(state.token_calls.load(Ordering::SeqCst), 1);

    task.abort();
    std::env::remove_var(private_key_env);
    std::env::remove_var(webhook_secret_env);
}

#[tokio::test]
async fn github_fixture_does_not_reuse_expired_installation_tokens() {
    let private_key_env = "APP_MANAGER_BROKER_GITHUB_FIXTURE_KEY_EXPIRED";
    let webhook_secret_env = "APP_MANAGER_BROKER_GITHUB_FIXTURE_SECRET_EXPIRED";
    let (private_key, public_key) = disposable_rsa_key();
    std::env::set_var(private_key_env, private_key);
    std::env::set_var(webhook_secret_env, "webhook-secret-expired");
    let state = FixtureState {
        verification_key: Arc::new(public_key.into_bytes()),
        expire_tokens: true,
        ..FixtureState::default()
    };
    let (base, task) = spawn_fixture(state.clone()).await;
    let directory = TempDir::new().expect("state dir");
    let broker = Broker::from_config(config(
        base,
        directory.path(),
        private_key_env,
        webhook_secret_env,
    ))
    .expect("broker");
    let response = broker
        .router()
        .oneshot(build_request("desktop-token", COMMIT))
        .await
        .expect("build response");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(state.token_calls.load(Ordering::SeqCst) >= 2);
    task.abort();
    std::env::remove_var(private_key_env);
    std::env::remove_var(webhook_secret_env);
}

#[tokio::test]
async fn github_revocation_during_reference_lookup_blocks_dispatch() {
    let private_key_env = "APP_MANAGER_BROKER_GITHUB_FIXTURE_KEY_B";
    let webhook_secret_env = "APP_MANAGER_BROKER_GITHUB_FIXTURE_SECRET_B";
    let (private_key, public_key) = disposable_rsa_key();
    std::env::set_var(private_key_env, private_key);
    std::env::set_var(webhook_secret_env, "webhook-secret-b");
    let barrier = Barrier::default();
    let state = FixtureState {
        verification_key: Arc::new(public_key.into_bytes()),
        barrier: Some(barrier.clone()),
        ..FixtureState::default()
    };
    let (base, task) = spawn_fixture(state.clone()).await;
    let directory = TempDir::new().expect("state dir");
    let broker_config = config(base, directory.path(), private_key_env, webhook_secret_env);
    let broker = Broker::from_config(broker_config).expect("broker");
    let app = broker.router();
    let build_task = tokio::spawn({
        let app = app.clone();
        async move {
            app.oneshot(build_request("desktop-token", COMMIT))
                .await
                .expect("build response")
        }
    });
    timeout(Duration::from_secs(10), barrier.started.notified())
        .await
        .expect("reference fixture reached revocation barrier");

    let body = json!({
        "action": "deleted",
        "installation": {"id": 17}
    })
    .to_string();
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(b"webhook-secret-b").expect("HMAC");
    mac.update(body.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let webhook = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/webhooks/github")
                .header("x-hub-signature-256", signature)
                .header("x-github-delivery", "github-fixture-revocation")
                .header("x-github-event", "installation")
                .body(axum::body::Body::from(body))
                .expect("webhook request"),
        )
        .await
        .expect("webhook response");
    assert_eq!(webhook.status(), StatusCode::OK);
    barrier.release.notify_one();

    let build = build_task.await.expect("build task");
    assert_eq!(build.status(), StatusCode::FORBIDDEN);
    assert!(state.dispatches.lock().expect("fixture lock").is_empty());
    task.abort();
    std::env::remove_var(private_key_env);
    std::env::remove_var(webhook_secret_env);
}
