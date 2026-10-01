//! Saved SSH machine profiles and read-only workspace discovery.
//!
//! The SSH transport deliberately uses the user's existing OpenSSH
//! configuration and agent.  Remote commands are a fixed `sh -s --` script
//! sent over stdin; profile values are data in that script, rather than
//! arguments that a remote shell could reinterpret.

use std::fmt::Write as _;
use std::net::Ipv6Addr;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;
use url::Url;

use crate::models::{ConnectionReport, MachineProfile, Workspace};

const SSH_PROGRAM: &str = "ssh";
const SSH_CONNECT_TIMEOUT_SECONDS: u64 = 10;
const SSH_PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const SSH_OUTPUT_LIMIT: usize = 128 * 1024;
const MAX_ID_LENGTH: usize = 128;
const MAX_NAME_LENGTH: usize = 256;
const MAX_HOST_LENGTH: usize = 253;
const MAX_USER_LENGTH: usize = 128;
const MAX_ROOT_LENGTH: usize = 4096;
const MAX_ROOTS: usize = 64;
const MAX_DISCOVERED_WORKSPACES: usize = 256;

const CONNECTION_OK: &str = "APP_MANAGER_CONNECTION_OK\n";
const CONNECTION_PROBE_SCRIPT: &str = "set -eu\nprintf '%s\\n' APP_MANAGER_CONNECTION_OK\n";

/// Validate a saved machine profile before it is used for an SSH operation.
///
/// Profiles are intentionally limited to non-secret SSH metadata.  Keys,
/// passwords and host trust remain under OpenSSH's normal configuration and
/// agent/known-hosts handling.
pub fn validate_profile(profile: &MachineProfile) -> Result<()> {
    validate_identifier("machine id", &profile.id, MAX_ID_LENGTH)?;

    if profile.name.trim().is_empty() {
        bail!("machine name must not be empty");
    }
    if profile.name.len() > MAX_NAME_LENGTH {
        bail!("machine name is too long");
    }
    if profile.name.chars().any(char::is_control) {
        bail!("machine name contains a control character");
    }

    validate_host(&profile.host)?;

    if profile.port == 0 {
        bail!("SSH port must be between 1 and 65535");
    }

    if let Some(user) = profile.user.as_deref() {
        validate_user(user)?;
    }

    if profile.workspace_roots.len() > MAX_ROOTS {
        bail!("too many workspace roots (maximum is {MAX_ROOTS})");
    }
    for root in &profile.workspace_roots {
        validate_workspace_root(root)?;
    }

    Ok(())
}

/// Open an SSH connection, run a harmless probe, and inspect configured
/// workspace roots when any were approved in the profile.
pub async fn test_connection(profile: &MachineProfile) -> Result<ConnectionReport> {
    test_connection_inner(profile, None).await
}

/// Test-only transport seam for disposable SSH fixtures.
///
/// The normal product path never supplies an SSH config path.  This helper is
/// hidden from generated API documentation so integration tests can point
/// OpenSSH at an ephemeral identity and known-hosts file without changing a
/// user's real SSH configuration.  BatchMode and strict host checking remain
/// enforced by the argv built below.
#[doc(hidden)]
pub async fn test_connection_with_ssh_config(
    profile: &MachineProfile,
    config_path: &Path,
) -> Result<ConnectionReport> {
    test_connection_inner(profile, Some(config_path)).await
}

async fn test_connection_inner(
    profile: &MachineProfile,
    config_path: Option<&Path>,
) -> Result<ConnectionReport> {
    validate_profile(profile)?;

    let output = run_ssh_script_with_config(profile, CONNECTION_PROBE_SCRIPT, config_path).await?;
    if output.stdout != CONNECTION_OK.as_bytes() {
        bail!("SSH probe returned an unexpected response");
    }

    let workspaces = discover_workspaces_inner(profile, config_path).await?;
    Ok(ConnectionReport {
        status: "connected".to_string(),
        message: "SSH connection established".to_string(),
        workspaces,
    })
}

/// Discover Git workspaces below the profile's approved roots.
///
/// Discovery is read-only.  Roots are canonicalized remotely, `find` does
/// not follow symlinks, and only a bounded depth and result count are
/// inspected.  An empty approval list intentionally performs no connection
/// and returns no workspaces.
pub async fn discover_workspaces(profile: &MachineProfile) -> Result<Vec<Workspace>> {
    discover_workspaces_inner(profile, None).await
}

/// Test-only counterpart to [`discover_workspaces`] using a disposable SSH
/// config.  See [`test_connection_with_ssh_config`] for why this exists.
#[doc(hidden)]
pub async fn discover_workspaces_with_ssh_config(
    profile: &MachineProfile,
    config_path: &Path,
) -> Result<Vec<Workspace>> {
    discover_workspaces_inner(profile, Some(config_path)).await
}

async fn discover_workspaces_inner(
    profile: &MachineProfile,
    config_path: Option<&Path>,
) -> Result<Vec<Workspace>> {
    validate_profile(profile)?;
    if profile.workspace_roots.is_empty() {
        return Ok(Vec::new());
    }

    let script = discovery_script(&profile.workspace_roots);
    let output = run_ssh_script_with_config(profile, &script, config_path).await?;
    parse_discovery_output(&output.stdout)
}

fn validate_identifier(field: &str, value: &str, max_length: usize) -> Result<()> {
    if value.is_empty() {
        bail!("{field} must not be empty");
    }
    if value.len() > max_length {
        bail!("{field} is too long");
    }
    if value.starts_with('-') {
        bail!("{field} must not start with '-'");
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("{field} contains unsupported characters");
    }
    Ok(())
}

fn validate_user(user: &str) -> Result<()> {
    if user.is_empty() {
        bail!("SSH user must not be empty");
    }
    if user.len() > MAX_USER_LENGTH {
        bail!("SSH user is too long");
    }
    if user.starts_with('-') {
        bail!("SSH user must not start with '-'");
    }
    if !user
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("SSH user contains unsupported characters");
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("SSH host must not be empty");
    }
    if host.len() > MAX_HOST_LENGTH {
        bail!("SSH host is too long");
    }
    if host.starts_with('-') {
        bail!("SSH host must not start with '-'");
    }
    if host.contains('@') || host.contains('/') || host.contains('\\') {
        bail!("SSH host must be a host or SSH config alias");
    }
    if host
        .chars()
        .any(|character| character.is_ascii_whitespace() || character.is_control())
    {
        bail!("SSH host contains whitespace or a control character");
    }

    // Keep the destination a single OpenSSH argument.  These characters are
    // not needed in a hostname or SSH config alias and can have option/config
    // interpretation in other SSH implementations.
    if host.bytes().any(|byte| {
        !byte.is_ascii_alphanumeric()
            && !matches!(byte, b'.' | b'-' | b'_' | b':' | b'[' | b']' | b'%')
    }) {
        bail!("SSH host contains unsupported characters");
    }

    if host.starts_with('[') || host.ends_with(']') {
        let Some(inner) = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
        else {
            bail!("bracketed SSH host is malformed");
        };
        if inner.is_empty() || !inner.contains(':') {
            bail!("bracketed SSH host must contain an IPv6 address");
        }
        validate_ipv6_literal(inner)?;
    } else if host.contains(':') {
        validate_ipv6_literal(host)?;
    } else {
        for label in host.split('.') {
            if label.is_empty() || label.starts_with('-') || label.ends_with('-') {
                bail!("SSH host contains an empty or malformed hostname label");
            }
        }
    }

    Ok(())
}

fn validate_ipv6_literal(value: &str) -> Result<()> {
    let address = value.split_once('%').map_or(value, |(address, _)| address);
    if address.parse::<Ipv6Addr>().is_err() {
        bail!("SSH host contains a malformed IPv6 address");
    }
    if let Some((_, zone)) = value.split_once('%') {
        if zone.is_empty()
            || !zone
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            bail!("SSH host contains a malformed IPv6 zone");
        }
    }
    Ok(())
}

fn validate_workspace_root(root: &str) -> Result<()> {
    if root.is_empty() {
        bail!("workspace root must not be empty");
    }
    if root.len() > MAX_ROOT_LENGTH {
        bail!("workspace root is too long");
    }
    if root.trim() != root {
        bail!("workspace root must not have leading or trailing whitespace");
    }
    if root.chars().any(char::is_control) {
        bail!("workspace root contains a control character");
    }

    let is_tilde_root = root == "~" || root.starts_with("~/");
    if !is_tilde_root && !root.starts_with('/') {
        bail!("workspace root must be absolute or begin with '~/'");
    }
    if root.starts_with('~') && !is_tilde_root {
        bail!("only the current user's '~' workspace root is supported");
    }
    if root.trim_end_matches('/').is_empty() {
        bail!("workspace root '/' is too broad");
    }

    for component in root.split('/') {
        if component == "." || component == ".." {
            bail!("workspace root must not contain '.' or '..' path components");
        }
    }

    Ok(())
}

fn ssh_target(profile: &MachineProfile) -> String {
    let host = if profile.host.contains(':') && !profile.host.starts_with('[') {
        format!("[{}]", profile.host)
    } else {
        profile.host.clone()
    };
    match profile.user.as_deref() {
        Some(user) => format!("{user}@{host}"),
        None => host,
    }
}

fn validate_ssh_config_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("SSH config path must be absolute");
    }
    let value = path
        .to_str()
        .ok_or_else(|| anyhow!("SSH config path must be valid UTF-8"))?;
    if value.chars().any(char::is_control) {
        bail!("SSH config path contains a control character");
    }
    Ok(())
}

/// Build argv for OpenSSH.  Keeping this separate makes it possible to test
/// that no profile field can become an option or an interpolated command.
#[cfg(test)]
fn ssh_args(profile: &MachineProfile) -> Result<Vec<String>> {
    ssh_args_with_config(profile, None)
}

fn ssh_args_with_config(
    profile: &MachineProfile,
    config_path: Option<&Path>,
) -> Result<Vec<String>> {
    validate_profile(profile)?;
    if let Some(config_path) = config_path {
        validate_ssh_config_path(config_path)?;
    }

    let mut args = vec!["-T".to_string()];
    if let Some(config_path) = config_path {
        args.push("-F".to_string());
        args.push(
            config_path
                .to_str()
                .expect("validated SSH config path")
                .to_string(),
        );
    }
    args.extend([
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECONDS}"),
        "-o".to_string(),
        "ConnectionAttempts=1".to_string(),
        "-p".to_string(),
        profile.port.to_string(),
        "--".to_string(),
        ssh_target(profile),
        "sh".to_string(),
        "-s".to_string(),
        "--".to_string(),
    ]);
    Ok(args)
}

fn build_ssh_command_with_config(
    profile: &MachineProfile,
    config_path: Option<&Path>,
) -> Result<Command> {
    let args = ssh_args_with_config(profile, config_path)?;
    let mut command = Command::new(SSH_PROGRAM);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

async fn run_ssh_script_with_config(
    profile: &MachineProfile,
    script: &str,
    config_path: Option<&Path>,
) -> Result<ProcessOutput> {
    let command = build_ssh_command_with_config(profile, config_path)?;
    let operation = run_process(command, script.as_bytes());
    let output = timeout(SSH_PROCESS_TIMEOUT, operation)
        .await
        .map_err(|_| anyhow!("SSH operation timed out"))??;

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "signal".to_string());
        // Do not include ssh's stderr: it may contain paths, usernames or
        // data supplied by a remote host, including credential-bearing text.
        bail!("SSH operation failed (exit {code})");
    }

    Ok(output)
}

async fn run_process(mut command: Command, script: &[u8]) -> Result<ProcessOutput> {
    let mut child = command
        .spawn()
        .map_err(|_| anyhow!("could not start the OpenSSH executable"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("OpenSSH stdin was not available"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("OpenSSH stdout was not available"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("OpenSSH stderr was not available"))?;

    let write_stdin = async {
        let result = stdin.write_all(script).await;
        drop(stdin);
        result
    };
    let wait_for_child = child.wait();
    let read_stdout = read_capped(stdout);
    let read_stderr = read_capped(stderr);
    let (write_result, status_result, stdout_result, stderr_result) =
        tokio::join!(write_stdin, wait_for_child, read_stdout, read_stderr);

    write_result.map_err(|_| anyhow!("could not send the SSH operation"))?;
    let status = status_result.map_err(|_| anyhow!("could not wait for the SSH operation"))?;
    let stdout = stdout_result?;
    // Stderr is deliberately read to enforce its bound, then discarded.
    stderr_result?;

    Ok(ProcessOutput { status, stdout })
}

async fn read_capped<R>(mut reader: R) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| anyhow!("could not read SSH output"))?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > SSH_OUTPUT_LIMIT {
            return Err(anyhow!("SSH output exceeded the safety limit"));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn discovery_script(roots: &[String]) -> String {
    // This script uses only fixed commands and quoted data.  In particular,
    // roots are not passed as `ssh host <root>` arguments because OpenSSH
    // concatenates remote command arguments before invoking the remote shell.
    let mut script = String::from(
        r#"set -eu

inspect_repo() {
    candidate=$1
    repo_root=$(git -C "$candidate" rev-parse --show-toplevel 2>/dev/null) || return 0
    commit=$(git -C "$repo_root" rev-parse --verify HEAD 2>/dev/null) || return 0
    case "$commit" in
        ''|*[!0-9a-fA-F]*) return 0 ;;
    esac
    branch=$(git -C "$repo_root" symbolic-ref --quiet --short HEAD 2>/dev/null || true)
    repository=$(git -C "$repo_root" remote get-url origin 2>/dev/null || true)
    status_line=$(git -C "$repo_root" status --porcelain=v1 --untracked-files=normal 2>/dev/null | head -n 1 || true)
    if [ -n "$status_line" ]; then
        dirty=1
    else
        dirty=0
    fi
    printf 'W\t%s\t%s\t%s\t%s\t%s\n' "$repo_root" "$repository" "$branch" "$commit" "$dirty"
}

inspect_root() {
    configured=$1
    case "$configured" in
        '~') expanded=${HOME-} ;;
        '~/'*) expanded=${HOME-}/${configured#\~/} ;;
        /*) expanded=$configured ;;
        *) return 0 ;;
    esac
    [ -n "$expanded" ] || return 0
    canonical=$(realpath -e -- "$expanded" 2>/dev/null) || return 0
    [ -d "$canonical" ] || return 0

    inspect_repo "$canonical" || true
    find -P "$canonical" -xdev -mindepth 1 -maxdepth 4 \( -type d -o -type f \) -name .git -print 2>/dev/null |
        while IFS= read -r marker; do
            case "$marker" in
                */.git) inspect_repo "${marker%/.git}" || true ;;
            esac
        done
}

"#,
    );

    for root in roots {
        // `validate_profile` has already rejected control characters.  The
        // shell quote remains necessary for spaces, `$`, backslashes and
        // apostrophes in otherwise valid remote paths.
        let _ = writeln!(script, "inspect_root {}", shell_quote(root));
    }
    script
}

fn parse_discovery_output(output: &[u8]) -> Result<Vec<Workspace>> {
    let output = std::str::from_utf8(output).context("SSH discovery returned invalid UTF-8")?;
    let mut workspaces = Vec::new();

    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 6 || fields[0] != "W" {
            bail!("SSH discovery returned a malformed workspace record");
        }

        let path = fields[1];
        validate_discovered_path(path)?;
        let repository = sanitize_repository(fields[2])?;
        let branch = if fields[3].is_empty() {
            None
        } else {
            validate_text_field("branch", fields[3], 256)?;
            Some(fields[3].to_string())
        };
        let commit = if fields[4].is_empty() {
            None
        } else {
            validate_commit(fields[4])?;
            Some(fields[4].to_ascii_lowercase())
        };
        let dirty = match fields[5] {
            "0" => false,
            "1" => true,
            _ => bail!("SSH discovery returned an invalid dirty-state flag"),
        };

        if workspaces
            .iter()
            .any(|workspace: &Workspace| workspace.path == path)
        {
            continue;
        }
        if workspaces.len() >= MAX_DISCOVERED_WORKSPACES {
            bail!("SSH discovery returned too many workspaces");
        }
        workspaces.push(Workspace {
            path: path.to_string(),
            repository,
            branch,
            commit,
            dirty,
        });
    }

    Ok(workspaces)
}

fn validate_text_field(field: &str, value: &str, max_length: usize) -> Result<()> {
    if value.len() > max_length || value.chars().any(char::is_control) {
        bail!("SSH discovery returned an invalid {field}");
    }
    Ok(())
}

fn validate_discovered_path(path: &str) -> Result<()> {
    validate_text_field("workspace path", path, MAX_ROOT_LENGTH)?;
    if !path.starts_with('/') {
        bail!("SSH discovery returned a non-absolute workspace path");
    }
    if path.split('/').any(|component| component == "..") {
        bail!("SSH discovery returned a traversing workspace path");
    }
    Ok(())
}

fn validate_commit(commit: &str) -> Result<()> {
    if !(commit.len() == 40 || commit.len() == 64)
        || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("SSH discovery returned an invalid commit");
    }
    Ok(())
}

fn sanitize_repository(value: &str) -> Result<Option<String>> {
    if value.is_empty() {
        return Ok(None);
    }
    validate_text_field("repository", value, MAX_ROOT_LENGTH)?;

    // Git's scp-like syntax (`user@host:path`) is not a URL.  Strip the
    // userinfo before returning it; this also removes `user:password@host`.
    if !value.contains("://") {
        if let Some((_, destination)) = value.rsplit_once('@') {
            let Some((host, path)) = destination.split_once(':') else {
                return Ok(None);
            };
            let path = path.split(['?', '#']).next().unwrap_or(path);
            if host.is_empty() || path.is_empty() {
                return Ok(None);
            }
            return Ok(Some(format!("{host}:{path}")));
        }
    }

    if let Ok(mut url) = Url::parse(value) {
        // URL userinfo, query and fragment are common places for access
        // tokens/passwords.  Keep only a credential-free locator.
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        return Ok(Some(url.to_string()));
    }

    // A non-URL value containing userinfo is ambiguous and could retain a
    // credential.  Refuse it rather than returning it to the UI.
    if value.contains('@') || value.contains("//") {
        return Ok(None);
    }
    Ok(Some(value.to_string()))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::Path;

    use super::*;

    fn execute_local_shell(script: &str) -> std::process::Output {
        let mut command = std::process::Command::new("sh");
        command
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("local sh should be installed");
        child
            .stdin
            .take()
            .expect("local sh stdin should be piped")
            .write_all(script.as_bytes())
            .expect("probe script should be writable");
        child
            .wait_with_output()
            .expect("local sh should produce output")
    }

    fn run_git(cwd: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should be installed for the fixture")
    }

    fn profile() -> MachineProfile {
        MachineProfile {
            id: "dev-vm".to_string(),
            name: "Dev VM".to_string(),
            host: "dev-vm".to_string(),
            user: Some("builder".to_string()),
            port: 2222,
            workspace_roots: vec!["~/src".to_string()],
        }
    }

    #[test]
    fn ssh_args_use_strict_noninteractive_defaults_and_fixed_script() {
        let args = ssh_args(&profile()).expect("valid profile");
        assert_eq!(
            args,
            vec![
                "-T".to_string(),
                "-o".to_string(),
                "BatchMode=yes".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=yes".to_string(),
                "-o".to_string(),
                "ConnectTimeout=10".to_string(),
                "-o".to_string(),
                "ConnectionAttempts=1".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "--".to_string(),
                "builder@dev-vm".to_string(),
                "sh".to_string(),
                "-s".to_string(),
                "--".to_string(),
            ]
        );
    }

    #[test]
    fn connection_probe_script_emits_the_expected_newline_terminated_response() {
        let output = execute_local_shell(CONNECTION_PROBE_SCRIPT);
        assert!(
            output.status.success(),
            "probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, CONNECTION_OK.as_bytes());
    }

    #[test]
    fn ipv6_target_is_bracketed_without_becoming_an_option() {
        let mut profile = profile();
        profile.host = "2001:db8::10".to_string();
        assert_eq!(ssh_target(&profile), "builder@[2001:db8::10]");
        profile.host = "2001:db8:::10".to_string();
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn malformed_host_user_port_and_identifier_are_rejected() {
        let mut profile = profile();

        profile.host = "-oProxyCommand=bad".to_string();
        assert!(validate_profile(&profile).is_err());
        profile.host = "dev vm".to_string();
        assert!(validate_profile(&profile).is_err());
        profile.host = "dev-vm".to_string();

        profile.user = Some("builder;touch /tmp/pwned".to_string());
        assert!(validate_profile(&profile).is_err());
        profile.user = Some("builder".to_string());

        profile.port = 0;
        assert!(validate_profile(&profile).is_err());
        profile.port = 2222;

        profile.id = "../machine".to_string();
        assert!(validate_profile(&profile).is_err());
    }

    #[test]
    fn malformed_or_unsafe_workspace_roots_are_rejected() {
        for root in [
            "relative",
            "~/../secret",
            "/srv/../secret",
            "/",
            "//",
            "~other/src",
            "~/src\n",
        ] {
            let mut profile = profile();
            profile.workspace_roots = vec![root.to_string()];
            assert!(
                validate_profile(&profile).is_err(),
                "root should fail: {root:?}"
            );
        }
    }

    #[test]
    fn roots_are_shell_quoted_inside_the_fixed_remote_script() {
        let roots = vec!["/srv/a path/it's-safe".to_string()];
        let script = discovery_script(&roots);
        assert!(script.contains("inspect_root '/srv/a path/it'\\''s-safe'"));
        assert!(script.contains("realpath -e -- \"$expanded\""));
        assert!(script.contains("find -P \"$canonical\" -xdev"));
        assert!(!script.contains("ssh /srv/a path/it's-safe"));
    }

    #[test]
    fn discovery_script_reports_a_dirty_git_fixture_without_following_links() {
        let fixture = tempfile::tempdir().expect("temporary fixture directory");
        let repo = fixture.path().join("repo");
        std::fs::create_dir(&repo).expect("repository directory");

        let initialized = run_git(&repo, &["init", "-q", "-b", "main", "."]);
        assert!(
            initialized.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&initialized.stderr)
        );
        for (key, value) in [
            ("user.name", "App Manager Test"),
            ("user.email", "app-manager@example.test"),
            ("commit.gpgSign", "false"),
        ] {
            let configured = run_git(&repo, &["config", key, value]);
            assert!(configured.status.success(), "git config failed");
        }
        std::fs::write(repo.join("README.md"), "clean\n").expect("fixture file");
        assert!(run_git(&repo, &["add", "README.md"]).status.success());
        assert!(
            run_git(&repo, &["commit", "-q", "--no-verify", "-m", "initial"])
                .status
                .success()
        );
        let commit_output = run_git(&repo, &["rev-parse", "HEAD"]);
        assert!(commit_output.status.success());
        let commit = String::from_utf8(commit_output.stdout)
            .expect("commit should be UTF-8")
            .trim()
            .to_string();
        assert_eq!(commit.len(), 40);

        let remote = run_git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://token:secret@example.com/org/repo.git?token=again",
            ],
        );
        assert!(remote.status.success());
        std::fs::write(repo.join("README.md"), "dirty\n").expect("dirty fixture file");

        let outside = fixture.path().join("outside");
        std::fs::create_dir(&outside).expect("outside directory");
        assert!(run_git(&outside, &["init", "-q", "-b", "main", "."])
            .status
            .success());
        assert!(
            run_git(&outside, &["config", "user.name", "Outside Fixture"])
                .status
                .success()
        );
        assert!(
            run_git(&outside, &["config", "user.email", "outside@example.test"])
                .status
                .success()
        );
        std::fs::write(outside.join("outside.txt"), "outside\n").expect("outside fixture file");
        assert!(run_git(&outside, &["add", "outside.txt"]).status.success());
        assert!(
            run_git(&outside, &["commit", "-q", "--no-verify", "-m", "outside"])
                .status
                .success()
        );
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, repo.join("linked-outside")).expect("symlink fixture");

        let root = repo.to_string_lossy().into_owned();
        let output = execute_local_shell(&discovery_script(&[root]));
        assert!(
            output.status.success(),
            "discovery failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let workspaces = parse_discovery_output(&output.stdout).expect("valid discovery records");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(
            workspaces[0].path,
            repo.canonicalize().unwrap().to_string_lossy().to_string()
        );
        assert_eq!(workspaces[0].branch.as_deref(), Some("main"));
        assert_eq!(workspaces[0].commit.as_deref(), Some(commit.as_str()));
        assert!(
            workspaces[0].dirty,
            "modified tracked file must be reported dirty"
        );
        assert_eq!(
            workspaces[0].repository.as_deref(),
            Some("https://example.com/org/repo.git")
        );
    }

    #[test]
    fn discovery_output_parses_branch_commit_dirty_and_deduplicates() {
        let output = concat!(
            "W\t/home/builder/src/app\thttps://example.com/org/app.git\tmain\t",
            "0123456789012345678901234567890123456789\t1\n",
            "W\t/home/builder/src/app\thttps://example.com/org/app.git\tmain\t",
            "0123456789012345678901234567890123456789\t1\n",
            "W\t/home/builder/src/other\tgit@github.com:org/other.git\t\t",
            "abcdefabcdefabcdefabcdefabcdefabcdefabcd\t0\n",
        );
        let workspaces = parse_discovery_output(output.as_bytes()).expect("valid records");
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].branch.as_deref(), Some("main"));
        assert_eq!(
            workspaces[0].commit.as_deref(),
            Some("0123456789012345678901234567890123456789")
        );
        assert!(workspaces[0].dirty);
        assert_eq!(
            workspaces[1].repository.as_deref(),
            Some("github.com:org/other.git")
        );
        assert_eq!(workspaces[1].branch, None);
        assert!(!workspaces[1].dirty);
    }

    #[test]
    fn discovery_rejects_traversal_invalid_commit_and_malformed_fields() {
        let traversal =
            b"W\t/home/builder/../secret\t\tmain\t0123456789012345678901234567890123456789\t0\n";
        assert!(parse_discovery_output(traversal).is_err());

        let invalid_commit = b"W\t/home/builder/src\t\tmain\tnope\t0\n";
        assert!(parse_discovery_output(invalid_commit).is_err());

        let malformed =
            b"W\t/home/builder/src\t\tmain\t0123456789012345678901234567890123456789\t2\n";
        assert!(parse_discovery_output(malformed).is_err());
    }

    #[test]
    fn repository_credentials_are_redacted_or_refused() {
        assert_eq!(
            sanitize_repository("https://token:password@example.com/org/app.git?secret=1")
                .expect("URL should parse")
                .as_deref(),
            Some("https://example.com/org/app.git")
        );
        assert_eq!(
            sanitize_repository("user:password@example.com:org/app.git")
                .expect("scp-like URL should parse")
                .as_deref(),
            Some("example.com:org/app.git")
        );
        assert_eq!(
            sanitize_repository("git@example.com:org/app.git")
                .expect("standard scp-like URL should parse")
                .as_deref(),
            Some("example.com:org/app.git")
        );
        assert_eq!(
            sanitize_repository("opaque@credential-bearing-value")
                .expect("ambiguous value is refused"),
            None
        );
    }

    #[test]
    fn shell_quote_does_not_execute_apostrophes_or_dollar_expressions() {
        let quoted = shell_quote("$(touch /tmp/pwned);it's safe");
        assert_eq!(quoted, "'$(touch /tmp/pwned);it'\\''s safe'");
    }
}
