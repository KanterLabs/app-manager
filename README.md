# App Manager

An open-source desktop app for bringing verified CI releases onto your laptop,
requesting builds, and keeping your development VM connections in one place.

App Manager is an installed Tauri application. Its Rust core performs local
operations, its React interface presents their results, and the `appctl` CLI
uses the same core. An optional self-hosted broker integrates GitHub Apps and
Gitea Actions without putting provider credentials on the laptop.

## First version

- **Apps:** configure release sources, inspect verified publication receipts
  and SHA-256 digests, install GNOME Shell extension ZIPs, and roll back using
  a retained previous artifact.
- **Machines:** save SSH hosts or config aliases, test connections, and discover
  Git workspaces with their branch, exact commit, and dirty state.
- **Repositories and Builds:** connect an enrolled broker account, see its
  allowed repositories, request a registered workflow for an exact commit,
  and track a correlated provider run.
- **Settings:** connect or disconnect the optional broker. The broker account
  credential is stored in the native desktop keyring.

This is an early Linux version. GNOME extensions are the implemented install
adapter; RPM, Flatpak, macOS, and Windows app installation are future work.
The desktop app itself packages as an RPM, DEB, and Linux binary archive.
Installing an extension updates its files; enabling it and any session restart
remain GNOME desktop operations. Existing unmanaged extensions are preserved
and require an explicit migration before App Manager can replace them.

The app starts empty. It never enrolls a machine, dispatches a build, or installs
an app just because it was opened. The separate browser preview uses `?demo=1`
and labels all sample data. Demo actions never call SSH, a broker, or an installer.

## Install the Fedora candidate

The first packaging target is Fedora 43 on x86_64. Download the RPM and
`SHA256SUMS` from the same verified candidate, check its checksum, then install:

```sh
sha256sum --check SHA256SUMS
sudo dnf install './App Manager-0.1.0-1.x86_64.rpm'
app-manager-desktop
```

The RPM installs the desktop app and declares its GTK, WebKitGTK and OpenSSH
runtime dependencies. The accompanying `app-manager-linux-amd64.tar.gz`
contains the desktop executable, `appctl`, the optional broker, and setup docs.
Extract it into a new directory and run `./app-manager-0.1.0/app-manager-desktop`
with the same runtime dependencies installed. This archive is built against
Fedora 43; compatibility with other distributions has not been verified.

CI artifacts are candidates until the shared release pipeline publishes them
with a complete receipt. No public binary release is claimed by this README.

## Run from source

Use the pinned Rust toolchain and Node 22.22.1 or a compatible Node 22 release.
Install the Linux native prerequisites listed in [CONTRIBUTING.md](CONTRIBUTING.md).

```sh
npm ci
npm ci --prefix web --ignore-scripts
npm run dev
```

For a browser preview of the interface:

```sh
npm run dev --prefix web
# Open the printed URL with ?demo=1.
```

For the shared local CLI:

```sh
cargo run -p app-manager-cli -- status
cargo run -p app-manager-cli -- machine-add \
  --name 'Development VM' --host dev-vm --root '~/projects'
cargo run -p app-manager-cli -- machine-test MACHINE_ID
cargo run -p app-manager-cli -- workspaces MACHINE_ID
```

App Manager uses your existing OpenSSH configuration and agent. Establish the
host's identity with normal `ssh dev-vm` first. The app uses noninteractive
authentication and strict host-key checking, so an unknown or changed host is
reported rather than accepted automatically. It stores connection metadata,
never SSH private keys or passwords.

## Connect release CI

A release source needs three public HTTPS locations: a schema-1 release-tools
manifest, the published artifact base URL, and the complete publication receipt.
Its source ID must match the manifest's `project` field. See
[examples/gnome-source.example.json](examples/gnome-source.example.json).

```sh
cargo run -p app-manager-cli -- source-add source.json
cargo run -p app-manager-cli -- refresh
cargo run -p app-manager-cli -- install APP_ID
cargo run -p app-manager-cli -- rollback APP_ID
```

The consumer checks the manifest identity and receipt, then checks the downloaded
artifact's size and digest before extracting it. It rejects incomplete, draft,
dry-run, conflicting, and unpublished evidence. A receipt is publication
evidence from the configured source; this version does not verify a cryptographic
manifest signature. Configure sources you trust. Private release credentials
and expiring signed download URLs are not supported yet.

The package workflow creates immutable candidates and evidence. Canonical
publication is integrated through the shared release CI, rather than a second
publisher inside the desktop app. See [docs/ci.md](docs/ci.md).

## GitHub App and saved builds

The broker is optional: local SSH connections and public release installation
work without a GitHub account or a hosted App Manager service.

Follow [docs/broker.md](docs/broker.md) to register and install your GitHub App,
configure selected repositories, create an independent broker account, and
start the service:

```sh
cargo run -p app-manager-broker -- --config /path/to/broker.json
```

The GitHub App private key, provider tokens, and webhook secrets stay in the
broker's protected environment or key file. Account enrollment is separate from
GitHub App installation. The desktop credential authorizes only the account's
configured repository allowlist. Registered workflows accept `expected_commit`
and `request_id`; an exact resolved commit is required before dispatch.

KanterLabs uses Gitea as its canonical source and GitHub as its public mirror.
Other users can configure their own GitHub or Gitea repository mappings. The
product has no KanterLabs infrastructure dependency.

## Data and recovery

The application data path follows the operating system's user data convention.
`APP_MANAGER_DATA_DIR` or the CLI's `--data-dir` can select a separate app store.
GNOME extensions are installed under the user's actual data directory at
`gnome-shell/extensions`, independently of the app store.

State updates use a process lock, reread the latest state, verify a backup of the
prior bytes, and replace the file atomically. Compatible unknown fields survive
updates. Corrupt state and unsupported future schemas are preserved and refused;
the app never resets them or automatically restores a backup. Keep the previous
binary and a verified backup for an upgrade. Retained release artifacts support
app rollback without replacing the profile database.

An interrupted installation has a durable operation receipt. Matching completed
filesystem changes can be reconciled; ambiguous changes require inspection.
Never delete state or overwrite an extension to clear an error. See
[docs/recovery.md](docs/recovery.md) for backup, profile export, and recovery.

## Build and contribute

```sh
ci/check metadata
ci/check frontend
ci/check browser
ci/check rust
ci/package
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and validation, and
[docs/implementation-contract.md](docs/implementation-contract.md) for the native
command and broker APIs. Licensed under [MIT](LICENSE).
