# App Manager

Use scoped `luna_worker` agents for independent modules. The primary agent owns
cross-module contracts, lockfiles, full builds, integration and final verification.
Workers must not spawn agents or overwrite other workers' changes.

GitHub Actions jobs use `homelab` for checks/orchestration and `homelab-heavy` for
Rust/desktop builds and browser/integration suites. Gitea is canonical for the
KanterLabs deployment; GitHub is the public mirror.

Read `docs/implementation-contract.md` before changing interfaces. Preserve
saved connections, settings, operation receipts and retained artifacts on
upgrades/removal. Never store credentials in profiles or log secret values.
