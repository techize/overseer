//! Reading beads and Jira output, and running the source commands.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct Bead {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
    pub external_ref: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    pub key: String,
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
}

fn str_of(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// Parse `bd list --json`, an array of issues.
pub fn parse_beads(json: &str) -> Result<Vec<Bead>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("bd output: {e}"))?;
    let arr = v.as_array().ok_or("bd output: expected an array")?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let id = str_of(item, "id").ok_or("bd output: issue without an id")?;
        let priority = match item.get("priority") {
            Some(Value::Number(n)) => Some(format!("P{n}")),
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        };
        out.push(Bead {
            id,
            title: str_of(item, "title").unwrap_or_default(),
            status: str_of(item, "status").unwrap_or_default(),
            priority,
            external_ref: str_of(item, "external_ref").filter(|s| !s.is_empty()),
            labels: item
                .get("labels")
                .and_then(Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Parse Jira command output. Text before the first `[` or `{` is skipped.
/// Accepts an array of issues or an object holding `issues`.
pub fn parse_jira(out: &str) -> Result<Vec<Ticket>, String> {
    let start = out.find(['[', '{']).ok_or("jira output: no JSON found")?;
    let v: Value = serde_json::Deserializer::from_str(&out[start..])
        .into_iter::<Value>()
        .next()
        .ok_or("jira output: no JSON found")?
        .map_err(|e| format!("jira output: {e}"))?;
    let arr = match &v {
        Value::Array(a) => a,
        Value::Object(_) => v
            .get("issues")
            .and_then(Value::as_array)
            .ok_or("jira output: no issues list")?,
        _ => return Err("jira output: unexpected JSON".into()),
    };
    let name = |f: Option<&Value>, k: &str| {
        f.and_then(|f| f.get(k))
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let Some(key) = str_of(item, "key") else {
            continue;
        };
        let f = item.get("fields");
        out.push(Ticket {
            key,
            title: f
                .and_then(|f| f.get("summary"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            status: name(f, "status").unwrap_or_default(),
            priority: name(f, "priority"),
        });
    }
    Ok(out)
}

fn describe(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{} ms", d.as_millis())
    }
}

/// Run a source command and return its stdout.
pub async fn run_source(
    cmd: &[String],
    cwd: &Path,
    timeout: Duration,
    env: &[(OsString, OsString)],
) -> Result<String, String> {
    let (prog, args) = cmd.split_first().ok_or("empty command")?;
    if !cwd.is_dir() {
        return Err(format!("folder {} not found", cwd.display()));
    }
    let mut c = Command::new(prog);
    c.args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let child = c.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{prog} not found")
        } else {
            format!("{prog}: {e}")
        }
    })?;
    let pgid = child.id();
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => {
            // The child leads its own group; kill it and anything it started.
            if let Some(pgid) = pgid {
                unsafe { libc::killpg(pgid as libc::pid_t, libc::SIGKILL) };
            }
            return Err(format!("timed out after {}", describe(timeout)));
        }
        Ok(r) => r.map_err(|e| format!("{prog}: {e}"))?,
    };
    if !out.status.success() {
        let code = out
            .status
            .code()
            .map_or("a signal".to_owned(), |c| c.to_string());
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        return Err(if first.is_empty() {
            format!("{prog} exited with {code}")
        } else {
            format!("{prog} exited with {code}: {first}")
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Vec<(OsString, OsString)> {
        vec![("PATH".into(), std::env::var_os("PATH").unwrap())]
    }

    #[test]
    fn parses_bd_list_fixture() {
        let beads = parse_beads(include_str!("../../tests/fixtures/bd-list.json")).unwrap();
        assert_eq!(beads.len(), 4);
        assert_eq!(beads[0].id, "ov-1");
        assert_eq!(beads[0].priority.as_deref(), Some("P1"));
        assert_eq!(beads[0].labels, ["ui"]);
        assert_eq!(
            beads[1].external_ref.as_deref(),
            Some("https://jira.example.test/browse/DEMO-12")
        );
        assert_eq!(beads[2].priority.as_deref(), Some("P3"));
        assert_eq!(beads[3].priority.as_deref(), Some("P0"));
    }

    #[test]
    fn jira_with_a_leading_warning_line() {
        let out = format!(
            "warning: using cached token\n{}",
            include_str!("../../tests/fixtures/acli-search.json")
        );
        let t = parse_jira(&out).unwrap();
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn jira_object_with_issues() {
        let t = parse_jira(include_str!("../../tests/fixtures/acli-search.json")).unwrap();
        assert_eq!(t[0].key, "DEMO-12");
        assert_eq!(t[0].title, "Widget wiring");
        assert_eq!(t[0].status, "In Progress");
        assert_eq!(t[0].priority.as_deref(), Some("High"));
        assert_eq!(t[1].priority, None);
        assert_eq!(t[2].title, "");
        let arr = parse_jira(r#"[{"key":"DEMO-1"}]"#).unwrap();
        assert_eq!(arr[0].key, "DEMO-1");
    }

    #[test]
    fn jira_junk_is_an_error() {
        assert!(parse_jira("not json at all").is_err());
        assert!(parse_jira("{ broken").is_err());
        assert!(parse_jira(r#"{"nope":1}"#).is_err());
    }

    #[tokio::test]
    async fn run_source_returns_stdout() {
        let out = run_source(
            &["echo".into(), "hi".into()],
            Path::new("."),
            Duration::from_secs(5),
            &env(),
        )
        .await
        .unwrap();
        assert_eq!(out.trim(), "hi");
    }

    #[tokio::test]
    async fn run_source_nonzero_exit() {
        let e = run_source(
            &["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()],
            Path::new("."),
            Duration::from_secs(5),
            &env(),
        )
        .await
        .unwrap_err();
        assert!(e.contains('3') && e.contains("boom"), "{e}");
    }

    #[tokio::test]
    async fn run_source_missing_program() {
        let e = run_source(
            &["no-such-prog-xyz".into()],
            Path::new("."),
            Duration::from_secs(5),
            &env(),
        )
        .await
        .unwrap_err();
        assert_eq!(e, "no-such-prog-xyz not found");
    }

    #[tokio::test]
    async fn run_source_times_out() {
        let e = run_source(
            &["sleep".into(), "5".into()],
            Path::new("."),
            Duration::from_millis(200),
            &env(),
        )
        .await
        .unwrap_err();
        assert!(e.starts_with("timed out after"), "{e}");
    }

    #[tokio::test]
    async fn run_source_timeout_kills_the_process_group() {
        let dir = std::env::temp_dir().join(format!("ov-pg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let e = run_source(
            &["sh".into(), "-c".into(), script],
            &dir,
            Duration::from_millis(500),
            &env(),
        )
        .await
        .unwrap_err();
        assert!(e.starts_with("timed out"), "{e}");
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..50 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "grandchild {pid} survived the timeout");
    }

    #[tokio::test]
    async fn run_source_missing_folder() {
        let e = run_source(
            &["echo".into()],
            Path::new("/no/such/folder-xyz"),
            Duration::from_secs(5),
            &env(),
        )
        .await
        .unwrap_err();
        assert_eq!(e, "folder /no/such/folder-xyz not found");
    }

    #[test]
    fn jira_malformed_entries_are_skipped() {
        let t = parse_jira(r#"[{"key":"DEMO-1"}, 7, {"key":5}, "x", {"key":"DEMO-2"}]"#).unwrap();
        let keys: Vec<_> = t.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, ["DEMO-1", "DEMO-2"]);
    }
}
