//! Federation E2E: two local herdr servers — one remote origin, one hub federating with it.
//!
//! Exercises the full N3 federation pipeline without SSH isolation:
//! origin labels, snapshot polling, frame delivery, action routing, and input relay.
//!
//! Run explicitly with:
//! `HERDR_RUN_LOCAL_E2E=1 cargo nextest run --locked federation_local_e2e`

#![cfg(not(target_os = "windows"))]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const REMOTE_SESSION: &str = "fed-e2e-remote";
const HUB_SESSION: &str = "fed-e2e-hub";
const WORKSPACE_LABEL: &str = "federation-test-workspace";
const ORIGIN_KEY: &str = "nE2ETEST";
const ORIGIN_LABEL: &str = "nE2ET";
const MARKER: &str = "FED_E2E_MARKER_OK";
const INPUT_MARKER: &str = "FED_E2E_INPUT_OK";

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

/// Full N3 federation pipeline between two local herdr servers.
///
/// a) Origin labels carry the 5-char prefix
/// b) Foreign pane content (snapshot + frames) is visible on the hub
/// c) Action routing (split/close) on a foreign workspace hits the remote
/// d) Input relay delivers keystrokes to the remote pane
#[test]
fn federation_local_pipeline() {
    if !matches!(std::env::var("HERDR_RUN_LOCAL_E2E").as_deref(), Ok("1")) {
        eprintln!("skipping federation local E2E; set HERDR_RUN_LOCAL_E2E=1 to run it explicitly");
        return;
    }

    let root = unique_test_dir();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_herdr"));

    // --- Remote server setup ---
    let remote = ServerEnv::create(&root, "remote");
    let mut remote_server = HerdrServer::start(&binary, &remote, REMOTE_SESSION);
    let remote_api_socket = remote.session_socket(REMOTE_SESSION, "herdr.sock");
    wait_for_socket(&remote_api_socket, Duration::from_secs(10));

    // Create workspace on remote.
    let remote_ws_raw = cli_request(
        &binary,
        &remote,
        REMOTE_SESSION,
        &["workspace", "create", "--label", WORKSPACE_LABEL],
    );
    let remote_ws: Value = serde_json::from_str(&remote_ws_raw)
        .unwrap_or_else(|e| panic!("remote workspace create parse: {e}: {remote_ws_raw}"));
    let remote_workspace_id = remote_ws["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("remote workspace id")
        .to_string();

    // Create a tab on the remote.
    let remote_tab_raw = cli_request(
        &binary,
        &remote,
        REMOTE_SESSION,
        &[
            "tab",
            "create",
            "--workspace",
            &remote_workspace_id,
            "--label",
            "e2e-tab",
        ],
    );
    let remote_tab: Value = serde_json::from_str(&remote_tab_raw)
        .unwrap_or_else(|e| panic!("remote tab create parse: {e}: {remote_tab_raw}"));
    let remote_tab_id = remote_tab["result"]["tab"]["tab_id"]
        .as_str()
        .expect("remote tab id")
        .to_string();

    // Find the pane on the remote.
    let remote_panes_raw = cli_request(
        &binary,
        &remote,
        REMOTE_SESSION,
        &["pane", "list", "--workspace", &remote_workspace_id],
    );
    let remote_panes: Value = serde_json::from_str(&remote_panes_raw)
        .unwrap_or_else(|e| panic!("remote pane list parse: {e}: {remote_panes_raw}"));
    let (remote_pane_id, remote_pane_terminal_id) = remote_panes["result"]["panes"]
        .as_array()
        .and_then(|panes| {
            panes
                .iter()
                .find(|p| p["tab_id"] == remote_tab_id)
                .map(|p| {
                    (
                        p["pane_id"].as_str().unwrap().to_string(),
                        p["terminal_id"].as_str().unwrap().to_string(),
                    )
                })
        })
        .expect("remote pane id and terminal_id");

    // Send a marker to the remote pane so we can verify snapshot delivery.
    cli_request(
        &binary,
        &remote,
        REMOTE_SESSION,
        &[
            "pane",
            "send-text",
            &remote_pane_id,
            "--text",
            &format!("printf {MARKER}\\n"),
        ],
    );
    thread::sleep(Duration::from_millis(500));

    // --- Hub server with federation ---
    let hub = ServerEnv::create(&root, "hub");
    hub.write_federation_config(&remote_api_socket);
    let mut hub_server = HerdrServer::start(&binary, &hub, HUB_SESSION);
    let hub_api_socket = hub.session_socket(HUB_SESSION, "herdr.sock");
    let hub_client_socket = hub.session_socket(HUB_SESSION, "herdr-client.sock");
    wait_for_socket(&hub_api_socket, Duration::from_secs(10));
    wait_for_socket(&hub_client_socket, Duration::from_secs(10));

    // Wait for federation poll (~5s) to discover the remote workspace.
    let workspace = wait_for_workspace(
        &hub_api_socket,
        &format!("{ORIGIN_LABEL}:{WORKSPACE_LABEL}"),
        Duration::from_secs(30),
    );
    let workspace_id = string_field(&workspace, "workspace_id");

    // ===== Scenario A: Origin labels =====
    let label = workspace["label"].as_str().expect("workspace label");
    assert!(
        label.starts_with(&format!("{ORIGIN_LABEL}:")),
        "workspace label must carry origin prefix: got {label:?}"
    );
    assert!(
        label.contains(WORKSPACE_LABEL),
        "workspace label must contain remote label: got {label:?}"
    );
    assert!(
        workspace_id.starts_with(&format!("fed~{ORIGIN_KEY}~")),
        "workspace id must be namespaced: got {workspace_id:?}"
    );

    // ===== Scenario B: Foreign panes visible with correct metadata =====
    let pane_list_resp = pane_list(&hub_api_socket, &workspace_id);
    assert!(
        !pane_list_resp.is_empty(),
        "foreign workspace must expose panes"
    );
    let foreign_pane = &pane_list_resp[0];
    // The pane_id must be namespaced with fed~<key>~.
    assert!(
        foreign_pane["pane_id"]
            .as_str()
            .unwrap_or_default()
            .starts_with(&format!("fed~{ORIGIN_KEY}~")),
        "pane_id must be namespaced: got {:?}", foreign_pane["pane_id"]
    );
    // The terminal_id field must also be namespaced.
    let terminal_id = string_field(foreign_pane, "terminal_id");
    assert!(
        terminal_id.starts_with(&format!("fed~{ORIGIN_KEY}~")),
        "terminal_id must be namespaced: got {terminal_id:?}"
    );

    // Use the namespaced terminal_id of the e2e-tab pane as the stable pane
    // identifier for all subsequent scenarios. Hub pane numbers are reassigned
    // on every poll cycle; terminal_ids are stable and are now supported by
    // parse_pane_id via a direct scan.
    let pane_id = format!("fed~{ORIGIN_KEY}~{remote_pane_terminal_id}");

    // ===== Scenario C: Action routing — split on foreign workspace =====
    let pane_count_before = pane_list_resp.len() as u64;
    let tab_count_before = workspace_list(&hub_api_socket)
        .into_iter()
        .find(|ws| ws["workspace_id"] == workspace_id)
        .and_then(|ws| ws["tab_count"].as_u64())
        .unwrap_or(0);

    let split = api_request_ok(
        &hub_api_socket,
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
    // The terminal_id from the split response is the stable identifier for the
    // new pane. Hub pane numbers are reassigned on every poll cycle, but
    // terminal_ids are fixed and appear in pane.list responses.
    let split_terminal_id = string_field(&split["result"]["pane"], "terminal_id");

    // The split was routed to the remote; wait for the next poll to reflect it.
    wait_for_workspace_counts(
        &hub_api_socket,
        &workspace_id,
        tab_count_before,
        pane_count_before + 1,
        Duration::from_secs(15),
    );

    // Find the new pane by its stable terminal_id (hub pane numbers shift on
    // every set_foreign_rows call, so pane_id diff is unreliable).
    let pane_list_after_split = pane_list(&hub_api_socket, &workspace_id);
    let new_pane = pane_list_after_split
        .into_iter()
        .find(|p| p["terminal_id"].as_str().unwrap_or_default() == split_terminal_id)
        .expect("new pane must appear in hub pane list with matching terminal_id");
    let new_pane_id = string_field(&new_pane, "pane_id");
    assert!(
        new_pane_id.starts_with(&format!("fed~{ORIGIN_KEY}~")),
        "new pane id must be namespaced: got {new_pane_id:?}"
    );

    // Close the split pane using the hub-assigned pane_id.
    api_request_ok(
        &hub_api_socket,
        json!({
            "id":"fed-split-close",
            "method":"pane.close",
            "params":{"pane_id":new_pane_id}
        }),
    );
    wait_for_workspace_counts(
        &hub_api_socket,
        &workspace_id,
        tab_count_before,
        pane_count_before,
        Duration::from_secs(15),
    );

    // ===== Scenario D: Input relay =====
    api_request_ok(
        &hub_api_socket,
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
        &hub_api_socket,
        &pane_id,
        INPUT_MARKER,
        Duration::from_secs(20),
    );
    // Verify input also reached the remote.
    let remote_after_input = cli_request(
        &binary,
        &remote,
        REMOTE_SESSION,
        &[
            "pane",
            "read",
            &remote_pane_id,
            "--source",
            "recent",
            "--lines",
            "20",
        ],
    );
    assert!(
        remote_after_input.contains(INPUT_MARKER),
        "input must reach remote: {remote_after_input}"
    );

    // ===== Workspace close detaches without closing remote =====
    api_request_ok(
        &hub_api_socket,
        json!({
            "id":"fed-ws-close",
            "method":"workspace.close",
            "params":{"workspace_id":workspace_id}
        }),
    );
    assert!(workspace_list(&hub_api_socket)
        .iter()
        .all(|w| w["workspace_id"] != workspace_id));
    let remote_after = cli_request(&binary, &remote, REMOTE_SESSION, &["workspace", "list"]);
    assert!(
        remote_after.contains(WORKSPACE_LABEL),
        "remote workspace survives local detach: {remote_after}"
    );

    // --- Cleanup ---
    api_request_ok(
        &hub_api_socket,
        json!({"id":"stop-hub","method":"server.stop","params":{}}),
    );
    hub_server.wait_for_exit(Duration::from_secs(10));
    api_request_ok(
        &remote_api_socket,
        json!({"id":"stop-remote","method":"server.stop","params":{}}),
    );
    remote_server.wait_for_exit(Duration::from_secs(10));
}

// ---------------------------------------------------------------------------
// Server environment — isolated HOME/XDG/config per server
// ---------------------------------------------------------------------------

struct ServerEnv {
    name: &'static str,
    home: PathBuf,
    config_home: PathBuf,
    runtime: PathBuf,
}

impl ServerEnv {
    fn create(root: &Path, name: &'static str) -> Self {
        let home = root.join(format!("{name}-home"));
        let config_home = root.join(format!("{name}-config"));
        let runtime = root.join(format!("{name}-runtime"));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::set_permissions(
            &runtime,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        fs::write(
            config_home.join(app_dir_name()).join("config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
        Self {
            name,
            home,
            config_home,
            runtime,
        }
    }

    /// Derive the session-based socket path, matching herdr's own logic.
    fn session_socket(&self, session: &str, filename: &str) -> PathBuf {
        self.config_home
            .join(app_dir_name())
            .join("sessions")
            .join(session)
            .join(filename)
    }

    fn write_federation_config(&self, remote_socket: &Path) {
        let config_dir = self.config_home.join(app_dir_name());
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            "onboarding = false\n[experimental]\nfederation = true\n",
        )
        .unwrap();
        let origins = format!("{ORIGIN_KEY}={}", remote_socket.display());
        fs::write(config_dir.join("federation-origins.env"), &origins).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Herdr server process
// ---------------------------------------------------------------------------

struct HerdrServer {
    name: &'static str,
    child: Child,
}

impl HerdrServer {
    fn start(binary: &Path, env: &ServerEnv, session: &str) -> Self {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(env.runtime.join("server.log"))
            .unwrap();
        let mut cmd = Command::new(binary);
        cmd.arg("--session")
            .arg(session)
            .arg("server")
            .env("HOME", &env.home)
            .env("XDG_CONFIG_HOME", &env.config_home)
            .env("XDG_RUNTIME_DIR", &env.runtime)
            .env("SHELL", "/bin/sh")
            .env("HERDR_DISABLE_SOUND", "1")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_WORKSPACE_ID")
            .env_remove("HERDR_TAB_ID")
            .env_remove("HERDR_PANE_ID")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));

        // Inject federation origins if configured.
        let origins_path = env
            .config_home
            .join(app_dir_name())
            .join("federation-origins.env");
        if let Ok(origins) = fs::read_to_string(&origins_path) {
            let origins = origins.trim();
            if !origins.is_empty() {
                cmd.env("HERDR_FEDERATION_ORIGINS", origins);
            }
        }

        let child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("start {} server: {e}", env.name));
        support::register_spawned_herdr_pid(Some(child.id()));
        Self {
            name: env.name,
            child,
        }
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
        panic!("{} server did not exit before timeout", self.name);
    }
}

impl Drop for HerdrServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        support::unregister_spawned_herdr_pid(Some(self.child.id()));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    PathBuf::from("/tmp").join(format!(
        "hfed-local-{}-{:x}",
        std::process::id(),
        nanos & 0xffff_ffff
    ))
}

fn cli_request(binary: &Path, env: &ServerEnv, session: &str, args: &[&str]) -> String {
    let mut cmd = Command::new(binary);
    cmd.arg("--session").arg(session);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.env("HOME", &env.home)
        .env("XDG_CONFIG_HOME", &env.config_home)
        .env("XDG_RUNTIME_DIR", &env.runtime)
        .env("SHELL", "/bin/sh")
        .env("HERDR_DISABLE_SOUND", "1")
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_SESSION")
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("HERDR_TAB_ID")
        .env_remove("HERDR_PANE_ID");
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("cli {:?}: {e}", args));
    assert!(
        output.status.success(),
        "cli {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
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

fn api_request(socket: &Path, value: Value) -> Value {
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

fn api_request_ok(socket: &Path, value: Value) -> Value {
    let response = api_request(socket, value);
    assert!(response.get("error").is_none(), "API error: {response}");
    response
}

fn workspace_list(socket: &Path) -> Vec<Value> {
    api_request_ok(
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
            .find(|ws| ws["label"] == label)
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
            .find(|ws| ws["workspace_id"] == workspace_id)
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
    api_request_ok(
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
        let response = api_request(
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
