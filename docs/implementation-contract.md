# Module contract

This is a shared work contract for the initial implementation. Rust models in
`crates/core/src/models.rs` serialize with camelCase. Do not change shared models
without coordinating with the primary agent.

## Core APIs

- `machines::validate_profile(&MachineProfile) -> anyhow::Result<()>`
- `machines::test_connection(&MachineProfile) -> async Result<ConnectionReport>`
- `machines::discover_workspaces(&MachineProfile) -> async Result<Vec<Workspace>>`
- `releases::validate_source(&AppSource) -> Result<()>`
- `releases::refresh(&AppSource) -> async Result<AppRelease>` consumes schema-1
  release-tools manifests and matching complete published receipts.
- `releases::select_artifact(&AppRelease, &AppSource, &HostInfo) -> Result<Artifact>`
- `installer::install(&Store, &AppSource, &AppRelease, &HostInfo) -> async Result<InstalledApp>`
- `installer::rollback(&Store, app_id: &str, &HostInfo) -> async Result<InstalledApp>`
- `Store::open(path: impl AsRef<Path>) -> Result<Store>` path is a directory.
- `Store::snapshot() -> Result<AppState>`
- `Store::update(|state: &mut AppState| -> Result<T>) -> Result<T>` transaction:
  reread under process/file lock, mutate, preserve unknown keys, durable replace.
- `Store::root() -> &Path`; artifacts are in `root()/artifacts`.

## Broker HTTP contract

All `/api/*` except `/healthz` require a configured bearer credential mapped to an
account and authorized repository IDs. Deployment authentication is separately
provisioned; a GitHub App installation alone never authenticates a user.

- `GET /api/repositories` -> `RepositoryMapping[]` for that account's configured
  allowlist; discovery may use GitHub installation access to verify membership.
- `POST /api/builds` `{repositoryId, reference, commit}` -> `BuildRecord`. Commit
  must be an exact 40-character SHA, ref resolved on canonical provider before
  dispatch. Send registered workflow inputs `expected_commit`, `request_id`.
- `GET /api/builds` -> `BuildRecord[]` for that account. Refresh provider status
  only when the real run can be correlated to the request.
- `GET /api/status` -> `{githubConfigured:boolean, repositoryCount:number}`.
- `POST /webhooks/github` accepts raw signed bodies, deduplicates delivery ID,
  processes installation revocation and repo access changes without dispatching
  builds from mirror pushes.

Broker credentials: injected environment or protected configuration only, never
in public product fixtures. Frontend broker bearer stored through native
credential storage by parent desktop adapter, absent from saved AppState.

## Desktop command bridge

- `get_snapshot` -> `{state: AppState, host: HostInfo}`
- `save_machine` `{profile: MachineProfile}` -> `AppState`
- `remove_machine` `{id:string}` -> `AppState`
- `test_machine` `{id:string}` -> `ConnectionReport`
- `discover_workspaces` `{id:string}` -> `Workspace[]`
- `save_source` `{source: AppSource}` -> `AppState`
- `remove_source` `{id:string}` -> `AppState`
- `refresh_releases` -> `AppState` (report individual source failures to user)
- `install_app` `{appId:string}` -> `AppState`
- `rollback_app` `{appId:string}` -> `AppState`
- `connect_broker` `{url:string, account:string, token:string}` -> `AppState`
- `disconnect_broker` -> `AppState`
- `refresh_broker` -> `AppState`
- `request_build` `{repositoryId:string, reference:string, commit:string}` -> `AppState`

Desktop UI is real-data by default and has an explicit separate demo mode. No
browser-facing shell/fs/http plugin or arbitrary executable command APIs.

## Concurrency

Parent owns root Cargo/package lockfiles, builds, integration, docs and workflows.
Workers do not run Cargo dependency-changing commands or repository-wide builds.
Test implementations can be delivered for parent to run centrally. Avoid
starting or claiming Helm tasks; parent already tracks INFRA-169.
