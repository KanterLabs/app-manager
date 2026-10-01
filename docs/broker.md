# App Manager build broker

The broker is a small, self-hostable HTTP service that lets an App Manager
desktop client request a registered build workflow without giving the client a
provider token. It supports GitHub App installations and Gitea Actions. The
desktop bearer credential identifies an enrolled App Manager account; a GitHub
installation identifies the provider installation and is never treated as a
user identity.

## Configuration

Start the binary with `--config /etc/app-manager/broker.json`, or set
`APP_MANAGER_BROKER_CONFIG`. The state directory is created if it does not
exist. Existing state is loaded and never reset at startup. Each transaction
atomically replaces `state.json` and retains the previous file as
`state.json.bak`. State schema `1` is current; the versionless/schema-`0`
format is migrated in memory and upgraded only by the next successful
transaction. A future schema, corrupt or empty primary, or a missing primary
with a backup present stops startup and preserves both files for explicit
operator repair. The broker never silently restores a backup or resets state.
Before replacing the primary, it verifies that the backup is an exact byte
copy of the prior primary. A process lock at `state.lock` serializes writers,
and every snapshot read and writer rereads the latest primary while holding
that lock, so a second broker process observes revocations and new requests.

Each `accounts` entry contains a stable account name, a SHA-256 digest of the
bearer token (`tokenSha256`) or the name of an environment variable containing
that digest (`tokenSha256Env`), and a fixed list of repository IDs. For
small, isolated deployments, `tokenEnv` may name a protected environment
variable containing the raw token; the broker hashes it in memory and never
writes or logs it. Comparisons are constant-time. Repository IDs are the
authorization boundary and should not be reused for a different repository.

Every repository mapping supplies its provider API base, owner, repository,
registered workflow, default branch or tag, and installation details. The
workflow is configuration-owned; a build request can choose only a branch or
tag and a canonical commit. `apiUrl` is contacted only for that mapping. HTTPS
is required, with HTTP accepted only for loopback fixture servers. Redirects
are disabled so a provider cannot redirect a credential to another origin.

For GitHub, create a GitHub App and install it on the selected repositories.
The App must have these repository permissions:

* **Actions: Read and write** — dispatch the registered workflow and read its
  correlated run status.
* **Contents: Read-only** — resolve the selected branch or tag to its canonical
  commit.
* **Metadata: Read-only** — required by GitHub for repository metadata.

The App does not need organization administration, pull-request write access,
issues access, or arbitrary code-management permissions. Set `appId`, an
environment variable name for the PEM private key (`privateKeyEnv`), and an
optional webhook secret environment variable (`webhookSecretEnv`). A file
source (`privateKeyFile`) is also supported; on Unix it must be a regular file
with no group or world permissions (normally mode `0600`). PEM contents never
appear in configuration, state, responses, or logs. Each GitHub mapping carries
the numeric `installationId` for its installation.

For Gitea, create a token scoped to the mapped owner/repository with repository
read access and Actions workflow dispatch/read access. Put the token in a
protected environment variable and set that variable's name as
`giteaTokenEnv`. The value is never put in the JSON configuration. The token
is sent only to the mapping's configured Gitea `apiUrl`.

The receiver should be reachable from the provider over an isolated HTTPS
route, for example:

```
provider webhook -> reverse proxy /webhooks/github -> 127.0.0.1:8787
desktop         -> reverse proxy /api/*          -> 127.0.0.1:8787
```

Terminate TLS and enforce network policy in the reverse proxy or an isolated
service network. The broker itself listens on loopback by default; override
with `APP_MANAGER_BROKER_LISTEN` when a protected private interface is needed.
Configure the provider webhook for the raw receiver path
`/webhooks/github`, subscribe to installation, installation repositories,
repository, and workflow run events, and set the same HMAC secret in the
broker's protected environment variable. The broker validates the raw body
with HMAC-SHA256 before parsing it, limits body size, and durably deduplicates
the provider delivery ID.

Enrollment is intentionally separate from GitHub App installation. An
administrator should create the account token digest out of band, add an
account entry with an explicit repository allowlist, and deliver the raw
bearer token to the desktop through native credential storage. The broker
never accepts a provider installation as account authentication.

## HTTP contract

`/healthz` is the only unauthenticated route. All `/api/*` routes require
`Authorization: Bearer <account-token>`.

* `GET /api/repositories` returns the active `RepositoryMapping[]` for the
  authenticated account.
* `GET /api/status` returns `{githubConfigured, repositoryCount}` for that
  account.
* `GET /api/builds` returns that account's retained `BuildRecord[]` and
  refreshes a status only through a provider run correlated to the request.
* `POST /api/builds` accepts `{repositoryId, reference, commit}`. The commit
  must be exactly 40 hexadecimal characters. The broker resolves the branch or
  tag on the canonical provider first, requires an exact SHA match, persists
  the request, and sends the registered workflow inputs `expected_commit` and
  `request_id`.

The registered workflow should include `request_id` in its provider-visible
run name or equivalent run metadata, and should expose `expected_commit` in
the run's commit metadata where the provider supports it. This gives the
broker a durable correlation key after a dispatch API returns no run ID.

GitHub Actions and Gitea dispatch responses that cannot be classified because
of a timeout or connection failure are stored as `ambiguous`. They are never
retried blindly. A run is accepted for status tracking only when its provider
run ID came from the dispatch response or the provider's run listing contains
the request ID and the exact expected commit. A matching commit alone is not a
correlation. Build records do not claim that an artifact is a verified release;
release verification remains the release-tools and App Manager responsibility.

Installation revocation, suspension, repository removal, and repository
deletion events disable the affected mapping in durable state. Push and mirror
events are ignored and never dispatch builds.

## Example

See [`examples/broker-config.example.json`](../examples/broker-config.example.json)
for a secret-free configuration skeleton. It uses a loopback fixture URL so it
is suitable for local adapter tests only; replace it with the real HTTPS API
base in a protected deployment.
