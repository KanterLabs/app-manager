# First-version verification

Verified on 2026-10-01. App Manager is an early Linux implementation, with
Fedora 43 x86_64 as the native packaging target.

## Completed checks

- Full Rust workspace: 50 tests passed, including an actual loopback OpenSSH
  server, HTTP release downloads and install/update/rollback, GitHub App JWT and
  installation-token HTTP fixtures, and Gitea workflow dispatch fixtures.
- Rust formatting, workspace compilation and Clippy with warnings denied.
- Frontend type checking, six React tests, and a production web build.
- Populated schema migration preserves stable IDs and compatible unknown fields;
  prior bytes have a verified backup. Concurrent profile writes preserve both
  updates. Corrupt, future-schema and missing populated-state files are refused.
- CLI profile export/import round trip, conflict preservation and overwrite
  refusal. Profile export contains only connection/source metadata.
- Fedora 43 RPM and exact-binary portable archive generation, SHA256 inventory,
  and verification with infrastructure's shared release-tools schema-1 verifier.
- A clean Fedora 43 image installed the RPM using DNF's declared runtime
  dependencies, then launched `/usr/bin/app-manager-desktop` as a new user.
  The native WebDriver smoke passed without a demo bridge or sandbox override:
  empty live state, VM profile persistence across process restart, real SSH
  failure, UI and native source validation, and remote-origin command denial.
- Product-native collaborative browser review of the explicit demo interface,
  machine form, settings access and narrow layouts.

The native smoke uses supported WebDriver script execution for DOM input and
click events because Fedora's WebKit driver does not implement its element
mutation endpoints. It exercises the actual React handlers and Tauri commands.
See [desktop-verification.md](desktop-verification.md) for reproduction.

## Integration gates

Public binary publication and durable release receipts belong to the shared
release-CI integration. CI output is a candidate until that publisher promotes
and verifies the exact retained bytes. See [ci.md](ci.md).

A live GitHub App registration and self-hosted broker enrollment still require
an operator's provider configuration. The integration is tested with disposable
provider fixtures; no KanterLabs App credential is included. See
[broker.md](broker.md).

No laptop installation, real development-VM enrollment, two-release NFL Scores
pilot, macOS/Windows installation, Flatpak adapter, RPM app-management adapter,
or unattended self-update is claimed by these checks.
