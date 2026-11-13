//! `overseer mcp`: a stdio MCP server (newline-delimited JSON-RPC 2.0) that
//! gives a slot's Claude session the lock and relay tools.
//!
//! Only stdout carries protocol; diagnostics go to stderr. The daemon is
//! connected afresh per tool call (it may restart while the session lives)
//! and is never started from here.

use std::io::{BufRead, Write};
use std::time::Duration;

use overseer_proto::{paths, slot_name, Client, Request, Response, SlotId};
use serde_json::{json, Value};

/// Budget for connect plus the round trip of one tool call.
const CALL_WITHIN: Duration = Duration::from_secs(5);
const LATEST_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const PARSE_ERROR: i64 = -32700;
const NOT_IN_A_SLOT: &str = "This session is not in an Overseer slot.";

type Call<'a> = dyn FnMut(Request) -> Result<Response, String> + 'a;

fn tools() -> Value {
    json!([
        {
            "name": "claim_target",
            "description": "Claim a shared target (for example repo:demo-api:main, uat/root, or an ArgoCD app) before changing it. If another session holds it you are queued and told when it is yours.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "The target key, for example repo:demo-api:main."},
                    "reason": {"type": "string", "description": "What you are going to do with it."},
                    "item": {"type": "string", "description": "The work item the claim is for, for example DEMO-33 or ov-3."}
                },
                "required": ["target", "reason"]
            }
        },
        {
            "name": "release_target",
            "description": "Release a target you hold, or leave its queue. The next waiter is granted it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "The target key to release."}
                },
                "required": ["target"]
            }
        },
        {
            "name": "notify_session",
            "description": "Send a message to another Overseer session. Workers may only answer a message they were sent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": {"type": ["string", "integer"], "description": "The slot to message, as S3 or 3."},
                    "text": {"type": "string", "description": "The message."}
                },
                "required": ["slot", "text"]
            }
        },
        {
            "name": "list_targets",
            "description": "List the targets that are held, who holds them and who is waiting. Read-only.",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn tool_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": is_error})
}

/// A string argument that must be present.
fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    match args.get(name) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(format!("`{name}` must be a string")),
        None => Err(format!("missing `{name}`")),
    }
}

/// An optional string argument: absent or null is `None`.
fn opt_str_arg(args: &Value, name: &str) -> Result<Option<String>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{name}` must be a string")),
    }
}

/// `"S3"` or `3`. `None` when it is neither shape. A well-formed number too
/// large for a slot id maps to `Some(SlotId::MAX)`, which the daemon rejects
/// as an unknown slot.
fn slot_arg(v: &Value) -> Option<SlotId> {
    let n = match v {
        Value::String(s) => s.strip_prefix('S')?.parse::<u64>().ok()?,
        Value::Number(n) => n.as_u64()?,
        _ => return None,
    };
    Some(SlotId::try_from(n).unwrap_or(SlotId::MAX))
}

/// The tool call parsed into the request to send, before any slot check.
fn tool_request(name: &str, args: &Value, slot: Option<SlotId>) -> Result<ToolPlan, String> {
    let own = slot.unwrap_or(0);
    Ok(match name {
        "claim_target" => ToolPlan::Send(Request::ClaimTarget {
            slot: own,
            target: str_arg(args, "target")?.to_owned(),
            // The schema requires it; read it as a string either way.
            reason: Some(str_arg(args, "reason")?.to_owned()),
            item: opt_str_arg(args, "item")?,
        }),
        "release_target" => ToolPlan::Send(Request::ReleaseTarget {
            slot: own,
            target: str_arg(args, "target")?.to_owned(),
        }),
        "notify_session" => {
            let to = args
                .get("slot")
                .ok_or_else(|| "missing `slot`".to_string())?;
            let to =
                slot_arg(to).ok_or_else(|| "`slot` must be a slot like \"S3\" or 3".to_string())?;
            ToolPlan::Send(Request::SendRelay {
                from: Some(own),
                to,
                text: str_arg(args, "text")?.to_owned(),
            })
        }
        "list_targets" => ToolPlan::Send(Request::ListLocks),
        other => return Err(format!("unknown tool `{other}`")),
    })
}

enum ToolPlan {
    Send(Request),
}

/// `repo:demo-api:main  S1  since 08:40  for DEMO-33`, then one
/// `  waiting: S2` line per queued slot, or `No targets are held.`
fn format_locks(view: &overseer_proto::LockView) -> String {
    if view.claims.is_empty() && view.waits.is_empty() {
        return "No targets are held.".into();
    }
    let mut lines = Vec::new();
    for c in &view.claims {
        let mut line = format!(
            "{}  {}  since {}",
            c.target,
            slot_name(c.slot),
            crate::ctl::local_hhmm(c.since)
        );
        if let Some(why) = c.item.as_deref().or(c.reason.as_deref()) {
            line.push_str(&format!("  for {why}"));
        }
        lines.push(line);
        for w in view.waits.iter().filter(|w| w.target == c.target) {
            lines.push(format!("  waiting: {}", slot_name(w.slot)));
        }
    }
    // Waits on a target nobody holds (a grant not yet taken up).
    for w in &view.waits {
        if !view.claims.iter().any(|c| c.target == w.target) {
            lines.push(format!("{}  waiting: {}", w.target, slot_name(w.slot)));
        }
    }
    lines.join("\n")
}

fn call_tool(params: &Value, slot: Option<SlotId>, call: &mut Call) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing tool `name`".to_string())?;
    let empty = json!({});
    let args = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(a @ Value::Object(_)) => a,
        Some(_) => return Err("`arguments` must be an object".into()),
    };
    let ToolPlan::Send(req) = tool_request(name, args, slot)?;
    if slot.is_none() {
        return Ok(tool_result(NOT_IN_A_SLOT, true));
    }
    Ok(match call(req) {
        Err(reason) => tool_result(format!("Overseer is not reachable: {reason}"), true),
        Ok(Response::Error { message }) => tool_result(message, true),
        Ok(Response::Text { text }) => tool_result(text, false),
        Ok(Response::Queued { message }) => tool_result(message, false),
        Ok(Response::Locks(view)) => tool_result(format_locks(&view), false),
        Ok(Response::Ok) => tool_result("Done.", false),
        Ok(other) => tool_result(format!("Unexpected reply from Overseer: {other:?}"), true),
    })
}

/// Answers one JSON-RPC message; `None` for notifications. `slot` is this
/// session's slot, `None` when it is not in one.
pub fn handle(msg: &Value, slot: Option<SlotId>, call: &mut Call) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str);
    // A message with no id is a notification, whatever it says.
    let id = msg.get("id")?;
    let Some(method) = method else {
        // A response or junk: nothing to answer a server's own id with.
        return Some(rpc_error(id, -32600, "invalid request"));
    };
    let params = msg.get("params").unwrap_or(&Value::Null);
    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = asked
                .filter(|v| SUPPORTED_VERSIONS.contains(v))
                .unwrap_or(LATEST_VERSION);
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "overseer", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({"tools": tools()})),
        "tools/call" => match call_tool(params, slot, call) {
            Ok(result) => rpc_result(id, result),
            Err(message) => rpc_error(id, INVALID_PARAMS, &message),
        },
        other => rpc_error(id, METHOD_NOT_FOUND, &format!("method not found: {other}")),
    })
}

/// One request to the daemon over a fresh connection, within `CALL_WITHIN`.
fn ask_daemon(rt: &tokio::runtime::Runtime, req: Request) -> Result<Response, String> {
    rt.block_on(async {
        tokio::time::timeout(CALL_WITHIN, async {
            let client = Client::connect(paths::socket_path())
                .await
                .map_err(|e| e.to_string())?;
            client.request(req).await.map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|_| Err("timed out".to_string()))
    })
}

/// Serves stdin until it closes.
pub fn run() -> anyhow::Result<()> {
    let slot = std::env::var("OVERSEER_SLOT")
        .ok()
        .and_then(|s| crate::hook::slot(&s));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut call = |req: Request| ask_daemon(&rt, req);
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, slot, &mut call),
            Err(e) => Some(rpc_error(
                &Value::Null,
                PARSE_ERROR,
                &format!("parse error: {e}"),
            )),
        };
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use overseer_proto::{Claim, ClaimKind, LockView, Wait};

    fn rpc(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    }

    fn call_with(
        msg: &Value,
        slot: Option<SlotId>,
        reply: Result<Response, String>,
    ) -> (Option<Value>, Vec<Request>) {
        let mut seen = Vec::new();
        let out = handle(msg, slot, &mut |r| {
            seen.push(r);
            reply.clone()
        });
        (out, seen)
    }

    fn tool(name: &str, args: Value) -> Value {
        rpc("tools/call", json!({"name": name, "arguments": args}))
    }

    fn text_of(v: &Value) -> (&str, bool) {
        let r = &v["result"];
        (
            r["content"][0]["text"].as_str().unwrap(),
            r["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn initialize_echoes_a_supported_version_else_falls_back() {
        for v in ["2025-06-18", "2025-03-26", "2024-11-05"] {
            let (out, _) = call_with(
                &rpc("initialize", json!({"protocolVersion": v})),
                Some(1),
                Ok(Response::Ok),
            );
            let out = out.unwrap();
            assert_eq!(out["result"]["protocolVersion"], v);
            assert_eq!(out["result"]["capabilities"], json!({"tools": {}}));
            assert_eq!(out["result"]["serverInfo"]["name"], "overseer");
            assert_eq!(
                out["result"]["serverInfo"]["version"],
                env!("CARGO_PKG_VERSION")
            );
        }
        for params in [json!({"protocolVersion": "1999-01-01"}), json!({})] {
            let (out, _) = call_with(&rpc("initialize", params), None, Ok(Response::Ok));
            assert_eq!(out.unwrap()["result"]["protocolVersion"], "2025-06-18");
        }
    }

    #[test]
    fn notifications_get_no_answer() {
        let n = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(call_with(&n, Some(1), Ok(Response::Ok)).0.is_none());
    }

    #[test]
    fn ping_and_unknown_method() {
        let (out, _) = call_with(&rpc("ping", json!({})), Some(1), Ok(Response::Ok));
        assert_eq!(out.unwrap()["result"], json!({}));
        let (out, _) = call_with(&rpc("resources/list", json!({})), Some(1), Ok(Response::Ok));
        assert_eq!(out.unwrap()["error"]["code"], -32601);
    }

    #[test]
    fn tools_list_names_the_four_tools() {
        let (out, _) = call_with(&rpc("tools/list", json!({})), Some(1), Ok(Response::Ok));
        let out = out.unwrap();
        let names: Vec<&str> = out["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "claim_target",
                "release_target",
                "notify_session",
                "list_targets"
            ]
        );
    }

    #[test]
    fn bad_params_are_32602() {
        for msg in [
            tool("claim_target", json!({"reason": "push"})),
            tool("claim_target", json!({"target": 5, "reason": "push"})),
            tool("claim_target", json!({"target": "a"})),
            tool("release_target", json!({})),
            tool("notify_session", json!({"slot": "main", "text": "hi"})),
            tool("notify_session", json!({"slot": 1.5, "text": "hi"})),
            tool("notify_session", json!({"slot": "S1"})),
            tool("no_such_tool", json!({})),
            rpc("tools/call", json!({})),
            rpc(
                "tools/call",
                json!({"name": "list_targets", "arguments": 3}),
            ),
        ] {
            let (out, seen) = call_with(&msg, Some(1), Ok(Response::Ok));
            assert_eq!(out.unwrap()["error"]["code"], -32602, "{msg}");
            assert!(seen.is_empty());
        }
    }

    #[test]
    fn claim_sends_the_request_and_queued_is_not_an_error() {
        let msg = tool(
            "claim_target",
            json!({"target": "repo:demo-api:main", "reason": "push", "item": "DEMO-33"}),
        );
        let (out, seen) = call_with(
            &msg,
            Some(2),
            Ok(Response::Queued {
                message: "Queued behind S1.".into(),
            }),
        );
        assert_eq!(
            seen,
            [Request::ClaimTarget {
                slot: 2,
                target: "repo:demo-api:main".into(),
                reason: Some("push".into()),
                item: Some("DEMO-33".into()),
            }]
        );
        assert_eq!(text_of(&out.unwrap()), ("Queued behind S1.", false));
    }

    #[test]
    fn release_sends_the_request() {
        let msg = tool("release_target", json!({"target": "uat/root"}));
        let (out, seen) = call_with(
            &msg,
            Some(4),
            Ok(Response::Text {
                text: "Released uat/root.".into(),
            }),
        );
        assert_eq!(
            seen,
            [Request::ReleaseTarget {
                slot: 4,
                target: "uat/root".into()
            }]
        );
        assert_eq!(text_of(&out.unwrap()), ("Released uat/root.", false));
    }

    #[test]
    fn notify_accepts_both_slot_shapes_and_reports_daemon_errors() {
        for slot in [json!("S3"), json!(3)] {
            let (out, seen) = call_with(
                &tool("notify_session", json!({"slot": slot, "text": "hi"})),
                Some(0),
                Ok(Response::Text {
                    text: "Sent.".into(),
                }),
            );
            assert_eq!(
                seen,
                [Request::SendRelay {
                    from: Some(0),
                    to: 3,
                    text: "hi".into()
                }]
            );
            assert_eq!(text_of(&out.unwrap()), ("Sent.", false));
        }
        // A well-formed slot the daemon rejects is its error, not bad params.
        let (out, seen) = call_with(
            &tool("notify_session", json!({"slot": "S9", "text": "hi"})),
            Some(2),
            Ok(Response::Error {
                message: "No slot S9.".into(),
            }),
        );
        assert_eq!(seen.len(), 1);
        assert_eq!(text_of(&out.unwrap()), ("No slot S9.", true));
    }

    #[test]
    fn list_targets_formats_claims_and_waits() {
        let view = LockView {
            claims: vec![Claim {
                target: "repo:demo-api:main".into(),
                slot: 1,
                kind: ClaimKind::Claim,
                reason: Some("push".into()),
                item: Some("DEMO-33".into()),
                since: 1_000_000,
                last_used: 1_000_000,
                pin_revision: None,
            }],
            waits: vec![Wait {
                target: "repo:demo-api:main".into(),
                slot: 2,
                at: 1_000_100,
                reason: None,
            }],
            relays: vec![],
            flag_after_secs: 3600,
        };
        let (out, seen) = call_with(
            &tool("list_targets", json!({})),
            Some(1),
            Ok(Response::Locks(view)),
        );
        assert_eq!(seen, [Request::ListLocks]);
        let out = out.unwrap();
        let (text, err) = text_of(&out);
        assert!(!err);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let (head, hhmm) = lines[0].split_once("since ").unwrap_or(("", ""));
        assert_eq!(head, "repo:demo-api:main  S1  ");
        assert!(hhmm.ends_with("  for DEMO-33"), "{}", lines[0]);
        assert_eq!(lines[1], "  waiting: S2");

        let empty = LockView {
            claims: vec![],
            waits: vec![],
            relays: vec![],
            flag_after_secs: 3600,
        };
        let (out, _) = call_with(
            &tool("list_targets", json!({})),
            Some(1),
            Ok(Response::Locks(empty)),
        );
        assert_eq!(text_of(&out.unwrap()), ("No targets are held.", false));
    }

    #[test]
    fn daemon_error_and_unreachable_are_tool_errors() {
        let msg = tool("release_target", json!({"target": "uat/root"}));
        let (out, _) = call_with(
            &msg,
            Some(1),
            Ok(Response::Error {
                message: "S1 does not hold uat/root.".into(),
            }),
        );
        assert_eq!(text_of(&out.unwrap()), ("S1 does not hold uat/root.", true));
        let (out, _) = call_with(&msg, Some(1), Err("connection refused".into()));
        assert_eq!(
            text_of(&out.unwrap()),
            ("Overseer is not reachable: connection refused", true)
        );
    }

    #[test]
    fn no_slot_makes_every_tool_an_error_but_initialize_works() {
        let (out, _) = call_with(&rpc("initialize", json!({})), None, Ok(Response::Ok));
        assert!(out.unwrap().get("result").is_some());
        let (out, seen) = call_with(&tool("list_targets", json!({})), None, Ok(Response::Ok));
        assert_eq!(text_of(&out.unwrap()), (NOT_IN_A_SLOT, true));
        assert!(seen.is_empty());
    }
}
