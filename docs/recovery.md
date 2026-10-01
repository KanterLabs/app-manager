# Data preservation and recovery

App Manager stores profiles, source configuration, cached release/build metadata,
installed-app records, and operation receipts in its application data directory.
The directory is shown in Settings and the sidebar. The broker stores independent
account/build/delivery state in its configured `stateDir`.

## Back up before an upgrade

Close the desktop application and stop CLI operations before copying its data
directory. Stop the broker before copying its state directory. Verify the copied
files' digests against the original files and keep the previous binaries. Include
the app store's `artifacts/` directory; it contains the retained ZIPs required for
app rollback. Back up the actual GNOME extension directory separately if you need
to retain filesystem changes outside App Manager.

These backups contain local paths, repository metadata, and any retained extension
settings. Protect them as private data. The desktop broker token is stored in the
native keyring and is absent from `state.json`; the GitHub App key and provider
credentials are separately provisioned broker secrets.

Before replacing state, App Manager writes and verifies an exact-byte backup.
Compatible updates preserve unknown fields so an older retained binary does not
erase newly added data. Unsupported future schemas and corrupt files fail closed.
Neither the app nor broker automatically restores a backup or recreates state to
clear an error. Restoring application state is a separate, explicit operator
operation; choose and verify the exact backup before proceeding.

## Export connection profiles

Profile export is separate from a data backup:

```sh
appctl export-profiles profiles.json
appctl --data-dir /path/to/another/app-store import-profiles profiles.json
```

The export contains only schema-1 machine and release-source profiles. It omits
the broker connection, credentials, installed state, build records, and retained
artifacts. Export refuses to overwrite an existing file. Import validates all
profiles and refuses conflicting IDs without changing the existing store.

Profiles still disclose hostnames, usernames, workspace paths, and source URLs.
Review the file before sharing it. SSH keys, passwords, token-bearing source URLs,
and private provider credentials are never valid profile fields.

## Interrupted extension changes

An install or rollback writes a durable receipt before swapping directories. If
the activated extension has the expected marker, a later operation can acknowledge
the already completed swap. An ambiguous filesystem state is preserved for
inspection. A staged directory or backup left by an interrupted operation belongs
to that receipt; do not guess which directory should replace the active extension.

Inspect the receipt and actual extension directory, verify the retained artifact
digest, and keep a copy of the relevant directories before an explicit repair.
The installer refuses unmanaged extensions, mismatched UUIDs, symlinks, unsafe ZIP
paths, and unsupported GNOME versions. App rollback selects the retained previous
artifact and preserves settings; it does not roll back the profile database.

## Failed build dispatch

A timed-out dispatch may have reached the provider. The broker retains it as
`ambiguous` and does not retry automatically. Refresh Builds and inspect the
provider's run list for the request ID and exact commit. An unrelated run with
the same commit is insufficient evidence that the request succeeded.
