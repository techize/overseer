//! The Jira API token: read from the login Keychain, else the environment.
//!
//! The token never goes into a process's argv, a log line or an error
//! message. Saving it writes the `security` command to the tool's stdin.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::run_source;
use super::store::SOURCE_TIMEOUT;

/// The Keychain item's service. Its account is the Jira email.
pub const KEYCHAIN_SERVICE: &str = "overseer-jira";

/// The environment variable read when the Keychain has no token.
pub const TOKEN_VAR: &str = "ATLASSIAN_API_TOKEN";

const NO_ANSWER: &str = "Keychain did not answer";

/// The Jira token for `email`: the Keychain item first, then
/// `ATLASSIAN_API_TOKEN` in `vars`. `security` is the tool's path.
pub async fn jira_token(
    email: &str,
    vars: &[(OsString, OsString)],
    security: &str,
) -> Result<String, String> {
    jira_token_within(email, vars, security, SOURCE_TIMEOUT).await
}

pub(crate) async fn jira_token_within(
    email: &str,
    vars: &[(OsString, OsString)],
    security: &str,
    timeout: Duration,
) -> Result<String, String> {
    if let Some(tok) = keychain_token(email, vars, security, timeout).await? {
        return Ok(tok);
    }
    vars.iter()
        .find(|(k, _)| k == TOKEN_VAR)
        .and_then(|(_, v)| v.to_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("no Jira token: set it in Settings or {TOKEN_VAR}"))
}

/// The Keychain item for `email`, or `None` when there is none. The only
/// error is a timeout: a hang means a locked Keychain or an access prompt,
/// which is reported rather than quietly falling back.
async fn keychain_token(
    email: &str,
    vars: &[(OsString, OsString)],
    security: &str,
    timeout: Duration,
) -> Result<Option<String>, String> {
    let cmd = [
        security.to_owned(),
        "find-generic-password".into(),
        "-s".into(),
        KEYCHAIN_SERVICE.into(),
        "-a".into(),
        email.to_owned(),
        "-w".into(),
    ];
    match run_source(&cmd, Path::new("/"), timeout, vars).await {
        Ok(out) if !out.trim().is_empty() => Ok(Some(out.trim().to_owned())),
        Err(e) if e.starts_with("timed out") => Err(NO_ANSWER.into()),
        // Not found (exit 44) or any other failure.
        _ => Ok(None),
    }
}

/// Quote `s` for a `security -i` command line.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Store `token` as the Keychain item for `email`, replacing any old one.
/// The command goes to `security -i` on stdin, so the token is never in argv.
pub async fn save_jira_token(email: &str, token: &str, security: &str) -> Result<(), String> {
    save_jira_token_within(email, token, security, SOURCE_TIMEOUT).await
}

async fn save_jira_token_within(
    email: &str,
    token: &str,
    security: &str,
    timeout: Duration,
) -> Result<(), String> {
    let token = token.trim();
    // A line break would end the command early and start another one.
    if token.contains(['\n', '\r']) || email.contains(['\n', '\r']) {
        return Err("the Jira token and email must be one line".into());
    }
    let line = format!(
        "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {} -w {}\n",
        quote(email),
        quote(token)
    );
    let mut child = Command::new(security)
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{security}: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("security: no stdin")?;
    let run = async {
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("security: {e}"))?;
        drop(stdin);
        child
            .wait_with_output()
            .await
            .map_err(|e| format!("security: {e}"))
    };
    // stdout and stderr are read but never shown: they could echo the token.
    let out = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| NO_ANSWER.to_owned())??;
    if !out.status.success() {
        let code = out
            .status
            .code()
            .map_or("a signal".to_owned(), |c| c.to_string());
        return Err(format!(
            "Keychain did not save the Jira token: security exited with {code}"
        ));
    }
    // `security -i` need not report a failed command in its exit status, so
    // read the item back. This also catches quoting the parser mangled.
    let vars: Vec<(OsString, OsString)> = std::env::var_os("PATH")
        .map(|p| vec![("PATH".into(), p)])
        .unwrap_or_default();
    match keychain_token(email, &vars, security, timeout).await? {
        Some(stored) if stored == token => Ok(()),
        _ => Err("the Keychain did not store the token".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn path_var() -> Vec<(OsString, OsString)> {
        vec![("PATH".into(), std::env::var_os("PATH").unwrap())]
    }

    fn stub(dir: &Path, body: &str) -> String {
        let p: PathBuf = dir.join("security");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn token_from_keychain_then_env() {
        let t = tempfile::tempdir().unwrap();
        let found = stub(t.path(), "echo tok1");
        let tok = jira_token("demo@example.test", &path_var(), &found).await;
        assert_eq!(tok.unwrap(), "tok1");

        let t2 = tempfile::tempdir().unwrap();
        let missing = stub(t2.path(), "exit 44");
        let mut vars = path_var();
        vars.push(("ATLASSIAN_API_TOKEN".into(), "tok2".into()));
        let tok = jira_token("demo@example.test", &vars, &missing).await;
        assert_eq!(tok.unwrap(), "tok2");

        let e = jira_token("demo@example.test", &path_var(), &missing)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            "no Jira token: set it in Settings or ATLASSIAN_API_TOKEN"
        );
    }

    #[tokio::test]
    async fn keychain_timeout_is_reported() {
        let t = tempfile::tempdir().unwrap();
        let slow = stub(t.path(), "sleep 5; echo late");
        let mut vars = path_var();
        vars.push(("ATLASSIAN_API_TOKEN".into(), "tok2".into()));
        let e = jira_token_within(
            "demo@example.test",
            &vars,
            &slow,
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(e.contains("Keychain did not answer"), "{e}");
    }

    #[tokio::test]
    async fn save_token_never_in_argv() {
        let t = tempfile::tempdir().unwrap();
        let argv = t.path().join("argv");
        let stdin = t.path().join("stdin");
        // `-i` records argv and stdin; the read-back prints the stored token.
        let sec = stub(
            t.path(),
            &format!(
                "printf '%s\\n' \"$@\" >> '{argv}'\n\
                 if [ \"$1\" = -i ]; then cat > '{stdin}'; \
                 else printf '%s\\n' 'se\"cr\\et'; fi",
                argv = argv.display(),
                stdin = stdin.display()
            ),
        );
        save_jira_token("demo@example.test", r#"se"cr\et"#, &sec)
            .await
            .unwrap();
        let argv = std::fs::read_to_string(&argv).unwrap();
        assert!(argv.starts_with("-i\nfind-generic-password\n"), "{argv}");
        assert!(!argv.contains("cr"), "{argv}");
        assert_eq!(
            std::fs::read_to_string(&stdin).unwrap(),
            "add-generic-password -U -s overseer-jira -a \"demo@example.test\" -w \"se\\\"cr\\\\et\"\n"
        );
    }

    #[tokio::test]
    async fn save_token_that_was_not_stored_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        // `-i` swallows the command and exits 0; the read-back finds nothing.
        let sec = stub(
            t.path(),
            "if [ \"$1\" = -i ]; then cat > /dev/null; exit 0; else exit 44; fi",
        );
        let e = save_jira_token("demo@example.test", "zq9", &sec)
            .await
            .unwrap_err();
        assert_eq!(e, "the Keychain did not store the token");

        // A read-back that differs is also an error.
        let t2 = tempfile::tempdir().unwrap();
        let sec = stub(
            t2.path(),
            "if [ \"$1\" = -i ]; then cat > /dev/null; else echo other; fi",
        );
        let e = save_jira_token("demo@example.test", "zq9", &sec)
            .await
            .unwrap_err();
        assert_eq!(e, "the Keychain did not store the token");
    }

    #[tokio::test]
    async fn save_token_reports_a_failed_exit() {
        let t = tempfile::tempdir().unwrap();
        let sec = stub(t.path(), "cat > /dev/null; exit 3");
        let e = save_jira_token("demo@example.test", "zq9", &sec)
            .await
            .unwrap_err();
        assert!(e.contains('3'), "{e}");
        assert!(!e.contains("zq9"), "{e}");
    }
}
