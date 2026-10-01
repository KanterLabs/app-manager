# CI and release packaging

The repository keeps the build commands in `ci/` so contributors can run the
same bounded checks used by automation. The workflows are wrappers around
those commands; they do not contain a second copy of the build logic.

From a clean checkout, install the locked dependencies and run the fast checks:

```bash
npm ci
npm ci --prefix web --ignore-scripts
ci/check metadata
ci/check frontend
ci/check rust
ci/check browser
```

For a portable heavy check, use the pinned Fedora 43 builder instead of
depending on host desktop libraries:

```bash
ci/container-build rust
ci/container-build package
```

The wrapper requires Docker or Podman-compatible tooling. It builds
`ci/Dockerfile.fedora`, whose Fedora base digest, Node 22.22.1 archive, Rust
1.96.0 toolchain, and tauri-driver 2.1.0 binary are pinned and checksum-
verified where applicable. Native package names are explicit, while DNF
selects their current Fedora 43 versions and the evidence records that they
are not version-pinned.
Each invocation bind-mounts only the checkout, gives Cargo/npm and the
container home a unique temporary path, and removes those generated caches on
exit. It never mounts a developer home, `~/.cargo`, credentials, or a host
Docker socket. Package output remains under the checkout's `dist/` tree; use
`ci/package` directly when a host setup needs a custom reviewed path below
that tree. The wrapper directs Cargo and Tauri to `target/fedora43`, keeping
the Fedora result separate from any host `target/` build.
Run the wrapper as a normal non-root account. It passes the caller's numeric
UID/GID into the disposable image, creates `appmanager` with those IDs, and
checks both the image account map and a matching `id -un`/`id -gn` instance
before running a build. Root callers are rejected so generated files cannot
become unexpectedly root-owned.

`ci/check all` runs the complete local set in sequence. `ci/check metadata`
parses shell, Python, JSON, and workflow files and rejects unmanaged runner
labels. It also requires workflow action references to use immutable commit
pins. `ci/check frontend` runs the web package's `typecheck` and `test` npm
scripts. `ci/check browser` runs the production `build` script and, when the
web package exposes a non-empty `e2e` script, its browser suite. Rust checks
run formatting, workspace compilation, tests, and Clippy with warnings denied.
None of these commands installs dependencies.

## Runner tiers and trust boundary

GitHub jobs use the KanterLabs runner labels from the infrastructure contract:

| Work | Runner | Reason |
| --- | --- | --- |
| Metadata and frontend typecheck/unit tests | `homelab` | Short, non-desktop checks |
| Rust tests, browser build, and desktop packaging | `homelab-heavy` | CPU, memory, and native toolchain demand |
| Aggregate gates | `homelab` | Result aggregation only |

No workflow uses `ubuntu-latest` or the broad `self-hosted` label. Heavy
workflow jobs invoke the Fedora builder wrapper, so the runner only needs the
container engine supplied by the homelab DinD contract. Jobs do not call
`apt`, `dnf`, `sudo`, or other host mutation commands. The `homelab` jobs still
use the runner's shared metadata/Node toolchain for short checks.

The public GitHub workflows run only for the `KanterLabs/app-manager` mirror.
Gitea remains the canonical KanterLabs source and deployment boundary. A fork
pull request is skipped before it reaches the homelab pool; the commands above
remain the supported local reproduction path. Same-repo PR runs may cancel an
older run. Release candidates use a repository/version concurrency key and
never cancel an in-progress candidate. No CI job receives publication,
deployment, or infrastructure credentials.

## Native Linux packaging

`ci/package` calls the root `tauri` npm script and requests `rpm,deb` by
default. Set `TAURI_BUNDLES` only when deliberately narrowing a local
diagnostic build. The RPM is required because the release is Fedora-targeted;
the Debian bundle is emitted when the Tauri configuration and available native
toolchain support it. If the default build runs on a host without `dpkg-deb`,
the wrapper records a warning and requests the required RPM target only; an
explicit `TAURI_BUNDLES=rpm,deb` request fails when its Debian helper is
missing. If the configured target directory contains all exact release
executables and the reviewed setup tree, the wrapper creates a companion Linux
`.tar.gz` archive containing `app-manager-desktop`, `appctl`,
`app-manager-broker`, `LICENSE`, `README.md`, `CONTRIBUTING.md`, and the
complete `docs/` and `examples/` trees. The CLI and broker are built with
`cargo build --release -p app-manager-cli -p app-manager-broker --locked`
before the Tauri build, and the archive only accepts those exact executable
names and document inventory.

The Fedora builder installs and verifies these package names:

```text
gcc-c++ make pkgconf-pkg-config openssl-devel gtk3-devel
webkit2gtk4.1-devel libsoup3-devel librsvg2-devel
libappindicator-gtk3-devel dbus-daemon dbus-devel dbus-tools
openssl webkitgtk6.0 rpm-build python3-pyyaml git openssh-clients openssh-server
xorg-x11-server-Xvfb file tar unzip gzip xz
```

The disposable `appmanager` account has the literal `*` shadow marker, which
cannot be used as a password without marking the account as locked. Its SSH
fixture disables `PasswordAuthentication` and keyboard-interactive login;
native smoke tests still supply their own loopback key fixture when needed.

The Tauri bundler remains the authority for RPM/DEB helper requirements. The
workflow does not require `rpmbuild`, a desktop session, or a system package
manager on the host; a missing dependency is reported by `pkg-config` or the
Tauri build with an actionable failure. Native UI smoke commands can use the
Fedora `WebKitWebDriver` supplied by `webkitgtk6.0`; Fedora does not publish a
separate `webkit2gtk4.1driver` package. The builder also installs the pinned
`tauri-driver` 2.1.0 binary; the portable package wrapper runs
`tests/desktop-smoke.py` against the assembled release binary under a private
`:99` Xvfb display and `dbus-run-session`. Workflow artifact upload follows
that native smoke step, so an uploaded candidate has passed both packaging and
runtime checks.

The package wrapper stages all files in a private `mktemp` directory and moves
them into a new subdirectory under `dist/`. It refuses to overwrite an existing
output directory, manifest, or evidence file. For a second local run, choose a
fresh reviewed path and matching metadata paths, for example:

```bash
OUTPUT_DIR=dist/release-local-2 \
RELEASE_MANIFEST_PATH=dist/release-manifest-local-2.json \
PACKAGE_EVIDENCE_PATH=dist/package-evidence-local-2.json \
ci/package
```

Each successful run emits:

* `dist/.../SHA256SUMS`, with one SHA-256 record per package;
* a schema-1 `release-manifest.json` containing exact source commit, version,
  tag, target, size, and SHA-256 values;
* `package-evidence.json`, a machine-readable record that points to the
  manifest, repeats the exact `source_commit`, records `manifest_sha256`, and
  states that the bundle is assembled but unpublished. It also records the
  observed OS/toolchain, pinned builder contract, runtime smoke tools, and
  portable archive members. Fedora native package names are recorded as
  prerequisites with `versions_pinned: false` because DNF repository updates
  remain mutable.

`ci/release-manifest.py` is a dependency-free contract adapter for the shared
release-tools schema. When infrastructure supplies a reviewed, pinned
`RELEASE_TOOLS` path, `ci/package` also asks that tool to verify the manifest;
the repository does not copy unpublished moving infrastructure source. The
adapter refuses symlinks, path escapes, missing targets, extra files, and hash
or size mismatches.

## Release candidate flow

The public-mirror release workflow accepts a `v*.*.*` tag or a broker-compatible
manual dispatch from the protected `main` revision. Manual dispatch supplies
the required `expected_commit` and `request_id` inputs; the version is derived
from the checked-out workspace. The workflow checks that the selected commit
is the checkout and is an ancestor of the protected `main` history before any
repository script runs, validates the tag version when a tag triggered the run,
and then assembles the same package evidence on `homelab-heavy`. The run name
contains the request ID so broker polling can correlate the build. The
workflow uploads the bundle for review and retains it for 14 days. It has no
publish step, no release token, and no deployment action.

Canonical Gitea publication, durable retention, forge receipts, and
public-download verification remain the existing INFRA164 integration. That integration must
promote the exact retained bytes after checking `release-manifest.json` and
`SHA256SUMS`, and its complete published receipt must carry the matching
`source_commit` and `manifest_sha256`. Receipt completeness and artifact hash
evidence are the handoff gate; a `finalized` flag alone is not sufficient for
an already-published retry. A failed or partial publication is retried from
retained output, not by rebuilding mutable source.
