# Native desktop smoke verification

`tests/desktop-smoke.py` is the release-binary smoke harness for App Manager.
It is a classic W3C WebDriver client implemented with Python's standard
library; it does not use Vite, `?demo=1`, Selenium, a browser fixture, or a
mocked Tauri bridge.

The harness starts `tauri-driver`, which starts the executable passed in the
`tauri:options.application` capability. On Linux, `tauri-driver` translates
that capability to WebKitWebDriver's
`webkitgtk:browserOptions.binary` capability. The harness sets
`APP_MANAGER_DATA_DIR` to a private temporary directory inherited by the
native process, and removes that directory during teardown.

## Prerequisites

Run this on Linux with a display. For a headless Fedora builder, the existing
`ci/Dockerfile.fedora` native packages cover the Tauri build and Xvfb runtime;
the additional WebDriver requirement is an executable named
`WebKitWebDriver` on `PATH`.

Check the boundary before running the test:

```bash
python3 --version
command -v WebKitWebDriver
command -v Xvfb
command -v tauri-driver
```

Tauri's Linux manual setup documents `WebKitWebDriver` as the native driver
and `tauri-driver` as the intermediary. On Debian-family hosts the driver is
usually provided by `webkit2gtk-driver`. Fedora 43 package metadata currently
places `/usr/bin/WebKitWebDriver` in `webkitgtk6.0`; install that runtime
alongside the existing `webkit2gtk4.1-devel` build dependency. Package
contents can change with WebKitGTK updates, so verify the executable rather
than assuming the development package installs it:

```bash
dnf install -y webkitgtk6.0
command -v WebKitWebDriver
# If the command is still absent, ask DNF for the provider:
dnf provides '*/WebKitWebDriver'
sudo dnf install <package-reported-by-dnf>
```

Install the intermediary with the pinned Rust toolchain when it is absent:

```bash
cargo install tauri-driver --locked
```

Build the native release binary separately. The smoke harness never builds or
installs the application:

```bash
npm ci
npm ci --prefix web --ignore-scripts
npm run build:desktop
test -x target/release/app-manager-desktop
```

The builder/runtime image needs the native libraries listed in [the CI
guide](ci.md), including `webkit2gtk4.1-devel`, `gtk3-devel`,
`librsvg2-devel`, and `xorg-x11-server-Xvfb` on Fedora 43. The smoke actions
use SSH only against `127.0.0.1:1`, so no SSH server, key, account, broker, or
credential is needed.

## Run

With a real desktop display:

```bash
./tests/desktop-smoke.py --binary target/release/app-manager-desktop
```

With Fedora's headless display and a private D-Bus session:

```bash
xvfb-run -a dbus-run-session -- \
  ./tests/desktop-smoke.py --binary target/release/app-manager-desktop
```

`APP_MANAGER_BINARY` and `TAURI_DRIVER` can supply the two paths instead of
flags. `--tauri-driver PATH` is equivalent to setting `TAURI_DRIVER=PATH`.
When WebKitWebDriver is outside `PATH`, pass `--native-driver PATH` or set
`TAURI_NATIVE_DRIVER`; this is forwarded to tauri-driver's
`--native-driver` option. The harness selects free localhost ports for
`tauri-driver` and its native WebKitWebDriver, so it does not require ports
4444/4445 to be unused.

## Assertions covered

The test waits for the native window's `LIVE DESKTOP` state and confirms the
empty shelf contains no demo fixture data. It then:

1. Saves an unreachable SSH profile through the actual React form and checks
   the resulting `state.json` fields under the temporary data directory.
2. Deletes the WebDriver session, creates a new native session with the same
   data directory, and verifies that the profile is still rendered.
3. Clicks `Test connection` and requires the native SSH failure text; demo
   fixture text is rejected.
4. Enters an invalid canonical source ID and requires the live source-form
   validation alert, then submits a React-valid source with a credential-like
   URL query to exercise native source validation; both attempts must leave
   `state.json` without a source.
5. Serves a small owned localhost HTTP page, navigates the native WebView to
   it, and attempts `window.__TAURI_INTERNALS__.invoke('get_snapshot')` from
   that origin. A rejected navigation, an absent bridge, or a denied command
   is accepted; an allowed snapshot is a failure.

The test only cleans the temporary directory and process group it owns. It
does not call `killall`, delete an existing App Manager data directory, touch
the host keyring, or modify a user's SSH configuration.

## Official references

* [Tauri manual WebDriver setup](https://v2.tauri.app/develop/tests/webdriver/manual-setup/)
* [Tauri WebDriver CI setup](https://v2.tauri.app/develop/tests/webdriver/ci/)
* [`tauri-driver` README](https://github.com/tauri-apps/tauri/blob/dev/crates/tauri-driver/README.md)

The command exits nonzero when the binary, `tauri-driver`, WebKitWebDriver,
display, native session, persistence, UI validation, native error, or
remote-origin denial assertion cannot be established. A successful Python
syntax check alone is not a desktop smoke result.
