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
use hmac::Mac;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Clone, Default)]
struct FixtureState {
    dispatches: Arc<Mutex<Vec<Value>>>,
    oversized_dispatch: bool,
    dispatch_with_run_id: bool,
}

fn digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn gitea_config(base: String, state_dir: &std::path::Path, token_env: &str) -> BrokerConfig {
    BrokerConfig {
        state_dir: state_dir.to_path_buf(),
        accounts: vec![AccountConfig {
            account: "operator".to_owned(),
            token_sha256: Some(digest("desktop-token")),
            token_sha256_env: None,
            token_env: None,
            repository_ids: vec!["fixture".to_owned()],
        }],
        repositories: vec![RepositoryConfig {
            id: "fixture".to_owned(),
            name: "Fixture repository".to_owned(),
            provider: ProviderKind::Gitea,
            owner: "acme".to_owned(),
            repository: "widget".to_owned(),
            workflow: "release.yaml".to_owned(),
            default_ref: "main".to_owned(),
            api_url: base,
            installation_id: None,
            gitea_token_env: Some(token_env.to_owned()),
            webhook_secret_env: None,
            enabled: true,
        }],
        github: None,
        http_timeout_secs: 3,
        max_response_bytes: 1024 * 1024,
        max_webhook_body_bytes: 1024 * 1024,
    }
}

async fn fixture(
    State(state): State<FixtureState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("token fixture-token")
    );
    match (method, uri.path()) {
        (Method::GET, "/repos/acme/widget/commits/main") => {
            Json(json!({"sha": COMMIT})).into_response()
        }
        (Method::POST, "/repos/acme/widget/actions/workflows/release.yaml/dispatches") => {
            let payload: Value = serde_json::from_slice(&body).expect("dispatch JSON");
            state.dispatches.lock().expect("fixture lock").push(payload);
            if state.oversized_dispatch {
                return (StatusCode::OK, "x".repeat(256)).into_response();
            }
            if !state.dispatch_with_run_id {
                return StatusCode::NO_CONTENT.into_response();
            }
            (
                StatusCode::OK,
                Json(json!({"id": 41, "html_url": "http://fixture/run/41"})),
            )
                .into_response()
        }
        (Method::GET, "/repos/acme/widget/actions/runs") => {
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
                        "head_sha": "ffffffffffffffffffffffffffffffffffffffff",
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
        (Method::GET, "/repos/acme/widget/actions/runs/41") => Json(json!({
            "id": 41,
            "head_sha": COMMIT,
            "status": "completed",
            "conclusion": "success",
            "html_url": "http://fixture/run/41"
        }))
        .into_response(),
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
    (format!("http://{}", address), task)
}

#[tokio::test]
async fn gitea_fixture_resolves_sha_dispatches_inputs_and_tracks_only_run() {
    let token_env = "APP_MANAGER_BROKER_FIXTURE_TOKEN_A";
    std::env::set_var(token_env, "fixture-token");
    let fixture_state = FixtureState {
        oversized_dispatch: false,
        dispatch_with_run_id: true,
        ..FixtureState::default()
    };
    let (base, task) = spawn_fixture(fixture_state.clone()).await;
    let directory = TempDir::new().expect("state dir");
    let broker =
        Broker::from_config(gitea_config(base, directory.path(), token_env)).expect("broker");
    let app = broker.router();

    let wrong = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/builds")
                .header("authorization", "Bearer desktop-token")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "repositoryId": "fixture",
                        "reference": "main",
                        "commit": "fedcba9876543210fedcba9876543210fedcba98"
                    })
                    .to_string(),
                ))
                .expect("wrong request"),
        )
        .await
        .expect("wrong response");
    assert_eq!(wrong.status(), StatusCode::CONFLICT);
    assert!(broker.store().snapshot().expect("state").builds.is_empty());

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/builds")
                .header("authorization", "Bearer desktop-token")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "repositoryId": "fixture",
                        "reference": "main",
                        "commit": COMMIT
                    })
                    .to_string(),
                ))
                .expect("build request"),
        )
        .await
        .expect("build response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("build body");
    let record: BuildRecord = serde_json::from_slice(&body).expect("build record");
    assert_eq!(record.status, "queued");
    assert_eq!(record.run_url.as_deref(), Some("http://fixture/run/41"));
    {
        let dispatches = fixture_state.dispatches.lock().expect("fixture lock");
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
                .expect("list request"),
        )
        .await
        .expect("list response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("list body");
    let records: Vec<BuildRecord> = serde_json::from_slice(&body).expect("build list");
    assert_eq!(records[0].status, "success");
    task.abort();
    std::env::remove_var(token_env);
}

#[tokio::test]
async fn gitea_fixture_correlates_listed_run_name_and_exact_branch_commit() {
    let token_env = "APP_MANAGER_BROKER_FIXTURE_TOKEN_C";
    std::env::set_var(token_env, "fixture-token");
    let fixture_state = FixtureState {
        oversized_dispatch: false,
        dispatch_with_run_id: false,
        ..FixtureState::default()
    };
    let (base, task) = spawn_fixture(fixture_state.clone()).await;
    let directory = TempDir::new().expect("state dir");
    let broker =
        Broker::from_config(gitea_config(base, directory.path(), token_env)).expect("broker");
    let app = broker.router();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/builds")
                .header("authorization", "Bearer desktop-token")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "repositoryId": "fixture",
                        "reference": "main",
                        "commit": COMMIT
                    })
                    .to_string(),
                ))
                .expect("build request"),
        )
        .await
        .expect("build response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("build body");
    let record: BuildRecord = serde_json::from_slice(&body).expect("build record");
    assert_eq!(record.status, "queued");
    assert_eq!(record.run_url, None);

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/builds")
                .header("authorization", "Bearer desktop-token")
                .body(axum::body::Body::empty())
                .expect("list request"),
        )
        .await
        .expect("list response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("list body");
    let records: Vec<BuildRecord> = serde_json::from_slice(&body).expect("build list");
    assert_eq!(records[0].status, "success");
    assert_eq!(records[0].run_url.as_deref(), Some("http://fixture/run/41"));
    assert_eq!(
        fixture_state.dispatches.lock().expect("fixture lock").len(),
        1
    );
    task.abort();
    std::env::remove_var(token_env);
}

#[tokio::test]
async fn dispatch_response_limit_marks_request_ambiguous_without_retry() {
    let token_env = "APP_MANAGER_BROKER_FIXTURE_TOKEN_B";
    std::env::set_var(token_env, "fixture-token");
    let fixture_state = FixtureState {
        oversized_dispatch: true,
        dispatch_with_run_id: true,
        ..FixtureState::default()
    };
    let (base, task) = spawn_fixture(fixture_state.clone()).await;
    let directory = TempDir::new().expect("state dir");
    let mut config = gitea_config(base, directory.path(), token_env);
    config.max_response_bytes = 64;
    let broker = Broker::from_config(config.clone()).expect("broker");
    let response = broker
        .router()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/builds")
                .header("authorization", "Bearer desktop-token")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "repositoryId": "fixture",
                        "reference": "main",
                        "commit": COMMIT
                    })
                    .to_string(),
                ))
                .expect("build request"),
        )
        .await
        .expect("build response");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("build body");
    let record: BuildRecord = serde_json::from_slice(&body).expect("build record");
    assert_eq!(record.status, "ambiguous");
    assert_eq!(broker.store().snapshot().expect("state").builds.len(), 1);
    assert_eq!(
        fixture_state.dispatches.lock().expect("fixture lock").len(),
        1
    );
    task.abort();
    std::env::remove_var(token_env);
}

#[tokio::test]
async fn github_webhook_deduplicates_and_disables_revoked_installation() {
    let secret_name = "APP_MANAGER_BROKER_FIXTURE_WEBHOOK_SECRET";
    std::env::set_var(secret_name, "webhook-secret");
    let directory = TempDir::new().expect("state dir");
    let config = BrokerConfig {
        state_dir: directory.path().to_path_buf(),
        accounts: vec![AccountConfig {
            account: "operator".to_owned(),
            token_sha256: Some(digest("desktop-token")),
            token_sha256_env: None,
            token_env: None,
            repository_ids: vec!["github".to_owned()],
        }],
        repositories: vec![RepositoryConfig {
            id: "github".to_owned(),
            name: "GitHub fixture".to_owned(),
            provider: ProviderKind::Github,
            owner: "acme".to_owned(),
            repository: "widget".to_owned(),
            workflow: "release.yml".to_owned(),
            default_ref: "main".to_owned(),
            api_url: "http://127.0.0.1:9".to_owned(),
            installation_id: Some(7),
            gitea_token_env: None,
            webhook_secret_env: None,
            enabled: true,
        }],
        github: Some(GithubAppConfig {
            app_id: 42,
            private_key_env: Some("APP_MANAGER_BROKER_UNUSED_KEY".to_owned()),
            private_key_file: None,
            private_key_path: None,
            webhook_secret_env: Some(secret_name.to_owned()),
        }),
        ..BrokerConfig::default()
    };
    let broker = Broker::from_config(config.clone()).expect("broker");
    let app = broker.router();
    let body = json!({
        "action": "deleted",
        "installation": {"id": 7}
    })
    .to_string();
    let mut mac = hmac::Hmac::<Sha256>::new_from_slice(b"webhook-secret").expect("HMAC");
    mac.update(body.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/webhooks/github")
                .header("x-hub-signature-256", &signature)
                .header("x-github-delivery", "delivery-1")
                .header("x-github-event", "installation")
                .body(axum::body::Body::from(body.clone()))
                .expect("webhook request"),
        )
        .await
        .expect("webhook response");
    assert_eq!(first.status(), StatusCode::OK);
    assert!(broker
        .store()
        .is_disabled("github")
        .expect("disabled state"));
    let second = Broker::from_config(config).expect("second broker");
    assert!(second
        .store()
        .is_disabled("github")
        .expect("revocation observed by second broker"));

    let duplicate = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/webhooks/github")
                .header("x-hub-signature-256", &signature)
                .header("x-github-delivery", "delivery-1")
                .header("x-github-event", "installation")
                .body(axum::body::Body::from(body))
                .expect("duplicate request"),
        )
        .await
        .expect("duplicate response");
    assert_eq!(duplicate.status(), StatusCode::OK);
    let body = to_bytes(duplicate.into_body(), 1024 * 1024)
        .await
        .expect("duplicate body");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("duplicate JSON")["duplicate"],
        true
    );
    std::env::remove_var(secret_name);
}
