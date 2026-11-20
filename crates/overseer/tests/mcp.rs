//! `overseer mcp` driven over stdio against a temp daemon.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use overseer_daemon::{run, DaemonConfig};
use overseer_proto::Client;
use serde_json::{json, Value};

/// Stands in for launchctl: every call fails, so nothing reaches launchd.
const NO_LAUNCHCTL: &str = "/usr/bin/false";

/// A temp home under /tmp (socket paths are length-limited) holding a
/// daemon that runs on the returned runtime until the guard drops.
struct TempDaemon {
    home: std::path::PathBuf,
    rt: tokio::runtime::Runtime,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl TempDaemon {
    fn start(tag: &str) -> TempDaemon {
        TempDaemon::start_with(tag, |_| {})
    }

    fn start_with(tag: &str, edit: impl FnOnce(&mut overseer_daemon::Config)) -> TempDaemon {
        let home = std::path::PathBuf::from(format!("/tmp/ovmcp-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let socket = home.join("overseer.sock");
        // `overseer mcp` names the user from this file.
        std::fs::write(home.join("config.toml"), "user_name = \"Demo\"\n").unwrap();
        let mut settings = overseer_daemon::Config {
            beads_dir: "/nonexistent/overseer-test-beads".into(),
            brain_dir: "/nonexistent/overseer-test-brain".into(),
            jira_command: vec![],
            user_name: "Demo".into(),
            ..overseer_daemon::Config::default()
        };
        // No real beads or Jira unless the test supplies stubs.
        edit(&mut settings);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let task = rt.spawn(run(DaemonConfig {
            socket: socket.clone(),
            default_command: vec!["/bin/cat".to_string()],
            env: overseer_daemon::EnvSource::Inherit,
            state_dir: home.join("state"),
            files: overseer_daemon::SettingsFiles::in_dir(&home),
            hook_program: std::path::PathBuf::from("/bin/true"),
            holder_program: None,
            patterns: overseer_daemon::locks::targets::default_patterns(),
            settings,
        }));
        rt.block_on(async {
            let c = Client::connect_or_start(&socket, || Ok(())).await.unwrap();
            drop(c);
        });
        TempDaemon {
            home,
            rt,
            task: Some(task),
        }
    }
}

impl Drop for TempDaemon {
    fn drop(&mut self) {
        let socket = self.home.join("overseer.sock");
        let task = self.task.take();
        self.rt.block_on(async {
            if let Ok(c) = Client::connect(&socket).await {
                let _ = c.request(overseer_proto::Request::Shutdown).await;
            }
            if let Some(t) = task {
                let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
            }
        });
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// A running `overseer mcp` child.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(home: &std::path::Path, slot: &str) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_overseer"))
            .env("OVERSEER_LAUNCHCTL", NO_LAUNCHCTL)
            .arg("mcp")
            .env("OVERSEER_HOME", home)
            .env("OVERSEER_CONFIG", home.join("config.toml"))
            .env("OVERSEER_SLOT", slot)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let out = BufReader::new(child.stdout.take().unwrap());
        Mcp {
            child,
            stdin,
            out,
            next_id: 1,
        }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
        let mut line = String::new();
        self.out.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).expect("one JSON line of protocol");
        assert_eq!(reply["id"], id);
        reply
    }

    /// Calls a tool and returns its text and `isError`.
    fn tool(&mut self, name: &str, args: Value) -> (String, bool) {
        let reply = self.rpc("tools/call", json!({"name": name, "arguments": args}));
        let r = &reply["result"];
        (
            r["content"][0]["text"].as_str().expect("text").to_owned(),
            r["isError"].as_bool().expect("isError"),
        )
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn claim_then_list_then_release() {
    let d = TempDaemon::start("clr");
    let mut mcp = Mcp::start(&d.home, "S1");
    let init = mcp.rpc("initialize", json!({"protocolVersion": "2025-06-18"}));
    assert_eq!(init["result"]["serverInfo"]["name"], "overseer");

    let (text, err) = mcp.tool("list_targets", json!({}));
    assert_eq!((text.as_str(), err), ("No targets are held.", false));

    let (text, err) = mcp.tool(
        "claim_target",
        json!({"target": "repo:demo-api:main", "reason": "push", "item": "DEMO-33"}),
    );
    assert!(!err, "{text}");

    let (text, err) = mcp.tool("list_targets", json!({}));
    assert!(!err, "{text}");
    assert!(
        text.starts_with("repo:demo-api:main  S1  since ") && text.ends_with("  for DEMO-33"),
        "{text}"
    );

    let (text, err) = mcp.tool("release_target", json!({"target": "repo:demo-api:main"}));
    assert!(!err, "{text}");
    let (text, _) = mcp.tool("list_targets", json!({}));
    assert_eq!(text, "No targets are held.");
}

#[test]
fn held_claim_is_queued_text() {
    let d = TempDaemon::start("held");
    let mut s1 = Mcp::start(&d.home, "S1");
    let mut s2 = Mcp::start(&d.home, "S2");
    let target = json!({"target": "tf:demo-stack", "reason": "apply"});
    let (text, err) = s1.tool("claim_target", target.clone());
    assert!(!err, "{text}");
    let (text, err) = s2.tool("claim_target", target);
    assert!(!err, "a queued claim is not an error: {text}");
    assert!(text.contains("S1"), "{text}");
    let (text, _) = s1.tool("list_targets", json!({}));
    assert!(text.lines().any(|l| l == "  waiting: S2"), "{text}");
}

#[test]
fn worker_notify_without_a_message_is_an_error() {
    let d = TempDaemon::start("notify");
    let mut s2 = Mcp::start(&d.home, "S2");
    let (text, err) = s2.tool("notify_session", json!({"slot": "S3", "text": "hello"}));
    assert!(err);
    assert_eq!(
        text,
        "S2 can message S3 only to answer a message from S3. Workers may message S0 at any time."
    );
}

#[test]
fn no_daemon_is_a_tool_error_not_a_crash() {
    let home = std::path::PathBuf::from(format!("/tmp/ovmcp-none-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut mcp = Mcp::start(&home, "S1");
    let (text, err) = mcp.tool("list_targets", json!({}));
    assert!(err);
    assert!(text.starts_with("Overseer is not reachable: "), "{text}");
    // Still serving afterwards.
    assert_eq!(mcp.rpc("ping", json!({}))["result"], json!({}));
    drop(mcp);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn outside_a_slot_tools_fail_but_initialize_answers() {
    let home = std::path::PathBuf::from(format!("/tmp/ovmcp-noslot-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let mut mcp = Mcp::start(&home, "main");
    assert!(mcp.rpc("initialize", json!({}))["result"].is_object());
    let (text, err) = mcp.tool("list_targets", json!({}));
    assert_eq!(
        (text.as_str(), err),
        ("This session is not in an Overseer slot.", true)
    );
    drop(mcp);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn s0_proposes_and_gets_the_board() {
    let d = TempDaemon::start("plan");
    let mut s0 = Mcp::start(&d.home, "S0");
    let (text, err) = s0.tool(
        "propose_plan",
        json!({"summary": "one lane", "slots": [{"slot": "S3", "label": "demo lane", "cwd": "/tmp"}]}),
    );
    assert!(!err, "{text}");
    assert_eq!(text, "Plan 1 is waiting for Demo.");
    let (text, err) = s0.tool("get_board", json!({}));
    assert!(!err, "{text}");
    assert!(text.starts_with("Summary: none yet"), "{text}");
    assert!(text.contains("Plan 1 waiting for Demo since "), "{text}");
    let (text, err) = s0.tool("set_board_summary", json!({"text": "One lane waits."}));
    assert!(!err, "{text}");
    let (text, _) = s0.tool("get_board", json!({}));
    assert!(text.contains("): One lane waits."), "{text}");
    let (text, err) = s0.tool("get_work_items", json!({}));
    assert!(!err, "{text}");
}

#[test]
fn get_item_detail_is_listed_and_sends_the_request() {
    let d = TempDaemon::start("detail");
    let mut s0 = Mcp::start(&d.home, "S0");
    let reply = s0.rpc("tools/list", json!({}));
    let tool = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "get_item_detail")
        .expect("listed")
        .clone();
    assert_eq!(tool["inputSchema"]["properties"]["key"]["type"], "string");
    assert_eq!(
        tool["inputSchema"]["properties"]["refresh"]["type"],
        "boolean"
    );
    assert_eq!(tool["inputSchema"]["required"], json!(["key"]));
    // The daemon has no such item: its answer comes back as a tool error.
    let (text, err) = s0.tool("get_item_detail", json!({"key": "DEMO-404"}));
    assert!(err, "{text}");
    assert!(!text.starts_with("Unexpected reply"), "{text}");
}

#[test]
fn worker_cannot_propose() {
    let d = TempDaemon::start("worker");
    let mut s3 = Mcp::start(&d.home, "S3");
    let (text, err) = s3.tool(
        "propose_plan",
        json!({"summary": "mine", "slots": [{"slot": 3, "cwd": "/tmp"}]}),
    );
    assert!(err);
    assert_eq!(text, "only the coordinator (S0) can propose a plan");
    // A worker may still read the board.
    let (text, err) = s3.tool("get_board", json!({}));
    assert!(!err, "{text}");
}

#[test]
fn get_item_detail_returns_a_bead() {
    use std::os::unix::fs::PermissionsExt;
    let d = TempDaemon::start_with("bead", |c| {
        let home = std::path::PathBuf::from(format!("/tmp/ovmcp-bead-{}", std::process::id()));
        std::fs::create_dir_all(home.join("beads/.beads")).unwrap();
        let show = home.join("bd-show.json");
        std::fs::write(
            &show,
            include_str!("../../overseer-daemon/tests/fixtures/bd-show.json"),
        )
        .unwrap();
        let bd = home.join("bd");
        std::fs::write(
            &bd,
            format!(
                "#!/bin/sh\ncase \"$1\" in show) cat {};; *) echo '[{{\"id\":\"demo-1\",\"title\":\"Wire the detail view\",\"status\":\"open\",\"priority\":1}}]';; esac\n",
                show.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bd, std::fs::Permissions::from_mode(0o755)).unwrap();
        c.beads_dir = home.join("beads");
        c.bd_command = vec![bd.display().to_string()];
    });
    let mut s0 = Mcp::start(&d.home, "S0");
    let mut last = (String::new(), true);
    for _ in 0..100 {
        last = s0.tool("get_item_detail", json!({"key": "demo-1"}));
        if !last.0.contains("demo-1 is not a known item") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let (text, err) = last;
    assert!(!err, "{text}");
    assert!(text.contains("demo-1 Wire the detail view"), "{text}");
    assert!(text.contains("Show the item body."), "{text}");
    assert!(text.contains("Depends on: demo-2 (blocks)"), "{text}");
}

#[test]
fn a_session_drops_only_the_relays_it_sent() {
    use overseer_proto::{Request, Response};
    let d = TempDaemon::start("droprelay");
    let socket = d.home.join("overseer.sock");
    let ask = |req: Request| {
        d.rt.block_on(async {
            let c = Client::connect(&socket).await.unwrap();
            c.request(req).await.unwrap()
        })
    };
    // S2 runs `cat`, which never reports activity, so relays stay pending.
    let start = Request::StartSlot {
        id: 2,
        label: "demo lane".into(),
        cwd: "/tmp".into(),
        command: None,
        prompt: None,
    };
    assert_eq!(ask(start), Response::Ok);
    let mut s0 = Mcp::start(&d.home, "S0");
    let (text, err) = s0.tool("notify_session", json!({"slot": "S2", "text": "status?"}));
    assert!(!err, "{text}");
    let yours = ask(Request::SendRelay {
        from: None,
        to: 2,
        text: "hello from Demo".into(),
    });
    assert!(matches!(yours, Response::Text { .. }), "{yours:?}");
    let Response::Locks(v) = ask(Request::ListLocks) else {
        panic!("no lock view")
    };
    let id_of = |t: &str| v.relays.iter().find(|r| r.text == t).unwrap().id;
    let (mine, demos) = (id_of("status?"), id_of("hello from Demo"));

    let (text, err) = s0.tool("list_targets", json!({}));
    assert!(!err, "{text}");
    assert!(
        text.contains(&format!("relay #{mine} to S2, pending 0m: status?")),
        "{text}"
    );
    assert!(!text.contains("hello from Demo"), "{text}");

    let (text, err) = s0.tool("drop_relay", json!({"id": demos}));
    assert!(err, "dropped another sender's relay: {text}");
    assert_eq!(
        text,
        format!("relay {demos} was not sent by S0; a session can only drop relays it sent")
    );
    let (text, err) = s0.tool("drop_relay", json!({"to": "S2"}));
    assert_eq!(
        (text.as_str(), err),
        ("Dropped 1 relay you sent to S2.", false)
    );
    let Response::Locks(v) = ask(Request::ListLocks) else {
        panic!("no lock view")
    };
    let state = |id: i64| {
        let r = v.relays.iter().find(|r| r.id == id).unwrap();
        (r.state, r.drop_reason.clone())
    };
    assert_eq!(
        state(mine),
        (
            overseer_proto::RelayState::Dropped,
            Some("dropped by S0".into())
        )
    );
    assert_eq!(state(demos), (overseer_proto::RelayState::Pending, None));
}
