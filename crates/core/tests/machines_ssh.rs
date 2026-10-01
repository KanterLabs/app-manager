//! Disposable loopback OpenSSH coverage for the saved-machine transport.
//!
//! The fixture owns every key, config and known-hosts file under a temporary
//! directory.  It uses the hidden config-path seam in `machines` so these
//! tests never modify the developer's SSH configuration or known-hosts file.

use std::fs;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use app_manager_core::machines;
use app_manager_core::models::MachineProfile;

struct SshServer {
    port: u16,
    child: Child,
    client_config: PathBuf,
    known_hosts: PathBuf,
}

impl Drop for SshServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Fixture {
    root: tempfile::TempDir,
    user: String,
    user_key: PathBuf,
    host_key: PathBuf,
    authorized_keys: PathBuf,
    workspace: PathBuf,
    server: SshServer,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.server.child.kill();
        let _ = self.server.child.wait();
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary SSH fixture directory");
        let user = current_user();
        let user_key = root.path().join("client-key");
        let host_key = root.path().join("host-key");
        let authorized_keys = root.path().join("authorized_keys");
        generate_key(&user_key);
        generate_key(&host_key);
        fs::copy(user_key.with_extension("pub"), &authorized_keys)
            .expect("authorized key should be copied");

        let workspace = root.path().join("workspace");
        create_git_fixture(&workspace);
        let server = start_server(
            root.path(),
            &user,
            &user_key,
            &host_key,
            &authorized_keys,
            None,
        );

        Self {
            root,
            user,
            user_key,
            host_key,
            authorized_keys,
            workspace,
            server,
        }
    }

    fn profile(&self, port: u16) -> MachineProfile {
        MachineProfile {
            id: "ssh-fixture".to_string(),
            name: "Loopback SSH fixture".to_string(),
            host: "127.0.0.1".to_string(),
            user: Some(self.user.clone()),
            port,
            workspace_roots: vec![self.workspace.to_string_lossy().into_owned()],
        }
    }
}

fn current_user() -> String {
    let output = Command::new("id")
        .arg("-un")
        .output()
        .expect("id should be installed");
    assert!(output.status.success(), "id -un failed");
    String::from_utf8(output.stdout)
        .expect("username should be UTF-8")
        .trim()
        .to_string()
}

fn generate_key(path: &Path) {
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .expect("ssh-keygen should be installed");
    assert!(
        output.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git should be installed")
}

fn create_git_fixture(path: &Path) {
    fs::create_dir(path).expect("workspace directory");
    let initialized = git(path, &["init", "-q", "-b", "main", "."]);
    assert!(
        initialized.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    for (key, value) in [
        ("user.name", "App Manager SSH Test"),
        ("user.email", "app-manager-ssh@example.test"),
        ("commit.gpgSign", "false"),
    ] {
        assert!(git(path, &["config", key, value]).status.success());
    }
    fs::write(path.join("README.md"), "initial\n").expect("workspace file");
    assert!(git(path, &["add", "README.md"]).status.success());
    assert!(git(path, &["commit", "-q", "--no-verify", "-m", "initial"])
        .status
        .success());
    let remote = git(
        path,
        &[
            "remote",
            "add",
            "origin",
            "https://token:secret@example.com/org/fixture.git?token=again",
        ],
    );
    assert!(remote.status.success());
    fs::write(path.join("README.md"), "dirty\n").expect("dirty workspace file");
}

fn next_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
    listener.local_addr().expect("loopback address").port()
}

fn public_key_line(path: &Path) -> String {
    let value = fs::read_to_string(path).expect("public key file");
    let mut fields = value.split_whitespace();
    let algorithm = fields.next().expect("public key algorithm");
    let key = fields.next().expect("public key body");
    format!("{algorithm} {key}")
}

fn known_host_line(port: u16, host_key: &Path) -> String {
    format!(
        "[127.0.0.1]:{port} {}\n",
        public_key_line(host_key.with_extension("pub").as_path())
    )
}

fn write_client_config(path: &Path, user: &str, port: u16, user_key: &Path, known_hosts: &Path) {
    let config = format!(
        "Host 127.0.0.1\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {}\n  IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking yes\n  BatchMode yes\n",
        user_key.display(),
        known_hosts.display(),
    );
    fs::write(path, config).expect("client SSH config");
}

fn write_server_config(
    path: &Path,
    user: &str,
    port: u16,
    host_key: &Path,
    authorized_keys: &Path,
    force_command: Option<&str>,
) {
    let mut config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nAllowUsers {user}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nChallengeResponseAuthentication no\nPubkeyAuthentication yes\nAuthenticationMethods publickey\nPermitRootLogin yes\nStrictModes no\nUsePAM no\nPermitTTY no\nAllowTcpForwarding no\nX11Forwarding no\nLogLevel QUIET\n",
        host_key.display(),
        authorized_keys.display(),
    );
    if let Some(force_command) = force_command {
        config.push_str(&format!("ForceCommand {force_command}\n"));
    }
    fs::write(path, config).expect("server SSH config");
}

fn start_server(
    root: &Path,
    user: &str,
    user_key: &Path,
    host_key: &Path,
    authorized_keys: &Path,
    force_command: Option<&str>,
) -> SshServer {
    let port = next_port();
    let suffix = force_command.map_or("ok", |_| "failure");
    let server_config = root.join(format!("sshd-{suffix}-{port}.conf"));
    let client_config = root.join(format!("ssh-{suffix}-{port}.conf"));
    let known_hosts = root.join(format!("known-hosts-{suffix}-{port}"));
    write_server_config(
        &server_config,
        user,
        port,
        host_key,
        authorized_keys,
        force_command,
    );
    fs::write(&known_hosts, known_host_line(port, host_key)).expect("known-hosts file");
    write_client_config(&client_config, user, port, user_key, &known_hosts);

    let child = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(&server_config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sshd should be installed");
    let mut server = SshServer {
        port,
        child,
        client_config,
        known_hosts,
    };
    wait_until_ready(&mut server);
    server
}

fn wait_until_ready(server: &mut SshServer) {
    let address = format!("127.0.0.1:{}", server.port)
        .parse()
        .expect("loopback address");
    for _ in 0..100 {
        if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
            return;
        }
        if let Some(status) = server.child.try_wait().expect("sshd status") {
            let mut stderr = String::new();
            if let Some(mut pipe) = server.child.stderr.take() {
                pipe.read_to_string(&mut stderr).expect("sshd output");
            }
            panic!("sshd exited before listening ({status:?}): {}", stderr);
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = server.child.kill();
    panic!("sshd did not listen on port {}", server.port);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_ssh_exercises_connection_discovery_and_security_failures() {
    let fixture = Fixture::new();
    let profile = fixture.profile(fixture.server.port);

    let report = machines::test_connection_with_ssh_config(&profile, &fixture.server.client_config)
        .await
        .expect("loopback SSH connection should succeed");
    assert_eq!(report.status, "connected");
    assert_eq!(report.workspaces.len(), 1);
    let workspace = &report.workspaces[0];
    assert_eq!(workspace.branch.as_deref(), Some("main"));
    assert!(workspace.commit.as_deref().is_some_and(|commit| {
        commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    }));
    assert!(workspace.dirty);
    assert_eq!(
        workspace.repository.as_deref(),
        Some("https://example.com/org/fixture.git")
    );

    let discovered =
        machines::discover_workspaces_with_ssh_config(&profile, &fixture.server.client_config)
            .await
            .expect("loopback workspace discovery should succeed");
    assert_eq!(discovered.len(), 1);
    assert!(discovered[0].dirty);

    let wrong_host_key = fixture.root.path().join("wrong-host-key");
    generate_key(&wrong_host_key);
    fs::write(
        &fixture.server.known_hosts,
        known_host_line(fixture.server.port, &wrong_host_key),
    )
    .expect("replace fixture known-hosts file");
    let changed_host =
        machines::test_connection_with_ssh_config(&profile, &fixture.server.client_config)
            .await
            .expect_err("changed host key must be rejected");
    assert!(changed_host.to_string().contains("SSH operation failed"));
    assert!(!changed_host.to_string().contains("secret"));

    let failure_server = start_server(
        fixture.root.path(),
        &fixture.user,
        &fixture.user_key,
        &fixture.host_key,
        &fixture.authorized_keys,
        Some("/bin/false"),
    );
    let failure_profile = fixture.profile(failure_server.port);
    let remote_failure =
        machines::test_connection_with_ssh_config(&failure_profile, &failure_server.client_config)
            .await
            .expect_err("forced remote command failure must be surfaced");
    assert!(remote_failure.to_string().contains("SSH operation failed"));
    assert!(!remote_failure.to_string().contains("secret"));
}
