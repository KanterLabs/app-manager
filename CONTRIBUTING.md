# Contributing

App Manager combines a Rust workspace, a Tauri 2 desktop shell, and a React
web package. Use the Rust toolchain declared by `rust-toolchain.toml`, a current
Node/npm toolchain, and Python 3. The desktop build also needs the native
GTK/WebKit development files described in [the CI guide](docs/ci.md).

After cloning, install the lockfile-defined dependencies and run the same
checks used by CI:

```bash
npm ci
npm ci --prefix web --ignore-scripts
ci/check metadata
ci/check frontend
ci/check rust
ci/check browser
```

Rust and desktop packaging can run in the portable Fedora 43 builder, which
supplies the pinned GTK/WebKit development environment:

```bash
ci/container-build rust
ci/container-build package
```

These commands require Docker or Podman-compatible tooling. The wrapper
mounts only this checkout and creates isolated temporary Cargo, npm, and home
directories for the invocation. It does not expose a developer home,
`~/.cargo`, credentials, or a host Docker socket. Its generated package
outputs stay under `dist/`; use direct `ci/package` for a host setup that needs
a custom reviewed path below that tree. Fedora builds use `target/fedora43` so
they do not consume artifacts produced by a different host toolchain.

Use `ci/check all` when a complete local pass is useful. The commands fail
when a required tool or dependency is absent; do not work around a failed
native dependency check by changing the workflow to install packages on the
runner. Record the image/toolchain change with the infrastructure owner.

Keep changes focused and include a short description of the user-visible
behavior in the pull request. Rust model and broker interface changes must
follow [the implementation contract](docs/implementation-contract.md). Never
place credentials, bearer tokens, private keys, or live deployment settings in
profiles, fixtures, logs, or test artifacts.

Fork pull requests are not executed on the trusted homelab runner pool. Run
the commands locally and include their results when a change needs a native
desktop environment. Same-repository PRs use the canonical CI workflow and
may be cancelled when superseded. Do not use `pull_request_target` to execute
contributor code.

## Packaging and releases

Run `ci/package` directly only after the root and web dependencies are
installed. The portable `ci/container-build package` command installs those
locked dependencies inside the Fedora builder first. Both paths build the
Fedora-target RPM plus available Debian/Linux archive outputs. The Linux
archive contains `app-manager-desktop`, `appctl`, `app-manager-broker`,
`LICENSE`, and `README.md`; the CLI and broker are built with `--locked` before
the Tauri build. Both paths then write checksums, a schema-1 release manifest,
and package evidence under `dist/`.
The package command stages output and refuses to overwrite an existing
destination; choose a fresh `OUTPUT_DIR`, `RELEASE_MANIFEST_PATH`, and
`PACKAGE_EVIDENCE_PATH` for a second direct local build.

Release candidates are created by a versioned tag or an explicit exact commit
and request ID dispatch (`expected_commit` and `request_id`). Dispatch commits
must belong to the protected `main` history. CI assembles and retains candidate
bytes for review. The
canonical INFRA164 release integration owns durable retention, forge
publication, receipts, and public verification. Its complete published receipt
must match the manifest's `source_commit` and `manifest_sha256`; a `finalized`
flag alone is not a publication proof for a retry. Contributors must not add
publication tokens or deploy steps to this repository's workflows.
