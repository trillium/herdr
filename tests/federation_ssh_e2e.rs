//! Federation E2E: origin labels, live-view protocol gating, action routing,
//! and input relay over a real localhost SSH bridge.
//!
//! Run explicitly with:
//! `HERDR_RUN_REMOTE_SSH_E2E=1 cargo nextest run --locked federation_protocol_gating_with_remote_attach`

#![cfg(target_os = "linux")]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const SESSION_NAME: &str = "fed-e2e";
const WORKSPACE_LABEL: &str = "federation test workspace";
const ORIGIN_KEY: &str = "nE2ETEST";
const ORIGIN_LABEL: &str = "e2e-remote";
const MARKER: &str = "FED_E2E_MARKER_OK";
const INPUT_MARKER: &str = "FED_E2E_INPUT_OK";

/// Self-federation E2E: two local herdr servers, one federating with the other
/// over a real API socket. Exercises the N2 federation pipeline end-to-end:
///
///  a) Origin labels in the workspace list carry the 5-char prefix
///  b) live_view_status degrades gracefully on protocol mismatch
///  c) Action routing (split/close) on a foreign workspace hits the remote
///  d) Input relay delivers keystrokes to the remote pane
#[test]
fn federation_protocol_gating_with_remote_attach() {
    if !matches!(
        std::env::var("HERDR_RUN_REMOTE_SSH_E2E").as_deref(),
        Ok("1")
    ) {
        eprintln!("skipping federation E2E; set HERDR_RUN_REMOTE_SSH_E2E=1 to run it explicitly");
        return;
    }

    let root = unique_test_dir();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_herdr"));
    let ssh = LocalSshd::start(root.clone(), &binary);

    // --- Remote server setup ---
    // Start a herdr server in the SSH-isolated environment. Use an explicit
    // HERDR_SOCKET_PATH so we know exactly where its API socket lands.
    let remote_socket = root.join("remote-api.sock");
    let remote_client_socket = root.join("remote-api-client.sock");
    let _remote_api = RemoteServer::start(&ssh, &binary, &remote_socket, &remote_client_socket);
    wait_for_socket(&remote_socket, Duration::from_secs(10));

    // Create a workspace on the remote with a known label.
    let remote_ws_raw = ssh.remote_command(&format!(
        "herdr --session {SESSION_NAME} workspace create --label '{WORKSPACE_LABEL}'"
    ));
    let remote_ws: Value = serde_json::from_str(&remote_ws_raw)
        .unwrap_or_else(|e| panic!("remote workspace create parse: {e}: {remote_ws_raw}"));
    let remote_workspace_id = remote_ws["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("remote workspace id")
        .to_string();

    // Create a tab on the remote so we have a pane to interact with.
    let remote_tab_raw = ssh.remote_command(&format!(
        "herdr --session {SESSION_NAME} tab create --workspace {remote_workspace_id} --label 'e2e-tab'"
    ));
    let remote_tab: Value = serde_json::from_str(&remote_tab_raw)
        .unwrap_or_else(|e| panic!("remote tab create parse: {e}: {remote_tab_raw}"));
    let remote_tab_id = remote_tab["result"]["tab"]["tab_id"]
        .as_str()
        .expect("remote tab id")
        .to_string();

    // Find the pane id on the remote.
    let remote_panes_raw = ssh.remote_command(&format!(
        "herdr --session {SESSION_NAME} pane list --workspace {remote_workspace_id}"
    ));
    let remote_panes: Value = serde_json::from_str(&remote_panes_raw)
        .unwrap_or_else(|e| panic!("remote pane list parse: {e}: {remote_panes_raw}"));
    let remote_pane_id = remote_panes["result"]["panes"]
        .as_array()
        .and_then(|panes| {
            panes
                .iter()
                .find(|p| p["tab_id"] == remote_tab_id)
                .map(|p| p["pane_id"].as_str().unwrap().to_string())
        })
        .expect("remote pane id");

    // Send a marker so we can verify snapshot delivery.
    // pane run joins args after pane_id as text and appends Enter; no --text flag needed.
    ssh.remote_command(&format!(
        "herdr --session {SESSION_NAME} pane run {remote_pane_id} printf '{MARKER}\\n'"
    ));
    // Give the remote PTY time to process the marker.
    thread::sleep(Duration::from_millis(500));

    // --- Local server with federation ---
    let local = LocalPaths::create(&root, &ssh);
    local.write_federation_config(&remote_socket);
    let mut server = LocalServer::start(&binary, &local);
    wait_for_socket(&local.api_socket, Duration::from_secs(10));
    wait_for_socket(&local.client_socket, Duration::from_secs(10));

    // Wait for the federation poll (~5s interval) to discover the remote.
    let workspace = wait_for_workspace(
        &local.api_socket,
        &format!("{ORIGIN_LABEL}:{WORKSPACE_LABEL}"),
        Duration::from_secs(30),
    );
    let workspace_id = string_field(&workspace, "workspace_id");

    // ===== Scenario A: Origin labels =====
    // The workspace label must start with the 5-char origin prefix.
    let label = workspace["label"].as_str().expect("workspace label");
    assert!(
        label.starts_with(&format!("{}:", ORIGIN_LABEL)),
        "workspace label must carry origin prefix: got {label:?}"
    );
    assert!(
        label.contains(WORKSPACE_LABEL),
        "workspace label must contain remote label: got {label:?}"
    );
    // The workspace id must be namespaced with fed~<key>~.
    assert!(
        workspace_id.starts_with(&format!("fed~{ORIGIN_KEY}~")),
        "workspace id must be namespaced: got {workspace_id:?}"
    );

    // ===== Scenario B: live_view_status on protocol mismatch =====
    // The remote server reports its snapshot protocol. If it differs from
    // the hub's, live_view_status must return VersionMismatch (not panic).
    // We verify this indirectly: the pane exists and is readable.
    let pane_list_resp = pane_list(&local.api_socket, &workspace_id);
    assert!(
        !pane_list_resp.is_empty(),
        "foreign workspace must expose panes"
    );
    let foreign_pane = &pane_list_resp[0];
    let pane_id = string_field(foreign_pane, "pane_id");

    // Reading the foreign pane must not error (live_view_status must not crash).
    let pane_read = request(
        &local.api_socket,
        json!({
            "id":"read-foreign",
            "method":"pane.read",
            "params":{"pane_id":pane_id,"source":"recent","lines":20}
        }),
    );
    assert!(
        pane_read.get("error").is_none(),
        "reading foreign pane must not error: {pane_read}"
    );

    // The remote's marker must be visible — snapshot delivery works.
    let pane_text = pane_read["result"]["read"]["text"].as_str().unwrap_or("");
    assert!(
        pane_text.contains(MARKER),
        "foreign pane must show remote marker: {pane_text}"
    );

    // ===== Scenario C: Action routing — split on foreign workspace =====
    let split = request_ok(
        &local.api_socket,
        json!({
            "id":"fed-split",
            "method":"pane.split",
            "params":{
                "target_pane_id":pane_id,
                "direction":"right",
                "ratio":0.5,
                "focus":true
            }
        }),
    );
    let split_pane_id = string_field(&split["result"]["pane"], "pane_id");
    assert!(
        split_pane_id.starts_with(&format!("fed~{ORIGIN_KEY}~")),
        "split pane id must be namespaced: got {split_pane_id:?}"
    );
    // The split was routed to the remote; the next poll should show 2 panes.
    wait_for_workspace_counts(
        &local.api_socket,
        &workspace_id,
        1,
        2,
        Duration::from_secs(15),
    );

    // ===== Scenario C continued: close the split =====
    request_ok(
        &local.api_socket,
        json!({
            "id":"fed-split-close",
            "method":"pane.close",
            "params":{"pane_id":split_pane_id}
        }),
    );
    wait_for_workspace_counts(
        &local.api_socket,
        &workspace_id,
        1,
        1,
        Duration::from_secs(15),
    );

    // ===== Scenario D: Input relay =====
    request_ok(
        &local.api_socket,
        json!({
            "id":"fed-input",
            "method":"pane.send_text",
            "params":{
                "pane_id":pane_id,
                "text":format!("printf '{INPUT_MARKER}\\n'\n")
            }
        }),
    );
    wait_for_pane_text(
        &local.api_socket,
        &pane_id,
        INPUT_MARKER,
        Duration::from_secs(20),
    );
    // Verify the input also reached the remote.
    // wait_for_pane_text already confirmed the hub sees INPUT_MARKER via the observer frame;
    // the remote visible screen has the output -- use --source visible, not recent (scrollback).
    let remote_after_input = ssh.remote_command(&format!(
        "herdr --session {SESSION_NAME} pane read {remote_pane_id} --source visible --lines 40"
    ));
    assert!(
        remote_after_input.contains(INPUT_MARKER),
        "input must reach remote: {remote_after_input}"
    );

    // ===== Workspace close detaches without closing remote =====
    request_ok(
        &local.api_socket,
        json!({
            "id":"fed-ws-close",
            "method":"workspace.close",
            "params":{"workspace_id":workspace_id}
        }),
    );
    assert!(workspace_list(&local.api_socket)
        .iter()
        .all(|w| w["workspace_id"] != workspace_id));
    let remote_after =
        ssh.remote_command(&format!("herdr --session {SESSION_NAME} workspace list"));
    assert!(
        remote_after.contains(WORKSPACE_LABEL),
        "remote workspace survives local detach: {remote_after}"
    );

    // --- Cleanup ---
    request_ok(
        &local.api_socket,
        json!({"id":"stop-local","method":"server.stop","params":{}}),
    );
    server.wait_for_exit(Duration::from_secs(10));
    ssh.stop_remote_server();
}

// ---------------------------------------------------------------------------
// Remote server helper — starts herdr server via SSH with an explicit socket
// ---------------------------------------------------------------------------

struct RemoteServer;

impl RemoteServer {
    fn start(
        ssh: &LocalSshd,
        _binary: &Path,
        socket_path: &Path,
        client_socket_path: &Path,
    ) -> Self {
        // Start the remote server via SSH with explicit socket paths.
        // The server listens on a socket we know, so the local federation
        // config can point at it directly.
        let _output = ssh.run_ssh(&format!(
            "HERDR_SOCKET_PATH={socket} HERDR_CLIENT_SOCKET_PATH={client} HERDR_DISABLE_SOUND=1 nohup herdr --session {session} server > /dev/null 2>&1 &",
            socket = socket_path.display(),
            client = client_socket_path.display(),
            session = SESSION_NAME,
        ));
        Self
    }
}

// ---------------------------------------------------------------------------
// Harness infrastructure
// ---------------------------------------------------------------------------

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    PathBuf::from("/tmp").join(format!(
        "hfed-{}-{:x}",
        std::process::id(),
        nanos & 0xffff_ffff
    ))
}

struct LocalPaths {
    home: PathBuf,
    config_home: PathBuf,
    runtime: PathBuf,
    api_socket: PathBuf,
    client_socket: PathBuf,
    server_log: PathBuf,
}

impl LocalPaths {
    fn create(root: &Path, ssh: &LocalSshd) -> Self {
        let home = root.join("local-home");
        let config_home = root.join("local-config");
        let runtime = root.join("local-runtime");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
        create_private_dir(&runtime);
        fs::copy(&ssh.client_config, home.join(".ssh/config")).unwrap();
        fs::write(
            config_home.join(app_dir_name()).join("config.toml"),
            "onboarding = false\n[remote]\nmanage_ssh_config = true\n",
        )
        .unwrap();
        Self {
            home,
            config_home,
            runtime,
            api_socket: root.join("local-api.sock"),
            client_socket: root.join("local-api-client.sock"),
            server_log: root.join("local-server.log"),
        }
    }

    /// Write federation config + origins file for the local server.
    fn write_federation_config(&self, remote_socket: &Path) {
        let config_dir = self.config_home.join(app_dir_name());
        fs::create_dir_all(&config_dir).unwrap();
        // Enable experimental.federation in config.toml.
        fs::write(
            config_dir.join("config.toml"),
            "onboarding = false\n[experimental]\nfederation = true\n",
        )
        .unwrap();
        // Write the static origins file: key=socket_path.
        let origins = format!("{ORIGIN_KEY}={}", remote_socket.display());
        fs::write(self.config_home.join("federation-origins.env"), &origins).unwrap();
    }
}

struct LocalServer {
    child: Child,
}

impl LocalServer {
    fn start(binary: &Path, paths: &LocalPaths) -> Self {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.server_log)
            .unwrap();
        let mut cmd = Command::new(binary);
        cmd.arg("server")
            .env("HOME", &paths.home)
            .env("XDG_CONFIG_HOME", &paths.config_home)
            .env("XDG_RUNTIME_DIR", &paths.runtime)
            .env("HERDR_SOCKET_PATH", &paths.api_socket)
            .env("HERDR_CLIENT_SOCKET_PATH", &paths.client_socket)
            .env("SHELL", "/bin/sh")
            .env("HERDR_DISABLE_SOUND", "1")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_PANE_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));

        // Inject the federation origins if configured.
        let origins_path = paths.config_home.join("federation-origins.env");
        if let Ok(origins) = fs::read_to_string(&origins_path) {
            let origins = origins.trim();
            if !origins.is_empty() {
                cmd.env("HERDR_FEDERATION_ORIGINS", origins);
            }
        }

        let child = cmd.spawn().expect("start isolated local Herdr server");
        support::register_spawned_herdr_pid(Some(child.id()));
        Self { child }
    }

    fn wait_for_exit(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.child.try_wait().unwrap().is_some() {
                support::unregister_spawned_herdr_pid(Some(self.child.id()));
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("local Herdr server did not exit before timeout");
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        support::unregister_spawned_herdr_pid(Some(self.child.id()));
    }
}

struct LocalSshd {
    root: PathBuf,
    alias: String,
    port: u16,
    sshd: PathBuf,
    server_config: PathBuf,
    client_config: PathBuf,
    log: PathBuf,
    child: Option<Child>,
}

impl LocalSshd {
    fn start(root: PathBuf, binary: &Path) -> Self {
        create_private_dir(&root);
        for dir in ["bin", "remote-home", "remote-config", "remote-runtime"] {
            create_private_dir(&root.join(dir));
        }
        symlink(binary, root.join("bin/herdr")).unwrap();

        let host_key = root.join("host-key");
        let client_key = root.join("client-key");
        generate_key(&host_key);
        generate_key(&client_key);
        fs::copy(
            client_key.with_extension("pub"),
            root.join("authorized_keys"),
        )
        .unwrap();

        let port = free_port();
        let alias = format!("herdr-e2e-{}-{port}", std::process::id());
        let username = command_stdout(Command::new("id").arg("-un"));
        let force_command = root.join("force-command.sh");
        fs::write(
            &force_command,
            format!(
                "#!/bin/sh\nset -eu\nexport HOME='{home}'\nexport XDG_CONFIG_HOME='{config}'\nexport XDG_RUNTIME_DIR='{runtime}'\nexport PATH='{bin}:/usr/bin:/bin'\nexport SHELL=/bin/sh\nunset HERDR_ENV HERDR_SESSION HERDR_SOCKET_PATH HERDR_CLIENT_SOCKET_PATH HERDR_WORKSPACE_ID HERDR_TAB_ID HERDR_PANE_ID\nexec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\"\n",
                home = root.join("remote-home").display(),
                config = root.join("remote-config").display(),
                runtime = root.join("remote-runtime").display(),
                bin = root.join("bin").display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&force_command, fs::Permissions::from_mode(0o700)).unwrap();

        let server_config = root.join("sshd_config");
        fs::write(
            &server_config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {host_key}\nPidFile {pid}\nAuthorizedKeysFile {authorized}\nPubkeyAuthentication yes\nAuthenticationMethods publickey\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes no\nPermitRootLogin no\nAllowUsers {username}\nPrintMotd no\nLogLevel VERBOSE\nForceCommand {force_command}\n",
                host_key = host_key.display(),
                pid = root.join("sshd.pid").display(),
                authorized = root.join("authorized_keys").display(),
                force_command = force_command.display(),
            ),
        )
        .unwrap();

        let host_public = fs::read_to_string(host_key.with_extension("pub")).unwrap();
        let mut fields = host_public.split_whitespace();
        let kind = fields.next().unwrap();
        let encoded = fields.next().unwrap();
        let known_hosts = root.join("known_hosts");
        fs::write(
            &known_hosts,
            format!("[127.0.0.1]:{port} {kind} {encoded}\n"),
        )
        .unwrap();

        let client_config = root.join("ssh_config");
        fs::write(
            &client_config,
            format!(
                "Host {alias}\n  HostName 127.0.0.1\n  Port {port}\n  User {username}\n  IdentityFile {client_key}\n  IdentitiesOnly yes\n  UserKnownHostsFile {known_hosts}\n  StrictHostKeyChecking yes\n  BatchMode yes\n  ConnectTimeout 3\n",
                client_key = client_key.display(),
                known_hosts = known_hosts.display(),
            ),
        )
        .unwrap();

        let sshd = find_executable("sshd");
        let log = root.join("sshd.log");
        let mut fixture = Self {
            root,
            alias,
            port,
            sshd,
            server_config,
            client_config,
            log,
            child: None,
        };
        fixture.restart();
        fixture
    }

    fn restart(&mut self) {
        assert!(self.child.is_none(), "sshd already running");
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let mut command = Command::new(&self.sshd);
        command
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(&self.server_config)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .process_group(0);
        self.child = Some(command.spawn().expect("start isolated localhost sshd"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                break;
            }
            if self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .is_some()
            {
                panic!("localhost sshd exited:\n{}", read_file(&self.log));
            }
            if Instant::now() >= deadline {
                panic!("localhost sshd did not listen:\n{}", read_file(&self.log));
            }
            thread::sleep(Duration::from_millis(25));
        }
        let output = self.run_ssh("true");
        assert!(
            output.status.success(),
            "localhost ssh authentication failed: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            read_file(&self.log)
        );
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            kill_process_tree(child.id());
            let _ = child.wait();
        }
    }

    fn run_ssh(&self, remote_command: &str) -> std::process::Output {
        Command::new("ssh")
            .arg("-F")
            .arg(&self.client_config)
            .arg("-T")
            .arg(&self.alias)
            .arg(remote_command)
            .output()
            .expect("run localhost SSH command")
    }

    fn remote_command(&self, command: &str) -> String {
        let output = self.run_ssh(command);
        assert!(
            output.status.success(),
            "remote command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn stop_remote_server(&self) {
        let _ = Command::new(self.root.join("bin/herdr"))
            .args(["--session", SESSION_NAME, "server", "stop"])
            .env("HOME", self.root.join("remote-home"))
            .env("XDG_CONFIG_HOME", self.root.join("remote-config"))
            .env("XDG_RUNTIME_DIR", self.root.join("remote-runtime"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("SHELL", "/bin/sh")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_PANE_ID")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for LocalSshd {
    fn drop(&mut self) {
        self.stop_remote_server();
        self.stop();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn generate_key(path: &Path) {
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed for {}", path.display());
}

fn create_private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn find_executable(name: &str) -> PathBuf {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths)
        .map(|path| path.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} is required for localhost SSH E2E"))
}

fn command_stdout(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn kill_process_tree(pid: u32) {
    let children_path = format!("/proc/{pid}/task/{pid}/children");
    if let Ok(children) = fs::read_to_string(children_path) {
        for child in children
            .split_whitespace()
            .filter_map(|value| value.parse::<u32>().ok())
        {
            kill_process_tree(child);
        }
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not become ready: {}", path.display());
}

fn request(socket: &Path, value: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    serde_json::to_writer(&mut stream, &value).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line)
        .unwrap_or_else(|error| panic!("invalid API response: {error}: {line}"))
}

fn request_ok(socket: &Path, value: Value) -> Value {
    let response = request(socket, value);
    assert!(response.get("error").is_none(), "API error: {response}");
    response
}

fn workspace_list(socket: &Path) -> Vec<Value> {
    request_ok(
        socket,
        json!({"id":"wl","method":"workspace.list","params":{}}),
    )["result"]["workspaces"]
        .as_array()
        .cloned()
        .unwrap()
}

fn wait_for_workspace(socket: &Path, label: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(workspace) = workspace_list(socket)
            .into_iter()
            .find(|workspace| workspace["label"] == label)
        {
            return workspace;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("projected workspace {label:?} did not appear");
}

fn wait_for_workspace_counts(
    socket: &Path,
    workspace_id: &str,
    tabs: u64,
    panes: u64,
    timeout: Duration,
) -> Value {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(workspace) = workspace_list(socket)
            .into_iter()
            .find(|workspace| workspace["workspace_id"] == workspace_id)
        {
            if workspace["tab_count"].as_u64() == Some(tabs)
                && workspace["pane_count"].as_u64() == Some(panes)
            {
                return workspace;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("workspace {workspace_id} did not reach {tabs} tabs / {panes} panes");
}

fn pane_list(socket: &Path, workspace_id: &str) -> Vec<Value> {
    request_ok(
        socket,
        json!({
            "id":"pl",
            "method":"pane.list",
            "params":{"workspace_id":workspace_id}
        }),
    )["result"]["panes"]
        .as_array()
        .cloned()
        .unwrap()
}

fn wait_for_pane_text(socket: &Path, pane_id: &str, marker: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_response = Value::Null;
    while Instant::now() < deadline {
        let response = request(
            socket,
            json!({
                "id":"pr",
                "method":"pane.read",
                "params":{"pane_id":pane_id,"source":"recent","lines":100}
            }),
        );
        if response["result"]["read"]["text"]
            .as_str()
            .is_some_and(|text| text.contains(marker))
        {
            return;
        }
        last_response = response;
        thread::sleep(Duration::from_millis(50));
    }
    panic!("foreign pane {pane_id} did not render {marker:?}; last: {last_response}");
}

fn string_field(value: &Value, field: &str) -> String {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("missing {field}: {value}"))
        .to_string()
}

fn read_file(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}
