//! `overseer mcp`: a stdio MCP server (newline-delimited JSON-RPC 2.0) that
//! gives a slot's Claude session the lock and relay tools.
//!
//! Only stdout carries protocol; diagnostics go to stderr. The daemon is
//! connected afresh per tool call (it may restart while the session lives)
//! and is never started from here.

use std::io::{BufRead, Write};
use std::time::Duration;

use overseer_proto::{
    paths, slot_name, Client, ItemDetail, Plan, PlanSlot, Request, Response, SlotId,
};
use serde_json::{json, Value};

/// Budget for connect plus the round trip of one tool call.
const CALL_WITHIN: Duration = Duration::from_secs(5);

/// Budget for an item detail that has to be fetched from Jira or bd. It
/// covers the daemon's worst case on the REST path: the wait for the
/// login-shell PATH (5 s), the Keychain lookup (30 s) and the REST call
/// (up to 60 s, a cloud id lookup plus the issue, 30 s each), 95 s in all,
/// so a call the daemon would answer is not cut off.
const DETAIL_CALL_WITHIN: Duration = Duration::from_secs(100);

/// A 2ndBrain write waits its turn in the queue, and a commit blocked by an
/// `index.lock` is answered after up to 600 s (as `Queued`).
const BRAIN_CALL_WITHIN: Duration = Duration::from_secs(660);

fn call_within(req: &Request) -> Duration {
    match req {
        Request::Commit { .. }
        | Request::AppendJournal { .. }
        | Request::EditShared { .. }
        | Request::UpdateCurrentSession { .. }
        | Request::SyncNotes { .. }
        | Request::RestoreNotesFile { .. }
        // Approving a plan creates worktrees and starts sessions.
        | Request::ProposePlan { .. }
        | Request::DecidePlan { .. } => BRAIN_CALL_WITHIN,
        // A cache miss runs acli or a REST call.
        Request::GetItemDetail { .. } => DETAIL_CALL_WITHIN,
        _ => CALL_WITHIN,
    }
}
const LATEST_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const PARSE_ERROR: i64 = -32700;
const NEEDS_A_SLOT: &str =
    "update_current_session needs a slot; use `overseer ctl current-session --slot S<n>`";
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
                    "reason": {"type": "string", "description": "What you are going to do with it. Optional, but it tells the user and other sessions why you hold it."},
                    "item": {"type": "string", "description": "The work item the claim is for, for example DEMO-33 or ov-3."}
                },
                "required": ["target"]
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
            "description": "Send a message to another Overseer session. Workers may message S0 at any time, with one message waiting to be typed in at a time, and cannot message other workers.",
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
            "name": "drop_relay",
            "description": "Withdraw a message you sent with notify_session that has not been typed in yet. Give `id` for one message (list_targets shows the ids of your pending ones) or `to` for every pending message you sent to that slot. You can only drop your own.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer", "description": "The relay id, for example 7."},
                    "to": {"type": ["string", "integer"], "description": "The slot, as S3 or 3."}
                }
            }
        },
        {
            "name": "list_targets",
            "description": "List the targets that are held, who holds them and who is waiting. Read-only.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "commit_2ndbrain",
            "description": "Commit and push files in the notes folder. Overseer commits only the paths you name, one commit at a time, so you never sweep up another session's half-finished edits. Use this instead of git. Replies with the commit, or says the commit is queued and will land.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Files to commit, relative to the notes folder or absolute. Deletions count."},
                    "message": {"type": "string", "description": "The commit message."}
                },
                "required": ["paths", "message"]
            }
        },
        {
            "name": "append_journal",
            "description": "Add text to a section of today's journal and commit it. The text goes at the end of the `## <section>` block; a missing section is created at the end of the file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "section": {"type": "string", "description": "The section heading without the leading ##, for example Work Log."},
                    "text": {"type": "string", "description": "What to add."}
                },
                "required": ["section", "text"]
            }
        },
        {
            "name": "edit_shared_file",
            "description": "Replace one exact piece of text in a shared notes file (a sprint dashboard, a journal) and commit it. `old_text` must occur exactly once; if the file has changed since you read it the edit fails and shows you the current text. To create a shared file that does not exist yet, give an empty `old_text` and the whole file as `new_text`; that fails if the file is already there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The shared file, relative to the notes folder or absolute."},
                    "old_text": {"type": "string", "description": "The exact text to replace. It must occur once. Empty to create a new file."},
                    "new_text": {"type": "string", "description": "What to put there."}
                },
                "required": ["path", "old_text", "new_text"]
            }
        },
        {
            "name": "update_current_session",
            "description": "Set your section of CURRENT_SESSION.md: what you are working on and where you got to. It replaces your previous text and leaves every other session's section alone.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Your section's text."}
                },
                "required": ["text"]
            }
        },
        {
            "name": "sync_2ndbrain",
            "description": "Bring the notes folder level with its remote: fetch, merge the remote's commits and push. Conflicts in the journal, session notes and CURRENT_SESSION.md keep this folder's copy; any other conflict stops the merge and changes nothing. A merge that makes a file much shorter is kept here but not pushed: check the file (restore_2ndbrain_file brings back the old copy), then call again with confirm.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "confirm": {"type": "boolean", "description": "Push a merge that was held because it made a file much shorter. Only after you have checked that file."}
                }
            }
        },
        {
            "name": "restore_2ndbrain_file",
            "description": "Put a notes file back as it was at an earlier commit and commit that. Use it when a merge cut a file short: the held-push message names the commit from before the merge.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The file, relative to the notes folder or absolute."},
                    "rev": {"type": "string", "description": "The commit to take it from, for example the hash a held push names."}
                },
                "required": ["path", "rev"]
            }
        },
        {
            "name": "get_board",
            "description": "The whole board in one answer: the coordinator's summary, every slot with its state, owned items, held targets and last report, the work items, the locks and any plan waiting for the user. Read-only.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "get_work_items",
            "description": "List the work items (tickets and beads) with their owners and source health. Read-only.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "get_item_detail",
            "description": "Read one ticket's or bead's details: description, notes, PRs, child beads. Cached for 5 minutes; refresh: true fetches again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "The item key, for example DEMO-12 or ov-3."},
                    "refresh": {"type": "boolean", "description": "Fetch again instead of using the cached copy."}
                },
                "required": ["key"]
            }
        },
        {
            "name": "propose_plan",
            "description": "Coordinator only. Propose a plan for the user to approve: which slot takes which items, which targets it claims, where it works and what it is told. Nothing runs until the user approves. A new plan replaces a pending one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "What the plan is for, in a line or two."},
                    "slots": {
                        "type": "array",
                        "description": "What each slot does, S1 to S6.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "slot": {"type": ["string", "integer"], "description": "The slot, as S3 or 3."},
                                "label": {"type": "string", "description": "A short name for the slot's work."},
                                "items": {"type": "array", "items": {"type": "string"}, "description": "Work item keys to assign, for example DEMO-12 or ov-3. An item another slot owns is refused here; put it in moves."},
                                "moves": {"type": "array", "items": {"type": "string"}, "description": "Work item keys another slot owns that this slot takes over. Approving the plan moves them, and the user sees each move before approving."},
                                "targets": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "target": {"type": "string", "description": "For example repo:demo-api:main."},
                                            "reason": {"type": "string"}
                                        },
                                        "required": ["target"]
                                    },
                                    "description": "Targets the slot claims."
                                },
                                "worktree": {
                                    "type": "object",
                                    "properties": {
                                        "repo": {"type": "string", "description": "The repository folder."},
                                        "ticket": {"type": "string", "description": "The ticket, which names the branch and the worktree."}
                                    },
                                    "required": ["repo", "ticket"],
                                    "description": "A git worktree to make for the slot."
                                },
                                "cwd": {"type": "string", "description": "The folder to start the session in when there is no worktree."},
                                "kickoff": {"type": "string", "description": "Extra text for the slot's first message."},
                                "rules": {"type": "array", "items": {"type": "string"}, "description": "Collision rules, for example Do not deploy to UAT before S2 merges."}
                            },
                            "required": ["slot"]
                        }
                    }
                },
                "required": ["summary", "slots"]
            }
        },
        {
            "name": "request_status",
            "description": "Coordinator only. Ask a running slot for a short status. It answers with report_progress.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slot": {"type": ["string", "integer"], "description": "The slot to ask, as S3 or 3."}
                },
                "required": ["slot"]
            }
        },
        {
            "name": "report_progress",
            "description": "Tell the coordinator where you are, in two or three lines. Use it when asked for a status and when you finish something.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Your progress."}
                },
                "required": ["text"]
            }
        },
        {
            "name": "set_board_summary",
            "description": "Coordinator only. Set the board summary the user sees at the top of the board. It replaces the previous one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The summary."}
                },
                "required": ["text"]
            }
        }
    ])
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// One item's detail as the text a session reads.
fn detail_text(d: &ItemDetail) -> String {
    let mut out = format!(
        "{} {}\nStatus: {} \u{b7} Priority: {}",
        d.key,
        d.title,
        d.status,
        d.priority.as_deref().unwrap_or("not set")
    );
    if let Some(stage) = d.stage {
        out.push_str(&format!(" \u{b7} Stage: {}", stage.label()));
    }
    out.push_str("\n\n");
    let body = d.body.as_deref().filter(|b| !b.trim().is_empty());
    let overview = d
        .overview
        .as_deref()
        .map(|o| format!("From the folder overview: {o}"));
    match (&d.error, body) {
        (Some(error), Some(body)) => out.push_str(&format!(
            "Could not fetch the description: {error}\n\nLast fetched copy:\n{body}"
        )),
        (Some(error), None) => {
            out.push_str(&format!("Could not fetch the description: {error}"));
            if let Some(overview) = overview {
                out.push_str(&format!("\n\n{overview}"));
            }
        }
        (None, Some(body)) => out.push_str(body),
        (None, None) => out.push_str(overview.as_deref().unwrap_or("No description")),
    }
    if let Some(notes) = &d.notes {
        out.push_str(&format!("\n\nNotes:\n{notes}"));
    }
    if !d.depends_on.is_empty() {
        out.push_str(&format!("\n\nDepends on: {}", d.depends_on.join(", ")));
    }
    if !d.prs.is_empty() {
        out.push_str("\n\nPull requests:");
        for pr in &d.prs {
            out.push_str(&format!("\n- {} #{} {}", pr.repo, pr.id, pr.title));
        }
    }
    if !d.children.is_empty() {
        out.push_str("\n\nChild beads:");
        for c in &d.children {
            out.push_str(&format!("\n- {} {} {}", c.key, c.status, c.title));
        }
    }
    if let Some(folder) = &d.folder {
        out.push_str(&format!("\n\nFolder: {}", folder.display()));
    }
    out
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

/// A list of strings that must be present.
fn str_list_arg(args: &Value, name: &str) -> Result<Vec<String>, String> {
    match args.get(name) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("`{name}` must be an array of strings")),
        Some(_) => Err(format!("`{name}` must be an array of strings")),
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

/// An optional true or false: absent or null is `None`.
fn opt_bool_arg(args: &Value, name: &str) -> Result<Option<bool>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("`{name}` must be true or false")),
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

/// A plan from its JSON: `{"summary": ..., "slots": [...]}`, where a slot's
/// `slot` is `"S3"` or `3`. The rest is the wire shape of `Plan`.
pub(crate) fn plan_from_json(v: &Value) -> Result<Plan, String> {
    let Some(Value::Array(raw)) = v.get("slots") else {
        return Err("`slots` must be an array".into());
    };
    let mut slots = Vec::new();
    for (i, entry) in raw.iter().enumerate() {
        let mut entry = entry.clone();
        let id = entry
            .get("slot")
            .ok_or_else(|| format!("slot {} of the plan has no `slot`", i + 1))?;
        let id = slot_arg(id)
            .ok_or_else(|| format!("slot {}: `slot` must be a slot like \"S3\" or 3", i + 1))?;
        entry["slot"] = json!(id);
        slots.push(
            serde_json::from_value::<PlanSlot>(entry)
                .map_err(|e| format!("slot {} of the plan is not valid: {e}", i + 1))?,
        );
    }
    Ok(Plan {
        summary: opt_str_arg(v, "summary")?.unwrap_or_default(),
        slots,
    })
}

/// The tool call parsed into the request to send, before any slot check.
fn tool_request(name: &str, args: &Value, slot: Option<SlotId>) -> Result<ToolPlan, String> {
    let own = slot.unwrap_or(0);
    Ok(match name {
        "claim_target" => ToolPlan::Send(Request::ClaimTarget {
            slot: own,
            target: str_arg(args, "target")?.to_owned(),
            // Optional, as for `ctl claim` and the GUI.
            reason: opt_str_arg(args, "reason")?,
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
        "drop_relay" => {
            let id = match args.get("id") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_i64()
                        .ok_or_else(|| "`id` must be a relay id like 7".to_string())?,
                ),
            };
            let to = match args.get("to") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    slot_arg(v)
                        .ok_or_else(|| "`to` must be a slot like \"S3\" or 3".to_string())?,
                ),
            };
            if id.is_some() == to.is_some() {
                return Err("give exactly one of `id` and `to`".into());
            }
            ToolPlan::Send(Request::DropRelays {
                from: Some(own),
                id,
                to,
            })
        }
        "list_targets" => ToolPlan::Send(Request::ListLocks),
        "commit_2ndbrain" => ToolPlan::Send(Request::Commit {
            from: slot,
            paths: str_list_arg(args, "paths")?,
            message: str_arg(args, "message")?.to_owned(),
        }),
        "append_journal" => ToolPlan::Send(Request::AppendJournal {
            from: slot,
            section: str_arg(args, "section")?.to_owned(),
            text: str_arg(args, "text")?.to_owned(),
        }),
        "edit_shared_file" => ToolPlan::Send(Request::EditShared {
            from: slot,
            path: str_arg(args, "path")?.to_owned(),
            old_text: str_arg(args, "old_text")?.to_owned(),
            new_text: str_arg(args, "new_text")?.to_owned(),
        }),
        // The daemon names the section after the slot and its label.
        "update_current_session" => ToolPlan::Send(Request::UpdateCurrentSession {
            slot: own,
            text: str_arg(args, "text")?.to_owned(),
        }),
        "sync_2ndbrain" => ToolPlan::Send(Request::SyncNotes {
            from: slot,
            confirm: opt_bool_arg(args, "confirm")?.unwrap_or(false),
        }),
        "restore_2ndbrain_file" => ToolPlan::Send(Request::RestoreNotesFile {
            from: slot,
            path: str_arg(args, "path")?.to_owned(),
            rev: str_arg(args, "rev")?.to_owned(),
        }),
        "get_board" => ToolPlan::Send(Request::GetBoard),
        "get_work_items" => ToolPlan::Send(Request::ListWorkItems),
        "get_item_detail" => ToolPlan::Send(Request::GetItemDetail {
            key: str_arg(args, "key")?.to_owned(),
            refresh: opt_bool_arg(args, "refresh")?.unwrap_or(false),
        }),
        "propose_plan" => {
            str_arg(args, "summary")?;
            ToolPlan::Send(Request::ProposePlan {
                from: slot,
                plan: plan_from_json(args)?,
            })
        }
        "request_status" => {
            let to = args
                .get("slot")
                .ok_or_else(|| "missing `slot`".to_string())?;
            let to =
                slot_arg(to).ok_or_else(|| "`slot` must be a slot like \"S3\" or 3".to_string())?;
            ToolPlan::Send(Request::RequestStatus {
                from: slot,
                slot: to,
            })
        }
        "report_progress" => ToolPlan::Send(Request::ReportProgress {
            slot: own,
            text: str_arg(args, "text")?.to_owned(),
        }),
        "set_board_summary" => ToolPlan::Send(Request::SetBoardSummary {
            from: slot,
            text: str_arg(args, "text")?.to_owned(),
        }),
        other => return Err(format!("unknown tool `{other}`")),
    })
}

enum ToolPlan {
    Send(Request),
}

/// `format_locks`, then the relays `slot` sent that are still pending
/// (`relay #7 to S2, pending 5m: <text>`), so it can drop one by id.
fn format_locks_for(view: &overseer_proto::LockView, slot: SlotId, now: i64) -> String {
    let mut out = format_locks(view);
    let mine: Vec<String> = view
        .relays
        .iter()
        .filter(|r| {
            r.from == (overseer_proto::Sender::Slot { slot })
                && r.state == overseer_proto::RelayState::Pending
        })
        .map(|r| {
            format!(
                "relay #{} to {}, pending {}: {}",
                r.id,
                slot_name(r.to),
                crate::ctl::short_age(now - r.at),
                crate::ctl::cut(&r.text, 80)
            )
        })
        .collect();
    if !mine.is_empty() {
        out.push_str("\nYour pending relays:\n");
        out.push_str(&mine.join("\n"));
    }
    out
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
            overseer_proto::local_hhmm(c.since)
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
    let Some(slot) = slot else {
        if name == "update_current_session" {
            return Ok(tool_result(NEEDS_A_SLOT, true));
        }
        return Ok(tool_result(NOT_IN_A_SLOT, true));
    };
    let reply = match call(req) {
        Err(reason) => tool_result(format!("Overseer is not reachable: {reason}"), true),
        Ok(Response::Error { message }) => tool_result(message, true),
        Ok(Response::Text { text }) => tool_result(text, false),
        Ok(Response::Queued { message }) => tool_result(message, false),
        Ok(Response::Locks(view)) => {
            tool_result(format_locks_for(&view, slot, crate::ctl::now_secs()), false)
        }
        Ok(Response::Board(view)) => tool_result(
            crate::ctl::format_board(&view, crate::ctl::now_secs(), &crate::ctl::user_name()),
            false,
        ),
        Ok(Response::Work(view)) => {
            let lines = crate::ctl::format_work(&view);
            let text = if lines.is_empty() {
                "No work items.".to_string()
            } else {
                lines.join("\n")
            };
            tool_result(text, false)
        }
        Ok(Response::ItemDetail(d)) => tool_result(detail_text(&d), false),
        Ok(Response::Ok) => tool_result("Done.", false),
        Ok(other) => tool_result(format!("Unexpected reply from Overseer: {other:?}"), true),
    };
    Ok(with_stale_relays(reply, slot, call))
}

/// `reply` with any relays to `slot` that have waited too long to be typed
/// in added as a second text block (#58). Asking is best effort: a missing
/// or failing answer leaves `reply` as it is.
fn with_stale_relays(mut reply: Value, slot: SlotId, call: &mut Call) -> Value {
    if let Ok(Response::Text { text }) = call(Request::TakeStaleRelays { slot }) {
        if !text.is_empty() {
            if let Some(content) = reply["content"].as_array_mut() {
                content.push(json!({"type": "text", "text": text}));
            }
        }
    }
    reply
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
    let within = call_within(&req);
    rt.block_on(async {
        tokio::time::timeout(within, async {
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
            if matches!(r, Request::TakeStaleRelays { .. }) {
                return Ok(Response::Text {
                    text: String::new(),
                });
            }
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
    fn brain_tools_send_the_calling_slot() {
        let cases = [
            (
                "commit_2ndbrain",
                json!({"paths": ["a.md", "b.md"], "message": "S2: notes"}),
                Request::Commit {
                    from: Some(2),
                    paths: vec!["a.md".into(), "b.md".into()],
                    message: "S2: notes".into(),
                },
            ),
            (
                "append_journal",
                json!({"section": "Work Log", "text": "did a thing"}),
                Request::AppendJournal {
                    from: Some(2),
                    section: "Work Log".into(),
                    text: "did a thing".into(),
                },
            ),
            (
                "edit_shared_file",
                json!({"path": "Sprints/sprint-26-01/SPRINT-DASHBOARD.md", "old_text": "a", "new_text": "b"}),
                Request::EditShared {
                    from: Some(2),
                    path: "Sprints/sprint-26-01/SPRINT-DASHBOARD.md".into(),
                    old_text: "a".into(),
                    new_text: "b".into(),
                },
            ),
            (
                "update_current_session",
                json!({"text": "working on DEMO-12"}),
                Request::UpdateCurrentSession {
                    slot: 2,
                    text: "working on DEMO-12".into(),
                },
            ),
            (
                "sync_2ndbrain",
                json!({}),
                Request::SyncNotes {
                    from: Some(2),
                    confirm: false,
                },
            ),
            (
                "sync_2ndbrain",
                json!({"confirm": true}),
                Request::SyncNotes {
                    from: Some(2),
                    confirm: true,
                },
            ),
            (
                "restore_2ndbrain_file",
                json!({"path": "notes.md", "rev": "abc1234"}),
                Request::RestoreNotesFile {
                    from: Some(2),
                    path: "notes.md".into(),
                    rev: "abc1234".into(),
                },
            ),
        ];
        for (name, args, want) in cases {
            let (out, seen) = call_with(
                &tool(name, args),
                Some(2),
                Ok(Response::Text {
                    text: "committed abc1234: S2".into(),
                }),
            );
            assert_eq!(seen, [want], "{name}");
            assert_eq!(text_of(&out.unwrap()), ("committed abc1234: S2", false));
        }
    }

    #[test]
    fn brain_tool_argument_errors_are_rpc_errors() {
        let bad = [
            (
                "commit_2ndbrain",
                json!({"message": "m"}),
                "missing `paths`",
            ),
            (
                "commit_2ndbrain",
                json!({"paths": "a.md", "message": "m"}),
                "`paths` must be an array of strings",
            ),
            (
                "commit_2ndbrain",
                json!({"paths": ["a.md", 3], "message": "m"}),
                "`paths` must be an array of strings",
            ),
            ("commit_2ndbrain", json!({"paths": []}), "missing `message`"),
            ("append_journal", json!({"section": "S"}), "missing `text`"),
            (
                "append_journal",
                json!({"section": "S", "text": 5}),
                "`text` must be a string",
            ),
            (
                "edit_shared_file",
                json!({"path": "p", "old_text": "a"}),
                "missing `new_text`",
            ),
            (
                "update_current_session",
                json!({"text": ["x"]}),
                "`text` must be a string",
            ),
            ("update_current_session", json!({}), "missing `text`"),
            (
                "sync_2ndbrain",
                json!({"confirm": "yes"}),
                "`confirm` must be true or false",
            ),
            (
                "restore_2ndbrain_file",
                json!({"path": "notes.md"}),
                "missing `rev`",
            ),
        ];
        for (name, args, why) in bad {
            let (out, seen) = call_with(&tool(name, args), Some(1), Ok(Response::Ok));
            let out = out.unwrap();
            assert_eq!(out["error"]["code"], -32602, "{name}: {out}");
            assert_eq!(out["error"]["message"], why, "{name}");
            assert!(seen.is_empty(), "{name} must not reach the daemon");
        }
    }

    #[test]
    fn brain_tools_outside_a_slot_are_tool_errors() {
        for (name, args, want) in [
            (
                "commit_2ndbrain",
                json!({"paths": ["a"], "message": "m"}),
                NOT_IN_A_SLOT,
            ),
            (
                "update_current_session",
                json!({"text": "t"}),
                "update_current_session needs a slot; use `overseer ctl current-session --slot S<n>`",
            ),
        ] {
            let (out, seen) = call_with(&tool(name, args), None, Ok(Response::Ok));
            assert_eq!(text_of(&out.unwrap()), (want, true));
            assert!(seen.is_empty());
        }
    }

    #[test]
    fn a_queued_commit_is_success_and_a_daemon_error_is_not() {
        let args = json!({"paths": ["a.md"], "message": "m"});
        let (out, _) = call_with(
            &tool("commit_2ndbrain", args.clone()),
            Some(1),
            Ok(Response::Queued {
                message: "index.lock held, will retry".into(),
            }),
        );
        assert_eq!(
            text_of(&out.unwrap()),
            ("index.lock held, will retry", false)
        );
        let (out, _) = call_with(
            &tool("commit_2ndbrain", args),
            Some(1),
            Ok(Response::Error {
                message: "the notes writer is off (brain_writer in config.toml)".into(),
            }),
        );
        let out = out.unwrap();
        assert!(text_of(&out).1);
        assert!(text_of(&out).0.contains("writer is off"));
    }

    #[test]
    fn brain_calls_wait_longer_than_lock_calls() {
        assert!(
            call_within(&Request::ListLocks)
                < call_within(&Request::UpdateCurrentSession {
                    slot: 1,
                    text: String::new()
                })
        );
        assert!(
            call_within(&Request::Commit {
                from: Some(1),
                paths: vec![],
                message: String::new()
            }) > Duration::from_secs(600)
        );
    }

    #[test]
    fn item_detail_outlasts_the_daemons_rest_worst_case() {
        let within = call_within(&Request::GetItemDetail {
            key: "DEMO-12".into(),
            refresh: false,
        });
        // 5 s PATH wait + 30 s Keychain + 60 s REST.
        assert!(within > Duration::from_secs(95), "{within:?}");
        assert!(within < BRAIN_CALL_WITHIN);
    }

    #[test]
    fn stale_relays_follow_the_tool_reply_as_a_second_block() {
        let msg = tool("get_board", json!({}));
        let mut asked = Vec::new();
        let out = handle(&msg, Some(3), &mut |r| {
            asked.push(r.clone());
            Ok(match r {
                Request::TakeStaleRelays { .. } => Response::Text {
                    text: "Overseer: DEMO-40 was moved to S3".into(),
                },
                _ => Response::Ok,
            })
        })
        .unwrap();
        assert_eq!(asked[1], Request::TakeStaleRelays { slot: 3 });
        let content = out["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "Done.");
        assert_eq!(content[1]["text"], "Overseer: DEMO-40 was moved to S3");
        assert_eq!(out["result"]["isError"], false);
        // None waiting, or no answer: the reply is unchanged.
        for stale in [
            Ok(Response::Text {
                text: String::new(),
            }),
            Err("timed out".to_string()),
        ] {
            let out = handle(&msg, Some(3), &mut |r| match r {
                Request::TakeStaleRelays { .. } => stale.clone(),
                _ => Ok(Response::Ok),
            })
            .unwrap();
            assert_eq!(out["result"]["content"].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn tools_list_has_eighteen_tools() {
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
                "drop_relay",
                "list_targets",
                "commit_2ndbrain",
                "append_journal",
                "edit_shared_file",
                "update_current_session",
                "sync_2ndbrain",
                "restore_2ndbrain_file",
                "get_board",
                "get_work_items",
                "get_item_detail",
                "propose_plan",
                "request_status",
                "report_progress",
                "set_board_summary"
            ]
        );
        assert_eq!(names.len(), 18);
    }

    fn demo_detail() -> ItemDetail {
        ItemDetail {
            key: "DEMO-12".into(),
            kind: overseer_proto::ItemKind::Ticket,
            title: "Fix the login".into(),
            status: "In Progress".into(),
            priority: None,
            stage: None,
            stage_since: None,
            folder: None,
            body: None,
            notes: None,
            depends_on: vec![],
            children: vec![],
            prs: vec![],
            overview: None,
            fetched_at: None,
            error: None,
        }
    }

    #[test]
    fn detail_text_lists_body_prs_and_children() {
        let d = ItemDetail {
            priority: Some("High".into()),
            stage: Some(overseer_proto::Stage::InProgress),
            body: Some("Login fails on retry.".into()),
            notes: Some("tried a reset".into()),
            depends_on: vec!["DEMO-3".into(), "DEMO-4".into()],
            prs: vec![overseer_proto::PullRequest {
                repo: "demo-api".into(),
                id: 7,
                title: "Retry the login".into(),
                branch: "demo-12".into(),
                url: None,
                author: None,
                created: None,
                ticket: None,
                approvals: None,
                build: None,
                comments: None,
                head: None,
            }],
            children: vec![overseer_proto::ChildBead {
                key: "ov-3".into(),
                title: "Add the retry".into(),
                status: "open".into(),
            }],
            folder: Some("/demo/DEMO-12".into()),
            ..demo_detail()
        };
        let label = overseer_proto::Stage::InProgress.label();
        assert_eq!(
            detail_text(&d),
            format!(
                "DEMO-12 Fix the login\nStatus: In Progress \u{b7} Priority: High \u{b7} Stage: {label}\n\nLogin fails on retry.\n\nNotes:\ntried a reset\n\nDepends on: DEMO-3, DEMO-4\n\nPull requests:\n- demo-api #7 Retry the login\n\nChild beads:\n- ov-3 open Add the retry\n\nFolder: /demo/DEMO-12"
            )
        );
    }

    #[test]
    fn detail_text_shows_the_error_and_overview() {
        let d = ItemDetail {
            error: Some("acli failed".into()),
            overview: Some("A demo project.".into()),
            ..demo_detail()
        };
        assert_eq!(
            detail_text(&d),
            "DEMO-12 Fix the login\nStatus: In Progress \u{b7} Priority: not set\n\nCould not fetch the description: acli failed\n\nFrom the folder overview: A demo project."
        );
        assert_eq!(
            detail_text(&demo_detail()),
            "DEMO-12 Fix the login\nStatus: In Progress \u{b7} Priority: not set\n\nNo description"
        );
    }

    #[test]
    fn detail_text_keeps_the_last_good_copy_on_an_error() {
        let d = ItemDetail {
            error: Some("acli failed".into()),
            body: Some("Login fails on retry.".into()),
            overview: Some("A demo project.".into()),
            ..demo_detail()
        };
        assert_eq!(
            detail_text(&d),
            "DEMO-12 Fix the login\nStatus: In Progress \u{b7} Priority: not set\n\nCould not fetch the description: acli failed\n\nLast fetched copy:\nLogin fails on retry."
        );
        let blank = ItemDetail {
            body: Some("  \n".into()),
            ..d
        };
        assert_eq!(
            detail_text(&blank),
            "DEMO-12 Fix the login\nStatus: In Progress \u{b7} Priority: not set\n\nCould not fetch the description: acli failed\n\nFrom the folder overview: A demo project."
        );
    }

    #[test]
    fn detail_text_falls_back_to_the_overview_without_an_error() {
        let d = ItemDetail {
            overview: Some("A demo project.".into()),
            ..demo_detail()
        };
        assert_eq!(
            detail_text(&d),
            "DEMO-12 Fix the login\nStatus: In Progress \u{b7} Priority: not set\n\nFrom the folder overview: A demo project."
        );
        let blank = ItemDetail {
            body: Some(" ".into()),
            ..d
        };
        assert_eq!(
            detail_text(&blank),
            "DEMO-12 Fix the login\nStatus: In Progress \u{b7} Priority: not set\n\nFrom the folder overview: A demo project."
        );
    }

    #[test]
    fn bad_params_are_32602() {
        for msg in [
            tool("claim_target", json!({"reason": "push"})),
            tool("claim_target", json!({"target": 5, "reason": "push"})),
            tool("claim_target", json!({"target": "a", "reason": 5})),
            tool("release_target", json!({})),
            tool("notify_session", json!({"slot": "main", "text": "hi"})),
            tool("notify_session", json!({"slot": 1.5, "text": "hi"})),
            tool("notify_session", json!({"slot": "S1"})),
            tool("drop_relay", json!({})),
            tool("drop_relay", json!({"id": 7, "to": "S2"})),
            tool("drop_relay", json!({"id": "seven"})),
            tool("drop_relay", json!({"to": "main"})),
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
    fn claim_without_a_reason_is_sent_without_one() {
        let msg = tool("claim_target", json!({"target": "custom:demo-lane"}));
        let (out, seen) = call_with(&msg, Some(3), Ok(Response::Ok));
        assert_eq!(
            seen,
            [Request::ClaimTarget {
                slot: 3,
                target: "custom:demo-lane".into(),
                reason: None,
                item: None,
            }]
        );
        assert!(out.unwrap().get("error").is_none());
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
    fn drop_relay_sends_the_calling_slot_with_an_id_or_a_slot() {
        let dropped = || {
            Ok(Response::Text {
                text: "Dropped relay 7.".into(),
            })
        };
        let (out, seen) = call_with(&tool("drop_relay", json!({"id": 7})), Some(0), dropped());
        assert_eq!(
            seen,
            [Request::DropRelays {
                from: Some(0),
                id: Some(7),
                to: None
            }]
        );
        assert_eq!(text_of(&out.unwrap()), ("Dropped relay 7.", false));
        for to in [json!("S2"), json!(2)] {
            let (_, seen) = call_with(&tool("drop_relay", json!({"to": to})), Some(3), dropped());
            assert_eq!(
                seen,
                [Request::DropRelays {
                    from: Some(3),
                    id: None,
                    to: Some(2)
                }]
            );
        }
    }

    #[test]
    fn list_targets_shows_the_callers_pending_relays() {
        use overseer_proto::{LockView, Relay, RelayKind, RelayState, Sender};
        let relay = |id, from, state| Relay {
            id,
            at: 100,
            from,
            to: 2,
            text: "status?\nmore".into(),
            state,
            delivered_at: None,
            dropped_at: None,
            kind: RelayKind::Note,
            drop_reason: None,
            held: None,
            held_since: None,
        };
        let v = LockView {
            claims: vec![],
            waits: vec![],
            relays: vec![
                relay(9, Sender::You, RelayState::Pending),
                relay(8, Sender::Slot { slot: 0 }, RelayState::Delivered),
                relay(7, Sender::Slot { slot: 0 }, RelayState::Pending),
            ],
            flag_after_secs: 3600,
        };
        assert_eq!(
            format_locks_for(&v, 0, 100 + 5 * 60),
            "No targets are held.\nYour pending relays:\nrelay #7 to S2, pending 5m: status?..."
        );
        // The same ages as ctl and the GUI: hours, then days.
        assert!(
            format_locks_for(&v, 0, 100 + 5 * 3600).ends_with("pending 5h: status?..."),
            "{}",
            format_locks_for(&v, 0, 100 + 5 * 3600)
        );
        assert!(format_locks_for(&v, 0, 100 + 3 * 86_400).ends_with("pending 3d: status?..."));
        assert_eq!(format_locks_for(&v, 3, 400), "No targets are held.");
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

    #[test]
    fn propose_plan_takes_slot_names_and_numbers() {
        let args = json!({
            "summary": "two lanes",
            "slots": [
                {"slot": "S3", "label": "demo lane", "items": ["DEMO-12"],
                 "targets": [{"target": "repo:demo-api:main", "reason": "merge"}],
                 "worktree": {"repo": "/srv/demo-api", "ticket": "DEMO-12"},
                 "rules": ["Do not deploy before S2 merges"]},
                {"slot": 3}
            ]
        });
        let (_, seen) = call_with(
            &tool("propose_plan", args),
            Some(0),
            Ok(Response::Text { text: "ok".into() }),
        );
        let Request::ProposePlan { from, plan } = &seen[0] else {
            panic!("{seen:?}")
        };
        assert_eq!(*from, Some(0));
        assert_eq!(plan.summary, "two lanes");
        assert_eq!(plan.slots[0].slot, 3);
        assert_eq!(plan.slots[1].slot, 3);
        assert_eq!(plan.slots[0].targets[0].target, "repo:demo-api:main");
        assert_eq!(plan.slots[0].worktree.as_ref().unwrap().ticket, "DEMO-12");
        assert_eq!(plan.slots[0].rules.len(), 1);
    }

    #[test]
    fn propose_plan_rejects_a_bad_slot_shape() {
        let args = json!({"summary": "x", "slots": [{"slot": true}]});
        let (out, seen) = call_with(&tool("propose_plan", args), Some(0), Ok(Response::Ok));
        assert!(seen.is_empty());
        assert_eq!(out.unwrap()["error"]["code"], -32602);
    }

    #[test]
    fn report_progress_without_a_slot_is_refused() {
        let (out, seen) = call_with(
            &tool("report_progress", json!({"text": "done"})),
            None,
            Ok(Response::Ok),
        );
        assert!(seen.is_empty());
        assert_eq!(text_of(&out.unwrap()), (NOT_IN_A_SLOT, true));
    }

    #[test]
    fn report_progress_is_from_the_calling_slot() {
        let (_, seen) = call_with(
            &tool("report_progress", json!({"text": "done"})),
            Some(4),
            Ok(Response::Ok),
        );
        assert!(matches!(
            &seen[0],
            Request::ReportProgress { slot: 4, text } if text == "done"
        ));
    }
}
