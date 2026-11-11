//! The environment sessions start with.
//!
//! Under launchd the daemon gets a minimal environment (no `LANG`, nothing
//! from the user's shell rc files), so at startup it asks the user's login
//! shell for its environment once and uses that as the base for every slot.
//! If the shell fails or takes too long, the daemon's own environment is used
//! instead.
//!
//! Either way, variables that belong to a running parent Claude Code session
//! (when the daemon itself was started from inside one) are removed, so a
//! session in a slot does not think it is a child of another session.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// How long the login shell gets to print its environment.
pub const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// Variables that identify a running parent Claude Code session. They are
/// set by Claude Code for the processes it runs and are never user
/// configuration, so they are always removed from a slot's environment.
pub const SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SDK_HAS_HOST_AUTH_REFRESH",
    "CLAUDE_AGENT_SDK_VERSION",
    "AI_AGENT",
];

/// Prefixes of session-scoped variables, removed like [`SESSION_VARS`]
/// (`CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_SESSION_ATTENDED` and so on).
pub const SESSION_PREFIXES: &[&str] = &["CLAUDE_CODE_SESSION"];

/// Set by a parent session for its children, but also something a user may
/// export in their shell rc (`claude setup-token`). It is removed only when
/// the daemon's own environment shows it was started inside a Claude Code
/// session (`CLAUDECODE` is set), and then only from the environment the
/// login shell starts with, so a token the user's rc files set survives.
pub const PARENT_TOKEN_VARS: &[&str] = &["CLAUDE_CODE_OAUTH_TOKEN"];

/// Variables the Claude desktop parent sets for its own sessions (account,
/// host plumbing, feature switches, `GIT_EDITOR`). A slot session must not
/// inherit them. Names already in [`SESSION_VARS`] are not repeated here.
/// Removed like [`PARENT_TOKEN_VARS`]: only inside a parent session and only
/// from the environment the login shell starts with, so values the user's rc
/// files set (`GIT_EDITOR=vim`) survive. User preferences a parent also sets
/// (`CLAUDE_CODE_NO_FLICKER`, `CLAUDE_CODE_DISABLE_TERMINAL_TITLE`,
/// `CLAUDE_CODE_ENABLE_TELEMETRY`) are deliberately not listed.
pub const PARENT_HOST_VARS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_ACCOUNT_UUID",
    "CLAUDE_CODE_ORGANIZATION_UUID",
    "CLAUDE_CODE_USER_EMAIL",
    "CLAUDE_CODE_OAUTH_SCOPES",
    "CLAUDE_CODE_HOST_SHORT_TEMP_DIRS",
    "CLAUDE_CODE_EMIT_STARTUP_TIMING",
    "CLAUDE_CODE_EMIT_TOOL_USE_SUMMARIES",
    "CLAUDE_CODE_STARTUP_FAILURE_RESULTS",
    "CLAUDE_CODE_REPORT_FINDINGS",
    "CLAUDE_CODE_TERMINAL_MCP_TOOLS",
    "CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL",
    "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
    "CLAUDE_CODE_EAGER_FLUSH",
    "CLAUDE_CODE_DISABLE_CRON",
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_EFFORT",
    "CLAUDE_PREVIEW_CLASSIFIER_FLOOR",
    "GIT_EDITOR",
];

/// Prefixes of host variables, removed like [`PARENT_HOST_VARS`].
pub const PARENT_HOST_PREFIXES: &[&str] = &["CLAUDE_CODE_DESKTOP_", "CLAUDE_AGENT_SDK_", "MCP_"];

/// Shell bookkeeping that means nothing to a new process.
const SHELL_LOCAL_VARS: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

/// Locale used when the environment names none, so programs in the session
/// treat output as UTF-8.
const DEFAULT_LANG: &str = "en_US.UTF-8";

pub type Vars = Vec<(OsString, OsString)>;

fn is_session_var(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    SESSION_VARS.contains(&name) || SESSION_PREFIXES.iter().any(|p| name.starts_with(p))
}

fn is_parent_only_var(name: &str) -> bool {
    PARENT_TOKEN_VARS.contains(&name)
        || PARENT_HOST_VARS.contains(&name)
        || PARENT_HOST_PREFIXES.iter().any(|p| name.starts_with(p))
}

fn inside_parent_session(vars: &[(OsString, OsString)]) -> bool {
    vars.iter().any(|(k, _)| k == "CLAUDECODE")
}

/// Removes parent-session variables. With `from_parent`, also removes
/// [`PARENT_TOKEN_VARS`], [`PARENT_HOST_VARS`] and [`PARENT_HOST_PREFIXES`].
pub fn scrub(vars: Vars, from_parent: bool) -> Vars {
    vars.into_iter()
        .filter(|(k, _)| {
            !is_session_var(k) && !(from_parent && k.to_str().is_some_and(is_parent_only_var))
        })
        .collect()
}

/// The daemon's own environment, scrubbed. The fallback base.
pub fn own_env() -> Vars {
    let vars: Vars = std::env::vars_os().collect();
    let parent = inside_parent_session(&vars);
    finish(scrub(vars, parent))
}

/// Final touches shared by both sources: drop shell bookkeeping and make
/// sure a locale is set.
fn finish(mut vars: Vars) -> Vars {
    vars.retain(|(k, _)| !k.to_str().is_some_and(|k| SHELL_LOCAL_VARS.contains(&k)));
    let has_locale = vars
        .iter()
        .any(|(k, v)| matches!(k.to_str(), Some("LANG" | "LC_ALL" | "LC_CTYPE")) && !v.is_empty());
    if !has_locale {
        vars.push(("LANG".into(), DEFAULT_LANG.into()));
    }
    vars
}

/// The user's login shell: `$SHELL`, else the passwd entry, else zsh.
pub fn user_shell() -> PathBuf {
    if let Some(s) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
        return s.into();
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; it is
    // read at once, on this thread, before any other passwd call.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            let s = std::ffi::CStr::from_ptr((*pw).pw_shell);
            if !s.to_bytes().is_empty() {
                return OsStr::from_bytes(s.to_bytes()).into();
            }
        }
    }
    PathBuf::from("/bin/zsh")
}

const BEGIN: &str = "__OVERSEER_ENV_BEGIN__";
const END: &str = "__OVERSEER_ENV_END__";

/// Parses `env -0` output found between the markers.
fn parse_env_output(out: &[u8]) -> Option<Vars> {
    let begin = find(out, BEGIN.as_bytes())? + BEGIN.len();
    let end = begin + find(&out[begin..], END.as_bytes())?;
    let vars: Vars = out[begin..end]
        .split(|&b| b == 0)
        .filter(|e| !e.is_empty())
        .filter_map(|e| {
            let eq = e.iter().position(|&b| b == b'=')?;
            (eq > 0).then(|| {
                (
                    OsString::from_vec(e[..eq].to_vec()),
                    OsString::from_vec(e[eq + 1..].to_vec()),
                )
            })
        })
        .collect();
    (!vars.is_empty()).then_some(vars)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Runs `shell -l -i -c` to print its environment and parses it. The shell
/// starts from `start_env`, with stdin from /dev/null, in a session of its
/// own (so it can never take over a terminal), and is killed with its whole
/// process group after `timeout`.
pub fn login_shell_env(shell: &Path, start_env: &Vars, timeout: Duration) -> Result<Vars, String> {
    let script = format!("printf '%s' {BEGIN}; /usr/bin/env -0; printf '%s' {END}");
    let mut cmd = Command::new(shell);
    cmd.args(["-l", "-i", "-c", &script])
        .env_clear()
        .envs(start_env.iter().map(|(k, v)| (k, v)))
        .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", shell.display()))?;
    let pid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    let kill_group = || {
        // SAFETY: plain syscall; the group is the shell's own session.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                kill_group();
                let _ = child.wait();
                return Err(format!("{} timed out after {timeout:?}", shell.display()));
            }
            Err(e) => {
                kill_group();
                return Err(format!("wait for {}: {e}", shell.display()));
            }
        }
    };
    // Something the rc files started in the background may still hold the
    // pipe open; the group kill ends it.
    let out = match rx.recv_timeout(Duration::from_millis(500)) {
        Ok(out) => out,
        Err(_) => {
            kill_group();
            rx.recv_timeout(Duration::from_millis(500))
                .map_err(|_| "shell output did not end".to_string())?
        }
    };
    parse_env_output(&out).ok_or_else(|| {
        format!(
            "{} exited with {status} without printing its environment",
            shell.display()
        )
    })
}

/// Resolves the base environment for slots: the login shell's, or the
/// daemon's own on failure. Logs the outcome (a count, never values).
pub fn resolve_base_env(shell: &Path, timeout: Duration) -> Vars {
    let own: Vars = std::env::vars_os().collect();
    let parent = inside_parent_session(&own);
    let start = scrub(own, parent);
    match login_shell_env(shell, &start, timeout) {
        Ok(vars) => {
            let vars = finish(scrub(vars, false));
            tracing::info!(
                "session environment from {} login shell: {} variables",
                shell.display(),
                vars.len()
            );
            vars
        }
        Err(e) => {
            let vars = finish(start);
            tracing::warn!(
                "could not read the login shell environment ({e}); sessions get the daemon's own environment ({} variables)",
                vars.len()
            );
            vars
        }
    }
}

/// Where the base environment comes from.
#[derive(Debug, Clone)]
pub enum EnvSource {
    /// The daemon's own environment, scrubbed. Used by tests.
    Inherit,
    /// As `Inherit`, with these variables set on top. Used by tests that
    /// must point a slot somewhere without touching the process environment.
    InheritWith(Vec<(String, String)>),
    /// The user's login shell, read once in the background at startup.
    LoginShell,
}

/// The base environment, possibly still being resolved. Cheap to clone.
#[derive(Clone)]
pub struct BaseEnv(watch::Receiver<Option<Arc<Vars>>>);

impl BaseEnv {
    /// A base environment that is ready now.
    pub fn ready(vars: Vars) -> BaseEnv {
        BaseEnv(watch::channel(Some(Arc::new(vars))).1)
    }

    /// Starts resolving from `source`. For `LoginShell` the shell runs on a
    /// thread of its own, so the daemon can bind its socket meanwhile.
    pub fn start(source: &EnvSource) -> BaseEnv {
        match source {
            EnvSource::Inherit => BaseEnv::ready(own_env()),
            EnvSource::InheritWith(extra) => {
                let mut vars = own_env();
                for (k, v) in extra {
                    vars.retain(|(name, _)| name != k.as_str());
                    vars.push((k.into(), v.into()));
                }
                BaseEnv::ready(vars)
            }
            EnvSource::LoginShell => {
                let (tx, rx) = watch::channel(None);
                let shell = user_shell();
                let spawned =
                    std::thread::Builder::new()
                        .name("login-env".into())
                        .spawn(move || {
                            let vars = resolve_base_env(&shell, LOGIN_SHELL_TIMEOUT);
                            let _ = tx.send(Some(Arc::new(vars)));
                        });
                if let Err(e) = spawned {
                    tracing::warn!("cannot start login shell thread ({e}); using own environment");
                    return BaseEnv::ready(own_env());
                }
                BaseEnv(rx)
            }
        }
    }

    /// Waits until resolved (bounded by the shell timeout).
    pub async fn wait(&mut self) {
        let _ = self.0.wait_for(Option::is_some).await;
    }

    /// The resolved environment, or the daemon's own if not resolved yet.
    pub fn get(&self) -> Arc<Vars> {
        self.0
            .borrow()
            .clone()
            .unwrap_or_else(|| Arc::new(own_env()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(pairs: &[(&str, &str)]) -> Vars {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn names(vars: &Vars) -> Vec<String> {
        vars.iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn scrub_removes_session_vars_and_keeps_config() {
        let vars = v(&[
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_ENTRYPOINT", "cli"),
            ("CLAUDE_CODE_SSE_PORT", "1234"),
            ("CLAUDE_CODE_SESSION_ID", "x"),
            ("CLAUDE_CODE_SESSION_ATTENDED", "1"),
            ("CLAUDE_CODE_MESSAGING_TOKEN", "t"),
            ("CLAUDE_PID", "9"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "tok"),
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("ANTHROPIC_API_KEY", "k"),
            ("AWS_PROFILE", "p"),
            ("HOME", "/h"),
        ]);
        let kept = names(&scrub(vars.clone(), false));
        assert_eq!(
            kept,
            [
                "CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_USE_BEDROCK",
                "ANTHROPIC_API_KEY",
                "AWS_PROFILE",
                "HOME"
            ]
        );
        let kept = names(&scrub(vars, true));
        assert!(!kept.contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_string()));
        assert!(kept.contains(&"CLAUDE_CODE_USE_BEDROCK".to_string()));
    }

    #[test]
    fn host_vars_dropped_inside_a_parent_session() {
        let host_names = [
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_ACCOUNT_UUID",
            "CLAUDE_CODE_ORGANIZATION_UUID",
            "CLAUDE_CODE_USER_EMAIL",
            "CLAUDE_CODE_OAUTH_SCOPES",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_HOST_SESSION_ID",
            "CLAUDE_CODE_HOST_SHORT_TEMP_DIRS",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_CODE_EMIT_STARTUP_TIMING",
            "CLAUDE_CODE_EMIT_TOOL_USE_SUMMARIES",
            "CLAUDE_CODE_STARTUP_FAILURE_RESULTS",
            "CLAUDE_CODE_REPORT_FINDINGS",
            "CLAUDE_CODE_TERMINAL_MCP_TOOLS",
            "CLAUDE_CODE_ENABLE_ASK_USER_QUESTION_TOOL",
            "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
            "CLAUDE_CODE_EAGER_FLUSH",
            "CLAUDE_CODE_DISABLE_CRON",
            "CLAUDE_CODE_SIMPLE",
            "CLAUDE_EFFORT",
            "CLAUDE_PID",
            "CLAUDE_PREVIEW_CLASSIFIER_FLOOR",
            "GIT_EDITOR",
            "CLAUDE_CODE_DESKTOP_FOO",
            "CLAUDE_AGENT_SDK_FOO",
            "MCP_FOO",
        ];
        let mut pairs: Vec<(&str, &str)> = host_names.iter().map(|n| (*n, "x")).collect();
        pairs.extend([
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_NO_FLICKER", "1"),
            ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
            ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
            ("PATH", "/usr/bin"),
        ]);
        let vars = v(&pairs);
        let parent = inside_parent_session(&vars);
        assert!(parent);
        let kept = names(&scrub(vars, parent));
        for n in host_names {
            assert!(!kept.contains(&n.to_string()), "{n} survived");
        }
        assert_eq!(
            kept,
            [
                "CLAUDE_CODE_NO_FLICKER",
                "CLAUDE_CODE_DISABLE_TERMINAL_TITLE",
                "CLAUDE_CODE_ENABLE_TELEMETRY",
                "PATH"
            ]
        );
    }

    #[test]
    fn host_vars_from_rc_files_survive() {
        let dir = temp_dir("hostrc");
        // The login shell's own rc files set these; its output is scrubbed
        // with from_parent = false, so they must stay.
        let shell = script(
            &dir,
            "rc-shell",
            "export GIT_EDITOR=vim MCP_TIMEOUT=5\nshift 3\neval \"$1\"",
        );
        let vars = login_shell_env(&shell, &path(), LOGIN_SHELL_TIMEOUT).unwrap();
        let kept = finish(scrub(vars, false));
        assert!(
            kept.contains(&("GIT_EDITOR".into(), "vim".into())),
            "{kept:?}"
        );
        assert!(
            kept.contains(&("MCP_TIMEOUT".into(), "5".into())),
            "{kept:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn finish_sets_lang_and_drops_shell_locals() {
        let out = finish(v(&[("PWD", "/x"), ("SHLVL", "2"), ("HOME", "/h")]));
        assert_eq!(names(&out), ["HOME", "LANG"]);
        let out = finish(v(&[("LC_ALL", "C.UTF-8")]));
        assert_eq!(names(&out), ["LC_ALL"]);
    }

    #[test]
    fn parses_marked_env_output() {
        let mut out = b"rc noise\n".to_vec();
        out.extend_from_slice(BEGIN.as_bytes());
        out.extend_from_slice(b"A=1\0B=two=parts\0MULTI=line\nnext\0");
        out.extend_from_slice(END.as_bytes());
        out.extend_from_slice(b"logout noise");
        let vars = parse_env_output(&out).unwrap();
        assert_eq!(
            vars,
            v(&[("A", "1"), ("B", "two=parts"), ("MULTI", "line\nnext")])
        );
        assert!(parse_env_output(b"no markers").is_none());
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn path() -> Vars {
        v(&[("PATH", "/usr/bin:/bin")])
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ov-env-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_env_from_a_login_shell() {
        let start = v(&[
            ("HOME", "/tmp"),
            ("FROM_START", "kept"),
            ("PATH", "/usr/bin:/bin"),
        ]);
        // /bin/sh -l -i -c works like the user's shell for this purpose.
        let vars = login_shell_env(Path::new("/bin/sh"), &start, LOGIN_SHELL_TIMEOUT).unwrap();
        assert!(
            vars.contains(&("FROM_START".into(), "kept".into())),
            "{vars:?}"
        );
    }

    #[test]
    fn shell_rc_values_win() {
        let dir = temp_dir("rc");
        // A fake shell that "sources an rc" and then runs the -c script.
        let shell = script(
            &dir,
            "fake-shell",
            "export FROM_RC=yes\nshift 3\neval \"$1\"",
        );
        let start = v(&[("PATH", "/usr/bin:/bin")]);
        let vars = login_shell_env(&shell, &start, LOGIN_SHELL_TIMEOUT).unwrap();
        assert!(vars.contains(&("FROM_RC".into(), "yes".into())), "{vars:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn slow_or_broken_shells_fail_fast() {
        let dir = temp_dir("bad");
        let slow = script(&dir, "slow-shell", "sleep 30");
        let t = Instant::now();
        let err = login_shell_env(&slow, &path(), Duration::from_millis(300)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        let broken = script(&dir, "broken-shell", "echo nope; exit 1");
        let err = login_shell_env(&broken, &path(), LOGIN_SHELL_TIMEOUT).unwrap_err();
        assert!(err.contains("without printing"), "{err}");
        let err =
            login_shell_env(&dir.join("missing"), &Vars::new(), LOGIN_SHELL_TIMEOUT).unwrap_err();
        assert!(err.contains("cannot run"), "{err}");
        // resolve_base_env falls back to the daemon's own environment.
        let vars = resolve_base_env(&slow, Duration::from_millis(200));
        assert!(vars.iter().any(|(k, _)| k == "PATH"), "{vars:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
