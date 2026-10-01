#!/usr/bin/env python3
"""Smoke-test the packaged App Manager Tauri binary through WebDriver.

This deliberately drives the native window through ``tauri-driver``.  It does
not start Vite, set ``?demo=1``, mock ``invoke``, or seed a fixture state file.
The only state written by the test is under a TemporaryDirectory passed to the
native process through APP_MANAGER_DATA_DIR.
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable, Dict, Iterable, List, Mapping, Optional, Sequence


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_TIMEOUT = 35.0
POLL_INTERVAL = 0.2
W3C_ELEMENT_KEY = "element-6066-11e4-a52e-4f735466cecf"


class SmokeFailure(AssertionError):
    """An expected native smoke assertion failed."""


class WebDriverError(RuntimeError):
    """A WebDriver request failed or returned a protocol error."""

    def __init__(
        self,
        message: str,
        *,
        status: Optional[int] = None,
        payload: Any = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.payload = payload


def _decode_json(raw: bytes) -> Any:
    if not raw:
        return {}
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return raw.decode("utf-8", errors="replace")


def _protocol_error(payload: Any) -> Optional[str]:
    if not isinstance(payload, Mapping):
        return None
    value = payload.get("value")
    if isinstance(value, Mapping) and value.get("error"):
        message = value.get("message") or value.get("error")
        return str(message)
    return None


class WebDriverClient:
    """Small classic-W3C WebDriver client using only Python's stdlib."""

    def __init__(self, endpoint: str, *, timeout: float = DEFAULT_TIMEOUT) -> None:
        self.endpoint = endpoint.rstrip("/")
        self.timeout = timeout
        self.session_id: Optional[str] = None

    def request(self, method: str, path: str, body: Optional[Mapping[str, Any]] = None) -> Any:
        url = f"{self.endpoint}{path if path.startswith('/') else f'/{path}'}"
        data = None if body is None else json.dumps(body).encode("utf-8")
        request = urllib.request.Request(
            url,
            data=data,
            method=method,
            headers={
                "Accept": "application/json",
                "Content-Type": "application/json; charset=utf-8",
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                payload = _decode_json(response.read())
                status = getattr(response, "status", 200)
        except urllib.error.HTTPError as error:
            payload = _decode_json(error.read())
            detail = _protocol_error(payload) or repr(payload)
            raise WebDriverError(
                f"WebDriver {method} {path} returned HTTP {error.code}: {detail}",
                status=error.code,
                payload=payload,
            ) from error
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise WebDriverError(f"WebDriver {method} {path} could not be reached: {error}") from error

        detail = _protocol_error(payload)
        if detail:
            raise WebDriverError(
                f"WebDriver {method} {path} returned a protocol error: {detail}",
                status=status,
                payload=payload,
            )
        return payload

    def _value(self, payload: Any) -> Any:
        if isinstance(payload, Mapping) and "value" in payload:
            return payload["value"]
        return payload

    def status(self) -> Mapping[str, Any]:
        value = self._value(self.request("GET", "/status"))
        if not isinstance(value, Mapping):
            raise WebDriverError(f"Unexpected WebDriver status payload: {value!r}")
        return value

    def new_session(self, application: Path) -> str:
        if self.session_id:
            self.quit()
        payload = {
            "capabilities": {
                "alwaysMatch": {
                    "browserName": "wry",
                    # tauri-driver translates this neutral capability to the
                    # platform's native WebDriver binary capability.
                    "tauri:options": {"application": str(application)},
                }
            }
        }
        response = self.request("POST", "/session", payload)
        value = self._value(response)
        session_id: Optional[str] = None
        if isinstance(value, Mapping):
            candidate = value.get("sessionId") or value.get("session_id")
            if candidate is not None:
                session_id = str(candidate)
        if session_id is None and isinstance(response, Mapping):
            candidate = response.get("sessionId") or response.get("session_id")
            if candidate is not None:
                session_id = str(candidate)
        if not session_id:
            raise WebDriverError(f"WebDriver did not return a session id: {response!r}")
        self.session_id = session_id
        # A short script timeout prevents a hostile/untrusted page from
        # holding the security probe open indefinitely.
        try:
            self.request("POST", f"/session/{session_id}/timeouts", {"script": 5000})
        except WebDriverError:
            # Older WebKitWebDriver versions can omit this endpoint.  The
            # client-side request timeout still bounds every individual call.
            pass
        return session_id

    def quit(self) -> None:
        session_id = self.session_id
        self.session_id = None
        if session_id:
            try:
                self.request("DELETE", f"/session/{session_id}")
            except WebDriverError:
                # The native app may already have exited after a failed test;
                # process-group cleanup below remains the final safety net.
                pass

    def _session_path(self, suffix: str) -> str:
        if not self.session_id:
            raise WebDriverError("WebDriver session has not been created")
        return f"/session/{self.session_id}{suffix}"

    @staticmethod
    def _element_id(value: Any) -> str:
        if not isinstance(value, Mapping):
            raise WebDriverError(f"Unexpected element payload: {value!r}")
        candidate = value.get(W3C_ELEMENT_KEY) or value.get("ELEMENT")
        if candidate is None:
            raise WebDriverError(f"Element id is missing from payload: {value!r}")
        return str(candidate)

    def find_elements(self, using: str, value: str) -> List[str]:
        response = self.request("POST", self._session_path("/elements"), {"using": using, "value": value})
        result = self._value(response)
        if not isinstance(result, list):
            raise WebDriverError(f"Unexpected elements payload: {result!r}")
        return [self._element_id(item) for item in result]

    def find_element(self, using: str, value: str) -> str:
        response = self.request("POST", self._session_path("/element"), {"using": using, "value": value})
        return self._element_id(self._value(response))

    def element_text(self, element: str) -> str:
        value = self._value(self.request("GET", self._session_path(f"/element/{element}/text")))
        return "" if value is None else str(value)

    def element_attribute(self, element: str, name: str) -> Optional[str]:
        value = self._value(
            self.request("GET", self._session_path(f"/element/{element}/attribute/{name}"))
        )
        return None if value is None else str(value)

    def click(self, element: str) -> None:
        self.request("POST", self._session_path(f"/element/{element}/click"), {})

    def clear(self, element: str) -> None:
        self.request("POST", self._session_path(f"/element/{element}/clear"), {})

    def send_keys(self, element: str, text: str) -> None:
        self.request(
            "POST",
            self._session_path(f"/element/{element}/value"),
            {"text": text, "value": list(text)},
        )

    def execute(self, script: str, args: Optional[Sequence[Any]] = None) -> Any:
        response = self.request(
            "POST",
            self._session_path("/execute/sync"),
            {"script": script, "args": list(args or [])},
        )
        return self._value(response)

    def execute_async(self, script: str, args: Optional[Sequence[Any]] = None) -> Any:
        response = self.request(
            "POST",
            self._session_path("/execute/async"),
            {"script": script, "args": list(args or [])},
        )
        return self._value(response)

    def body_text(self) -> str:
        value = self.execute("return document.body ? document.body.innerText : '';")
        return "" if value is None else str(value)

    def navigate(self, url: str) -> None:
        self.request("POST", self._session_path("/url"), {"url": url})

    def current_url(self) -> str:
        value = self._value(self.request("GET", self._session_path("/url")))
        return "" if value is None else str(value)


def wait_for(
    predicate: Callable[[], Any],
    description: str,
    *,
    timeout: float = DEFAULT_TIMEOUT,
) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Optional[BaseException] = None
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except Exception as error:  # WebDriver can briefly report stale/loading errors.
            last_error = error
        time.sleep(POLL_INTERVAL)
    suffix = f"; last error: {last_error}" if last_error else ""
    raise SmokeFailure(f"Timed out waiting for {description}{suffix}")


def wait_for_text(client: WebDriverClient, text: str, *, timeout: float = DEFAULT_TIMEOUT) -> str:
    return str(wait_for(lambda: text in client.body_text() and client.body_text(), f"text {text!r}", timeout=timeout))


def click_accessible_name(client: WebDriverClient, name: str) -> None:
    """Click a visible button/link by its accessible text or aria-label."""

    def find_and_click() -> bool:
        for selector in ("button", "a"):
            for element in client.find_elements("css selector", selector):
                label = client.element_attribute(element, "aria-label") or client.element_text(element)
                normalized = " ".join(label.split())
                # App Manager's navigation buttons put a decorative
                # aria-hidden icon before their visible label (for example,
                # "▣Machines").  Matching the visible suffix keeps this
                # selector tied to the accessible control name rather than to
                # an icon glyph.
                if normalized == name or normalized.endswith(name):
                    client.click(element)
                    return True
        return False

    wait_for(find_and_click, f"accessible control {name!r}")


def fill_css(client: WebDriverClient, selector: str, value: str) -> None:
    element = client.find_element("css selector", selector)
    client.clear(element)
    client.send_keys(element, value)


def role_text(client: WebDriverClient, role: str, *, within: Optional[str] = None) -> List[str]:
    selector = f"{within} [role={json.dumps(role)}]" if within else f"[role={json.dumps(role)}]"
    return [client.element_text(element) for element in client.find_elements("css selector", selector)]


def read_state(path: Path) -> Dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError, json.JSONDecodeError) as error:
        raise SmokeFailure(f"Could not read native state at {path}: {error}") from error
    if not isinstance(value, dict):
        raise SmokeFailure(f"Native state is not an object: {value!r}")
    return value


def wait_for_profile(path: Path, name: str) -> Dict[str, Any]:
    def profile() -> Optional[Dict[str, Any]]:
        if not path.exists():
            return None
        state = read_state(path)
        machines = state.get("machines")
        if not isinstance(machines, list):
            raise SmokeFailure(f"Native state machines field is not a list: {machines!r}")
        for item in machines:
            if isinstance(item, dict) and item.get("name") == name:
                return item
        return None

    return wait_for(profile, f"native state profile {name!r}")


def free_tcp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class RemoteFixture(http.server.ThreadingHTTPServer):
    """Owned localhost fixture used only for the remote-origin IPC check."""

    allow_reuse_address = True

    def __init__(self) -> None:
        self.served = threading.Event()
        super().__init__(("127.0.0.1", 0), RemoteFixtureHandler)


class RemoteFixtureHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        fixture = self.server
        if isinstance(fixture, RemoteFixture):
            fixture.served.set()
        if self.path != "/":
            self.send_error(http.HTTPStatus.NOT_FOUND)
            return
        body = b"<!doctype html><html><body><h1>Remote origin fixture</h1></body></html>"
        self.send_response(http.HTTPStatus.OK)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: Any) -> None:
        return


def resolve_binary(explicit: Optional[str]) -> Path:
    candidates: Iterable[Path]
    if explicit:
        candidates = (Path(explicit).expanduser(),)
    elif os.environ.get("APP_MANAGER_BINARY"):
        candidates = (Path(os.environ["APP_MANAGER_BINARY"]).expanduser(),)
    else:
        candidates = (
            ROOT / "target/release/app-manager-desktop",
            ROOT / "src-tauri/target/release/app-manager-desktop",
        )
    for candidate in candidates:
        path = candidate.resolve()
        if path.is_file() and os.access(path, os.X_OK):
            return path
    listed = ", ".join(str(Path(candidate).expanduser()) for candidate in candidates)
    raise SmokeFailure(
        "Native release binary was not found or is not executable. "
        "Build it first with `npm run build:desktop`, then pass "
        f"--binary PATH. Checked: {listed}"
    )


def start_driver(
    command: Sequence[str],
    *,
    port: int,
    native_port: int,
    native_driver: Optional[str],
    environment: Mapping[str, str],
    log_path: Path,
) -> subprocess.Popen[bytes]:
    argv = [*command, "--port", str(port), "--native-port", str(native_port)]
    if native_driver:
        argv.extend(["--native-driver", native_driver])
    try:
        log = log_path.open("wb")
        process = subprocess.Popen(
            argv,
            cwd=ROOT,
            env=dict(environment),
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    except FileNotFoundError as error:
        try:
            log.close()
        except UnboundLocalError:
            pass
        raise SmokeFailure(
            f"Could not start {argv[0]!r}. Install tauri-driver and ensure it is on PATH "
            "(the Linux native driver is WebKitWebDriver)."
        ) from error
    except OSError:
        log.close()
        raise
    # The process owns this descriptor after fork; the parent can close it.
    log.close()
    return process


def terminate_process_group(process: Optional[subprocess.Popen[bytes]]) -> None:
    if process is None or process.poll() is not None:
        return
    try:
        if os.name == "posix":
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.terminate()
        process.wait(timeout=8)
    except (OSError, subprocess.TimeoutExpired):
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
            process.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired):
            pass


def driver_log_tail(path: Path, limit: int = 8000) -> str:
    try:
        contents = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return ""
    return contents[-limit:].strip()


def assert_live_empty(client: WebDriverClient) -> None:
    wait_for_text(client, "LIVE DESKTOP")
    body = client.body_text()
    if "Your app shelf is empty" not in body:
        raise SmokeFailure(f"Native app did not render its empty live state: {body[:1000]!r}")
    if "Sample data" in body or "NFL Scores" in body:
        raise SmokeFailure("Native smoke session rendered demo/fixture data")


def check_remote_origin_denial(client: WebDriverClient) -> None:
    fixture = RemoteFixture()
    thread = threading.Thread(target=fixture.serve_forever, name="app-manager-smoke-http", daemon=True)
    thread.start()
    origin = f"http://127.0.0.1:{fixture.server_address[1]}"
    url = f"{origin}/"
    navigation_error: Optional[BaseException] = None
    try:
        try:
            client.navigate(url)
        except BaseException as error:
            navigation_error = error

        if not fixture.served.wait(timeout=3):
            detail = f" ({navigation_error})" if navigation_error else ""
            raise SmokeFailure(f"WebDriver never reached the owned remote-origin fixture{detail}")

        # A Tauri capability may reject the top-level navigation.  If it does,
        # the original app URL remains active and the native command bridge was
        # never exposed to the untrusted origin; that is a valid denial result.
        if navigation_error:
            return
        current_url = client.current_url()
        if not current_url.startswith(origin):
            return

        result = client.execute_async(
            """
            const done = arguments[arguments.length - 1];
            (async () => {
              const bridge = window.__TAURI_INTERNALS__;
              if (!bridge || typeof bridge.invoke !== 'function') {
                done({status: 'denied', detail: 'native bridge unavailable'});
                return;
              }
              try {
                await bridge.invoke('get_snapshot');
                done({status: 'allowed'});
              } catch (error) {
                done({status: 'denied', detail: String(error)});
              }
            })();
            """
        )
        if not isinstance(result, Mapping):
            raise SmokeFailure(f"Remote-origin IPC probe returned an invalid result: {result!r}")
        if result.get("status") == "allowed":
            raise SmokeFailure("Untrusted localhost origin invoked native get_snapshot")
        if result.get("status") != "denied":
            raise SmokeFailure(f"Remote-origin IPC probe did not prove denial: {result!r}")
    finally:
        fixture.shutdown()
        fixture.server_close()
        thread.join(timeout=3)


def run_smoke(binary: Path, driver_command: Sequence[str], native_driver: Optional[str]) -> None:
    driver_process: Optional[subprocess.Popen[bytes]] = None
    client: Optional[WebDriverClient] = None
    with tempfile.TemporaryDirectory(prefix="app-manager-desktop-smoke-") as temporary:
        temporary_root = Path(temporary)
        data_dir = temporary_root / "data"
        log_path = temporary_root / "tauri-driver.log"
        environment = os.environ.copy()
        environment["APP_MANAGER_DATA_DIR"] = str(data_dir)
        driver_port = free_tcp_port()
        native_port = free_tcp_port()
        driver_process = start_driver(
            driver_command,
            port=driver_port,
            native_port=native_port,
            native_driver=native_driver,
            environment=environment,
            log_path=log_path,
        )
        client = WebDriverClient(f"http://127.0.0.1:{driver_port}")
        try:
            def driver_ready() -> bool:
                if driver_process is not None and driver_process.poll() is not None:
                    raise SmokeFailure(
                        "tauri-driver exited before becoming ready.\n" + driver_log_tail(log_path)
                    )
                status = client.status()
                return bool(status.get("ready", True))

            wait_for(driver_ready, "tauri-driver to become ready", timeout=20)
            client.new_session(binary)
            assert_live_empty(client)

            # Add one real profile through the native UI.  The profile is
            # intentionally unreachable but syntactically valid so the Rust
            # command persists it before the connection test fails.
            click_accessible_name(client, "Machines")
            wait_for_text(client, "No saved machines")
            click_accessible_name(client, "Add a development machine")
            wait_for(lambda: bool(client.find_elements("css selector", "#machine-form")), "machine form")
            fill_css(client, '#machine-form input[placeholder="Development VM"]', "Smoke unreachable")
            fill_css(client, '#machine-form input[placeholder="dev-vm"]', "127.0.0.1")
            fill_css(client, '#machine-form input[type="number"]', "1")
            fill_css(client, "#machine-form textarea", "/tmp/app-manager-smoke")
            click_accessible_name(client, "Save machine")
            wait_for(lambda: not client.find_elements("css selector", "#machine-form"), "machine modal to close")
            wait_for_text(client, "Smoke unreachable")

            saved_profile = wait_for_profile(data_dir / "state.json", "Smoke unreachable")
            expected_roots = ["/tmp/app-manager-smoke"]
            if (
                saved_profile.get("host") != "127.0.0.1"
                or saved_profile.get("port") != 1
                or saved_profile.get("workspaceRoots") != expected_roots
            ):
                raise SmokeFailure(f"Native state contains an unexpected saved profile: {saved_profile!r}")
            state_before_restart = read_state(data_dir / "state.json")
            if state_before_restart.get("sources") != []:
                raise SmokeFailure("Unexpected release source appeared in the empty native state")

            # DELETE /session asks tauri-driver to close the actual native
            # process.  A new session then proves persistence across restart.
            client.quit()
            client.new_session(binary)
            assert_live_empty(client)
            click_accessible_name(client, "Machines")
            wait_for_text(client, "Smoke unreachable")
            if "127.0.0.1" not in client.body_text():
                raise SmokeFailure("Restarted native UI did not show the persisted SSH host")

            click_accessible_name(client, "Test connection")
            alert_text = str(
                wait_for(
                    lambda: next(iter(role_text(client, "alert")), ""),
                    "actual unreachable SSH error",
                    timeout=30,
                )
            )
            lowered_alert = alert_text.lower()
            if "connection test failed" not in lowered_alert:
                raise SmokeFailure(f"Unexpected connection-test alert: {alert_text!r}")
            if "sample" in lowered_alert or "nfl scores" in lowered_alert:
                raise SmokeFailure(f"Connection test displayed fixture data: {alert_text!r}")
            if not any(token in lowered_alert for token in ("ssh", "timed out", "connection")):
                raise SmokeFailure(f"Connection test did not expose a native SSH error: {alert_text!r}")

            # Validate the source form in the real live window.  The invalid
            # canonical ID is rejected in React before save_source can run.
            click_accessible_name(client, "Apps")
            wait_for_text(client, "Your app shelf is empty")
            click_accessible_name(client, "Add your first source")
            wait_for(lambda: bool(client.find_elements("css selector", "#source-form")), "source form")
            fill_css(client, '#source-form input[placeholder="nfl-scores"]', "bad id")
            click_accessible_name(client, "Save source")
            source_alert = str(
                wait_for(
                    lambda: next(iter(role_text(client, "alert", within="#source-form")), ""),
                    "source validation error",
                )
            )
            if "canonical app id" not in source_alert.lower():
                raise SmokeFailure(f"Unexpected source validation message: {source_alert!r}")

            # Submit a form that passes the React checks but is rejected by
            # the native verifier.  The credential-like query is deliberately
            # fake and must never be sent over the network.
            fill_css(client, '#source-form input[placeholder="nfl-scores"]', "smoke-source")
            fill_css(client, '#source-form input[placeholder="NFL Scores"]', "Smoke source")
            fill_css(client, '#source-form input[placeholder="https://releases.example/manifest.json"]', "https://example.org/manifest.json?token=fake")
            fill_css(client, '#source-form input[placeholder="https://releases.example/artifacts/"]', "https://example.org/artifacts/")
            fill_css(client, '#source-form input[placeholder="https://releases.example/receipt.json"]', "https://example.org/receipt.json")
            fill_css(client, '#source-form input[placeholder="app@example.org"]', "app@example.org")
            fill_css(client, '#source-form input[placeholder="48, 49, 50"]', "48")
            click_accessible_name(client, "Save source")
            native_source_alert = str(
                wait_for(
                    lambda: next(
                        (text for text in role_text(client, "alert") if "save source failed" in text.lower()),
                        "",
                    ),
                    "native source validation error",
                )
            )
            if "credential-like" not in native_source_alert.lower():
                raise SmokeFailure(f"Unexpected native source validation message: {native_source_alert!r}")
            state_after_validation = read_state(data_dir / "state.json")
            if state_after_validation.get("sources") != []:
                raise SmokeFailure("Invalid source form changed native state")

            check_remote_origin_denial(client)
            print("desktop smoke: PASS (native Tauri binary, persistence, error, validation, origin denial)")
        finally:
            if client is not None:
                client.quit()
            terminate_process_group(driver_process)
            tail = driver_log_tail(log_path)
            if tail and driver_process is not None and driver_process.returncode not in (None, 0):
                print(f"tauri-driver log:\n{tail}", file=sys.stderr)


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        help="path to an executable release binary (defaults to APP_MANAGER_BINARY or target/release/app-manager-desktop)",
    )
    parser.add_argument(
        "--tauri-driver",
        default=os.environ.get("TAURI_DRIVER", "tauri-driver"),
        help="tauri-driver executable (default: TAURI_DRIVER or tauri-driver)",
    )
    parser.add_argument(
        "--native-driver",
        default=os.environ.get("TAURI_NATIVE_DRIVER"),
        help="optional WebKitWebDriver path (default: TAURI_NATIVE_DRIVER or PATH lookup by tauri-driver)",
    )
    return parser.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    try:
        binary = resolve_binary(args.binary)
        driver_command = shlex.split(args.tauri_driver)
        if not driver_command:
            raise SmokeFailure("--tauri-driver must name an executable")
        if shutil.which(driver_command[0]) is None and not Path(driver_command[0]).is_file():
            raise SmokeFailure(
                f"{driver_command[0]!r} is not on PATH. Install tauri-driver and WebKitWebDriver first."
            )
        if args.native_driver:
            native_driver = Path(args.native_driver).expanduser()
            if not native_driver.is_file() or not os.access(native_driver, os.X_OK):
                raise SmokeFailure(f"--native-driver is not an executable file: {native_driver}")
        run_smoke(binary, driver_command, args.native_driver)
        return 0
    except (SmokeFailure, WebDriverError) as error:
        print(f"desktop smoke: FAIL: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("desktop smoke: interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
