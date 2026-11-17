//! Repo lists and open pull requests through the Bitbucket Cloud REST API.
//!
//! The calls block, so async code runs them through
//! [`run_blocking_for`](super::jira_api::run_blocking_for). The token is
//! passed per call and never stored, logged or put in an error. Each call
//! follows `next` and returns `{"values":[...]}`, the shape `parse_repos`
//! and `parse_prs` already read from `bb`.

use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use super::jira_api::Agent;

/// Where every request goes.
pub const DEFAULT_BASE: &str = "https://api.bitbucket.org/2.0/";

/// At most this many pages are read in one call.
const MAX_PAGES: usize = 20;

/// How long one HTTP request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A Bitbucket Cloud client for one Atlassian account.
#[derive(Debug)]
pub struct BitbucketApi {
    email: String,
    /// Ends in `/`.
    base: String,
    agent: Agent,
}

impl BitbucketApi {
    pub fn new(email: &str) -> BitbucketApi {
        BitbucketApi {
            email: email.trim().to_owned(),
            base: DEFAULT_BASE.to_owned(),
            agent: Agent(ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build()),
        }
    }

    #[cfg(test)]
    pub fn with_base(self, base: String) -> BitbucketApi {
        BitbucketApi { base, ..self }
    }

    fn auth(&self, token: &str) -> String {
        let raw = format!("{}:{token}", self.email);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    /// The repos of `workspace`, most recently updated first, only those in
    /// `project` when it is set. Blocking.
    pub fn repos(&self, token: &str, workspace: &str, project: &str) -> Result<String, String> {
        let mut url = format!(
            "{}repositories/{}?pagelen=100&sort=-updated_on",
            self.base,
            encode(workspace)
        );
        if !project.is_empty() {
            url.push_str("&q=");
            url.push_str(&encode(&format!("project.key=\"{project}\"")));
        }
        self.all_pages(token, url)
    }

    /// The open pull requests of `workspace/slug`. Blocking.
    pub fn open_prs(&self, token: &str, workspace: &str, slug: &str) -> Result<String, String> {
        let url = format!(
            "{}repositories/{}/{}/pullrequests?state=OPEN&pagelen=50",
            self.base,
            encode(workspace),
            encode(slug)
        );
        self.all_pages(token, url)
    }

    /// Every page's `values`, following `next`, as `{"values":[...]}`.
    /// A `next` that leaves `base` is not followed, so the token is only
    /// ever sent to Bitbucket.
    fn all_pages(&self, token: &str, first: String) -> Result<String, String> {
        let auth = self.auth(token);
        let mut values = Vec::new();
        let mut url = first;
        for _ in 0..MAX_PAGES {
            let text = read(
                self.agent
                    .0
                    .get(&url)
                    .set("Authorization", &auth)
                    .set("Accept", "application/json")
                    .call(),
                token,
            )?;
            let v: Value =
                serde_json::from_str(&text).map_err(|e| format!("Bitbucket output: {e}"))?;
            match v.get("values") {
                Some(Value::Array(page)) => values.extend(page.iter().cloned()),
                _ => return Err("Bitbucket output: no values list".into()),
            }
            match v.get("next").and_then(Value::as_str) {
                None => return Ok(json!({ "values": values }).to_string()),
                Some(next) if next.starts_with(&self.base) => url = next.to_owned(),
                Some(_) => return Err("Bitbucket gave a next page on another host".into()),
            }
        }
        tracing::warn!("bitbucket: stopped after {MAX_PAGES} pages");
        Ok(json!({ "values": values }).to_string())
    }
}

/// `text` from the server or the network with any copy of `token` taken
/// out, in case it was echoed back.
fn hide(text: &str, token: &str) -> String {
    if token.is_empty() {
        text.to_owned()
    } else {
        text.replace(token, "<hidden>")
    }
}

/// Percent-encode `s` for a URL path segment or query value.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The body of a successful response, or the error to show, which never
/// holds `token`.
fn read(r: Result<ureq::Response, ureq::Error>, token: &str) -> Result<String, String> {
    match r {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| format!("Bitbucket response unreadable: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(classify(code, &hide(&body, token)))
        }
        Err(ureq::Error::Transport(t)) => Err(format!(
            "Bitbucket unreachable: {}",
            hide(&t.to_string(), token)
        )),
    }
}

/// The message for a non-success HTTP status.
pub fn classify(status: u16, body: &str) -> String {
    if status == 401 || status == 403 {
        return format!("Bitbucket token rejected ({status}): check the token and its scopes");
    }
    match super::jira_api::body_summary(body) {
        Some(first) => format!("Bitbucket {status}: {first}"),
        None => format!("Bitbucket {status}"),
    }
}

/// The workspace and repo slug of a Bitbucket Cloud remote URL:
/// `https://[user@]bitbucket.org/ws/slug(.git)` or
/// `git@bitbucket.org:ws/slug(.git)`. `None` for anything else.
pub fn remote_target(remote_url: &str) -> Option<(String, String)> {
    let url = remote_url.trim();
    let path = if let Some(rest) = url.strip_prefix("https://") {
        let (host, path) = rest.split_once('/')?;
        let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
        if !host.eq_ignore_ascii_case("bitbucket.org") {
            return None;
        }
        path
    } else {
        url.strip_prefix("git@bitbucket.org:")?
    };
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (ws, slug) = path.split_once('/')?;
    if ws.is_empty() || slug.is_empty() || slug.contains('/') {
        return None;
    }
    Some((ws.to_owned(), slug.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::super::jira_api::tests::fake;
    use super::*;

    fn api(base: &str) -> BitbucketApi {
        BitbucketApi::new("demo@example.test").with_base(format!("{base}/2.0/"))
    }

    fn values(v: &str) -> Vec<Value> {
        let v: Value = serde_json::from_str(v).unwrap();
        v["values"].as_array().unwrap().clone()
    }

    #[test]
    fn repos_follows_next_and_filters_project() {
        let p1 = r#"{"values":[{"slug":"demo-api","updated_on":"2026-11-16T09:00:00+00:00"}],
            "next":"{base}/2.0/repositories/demo-ws?pagelen=100&page=2"}"#;
        let p2 = r#"{"values":[{"slug":"demo-web"}]}"#;
        let (base, rx) = fake(vec![(200, p1.into()), (200, p2.into())]);
        let out = api(&base).repos("tok", "demo-ws", "DEMO").unwrap();
        let got = super::super::parse_repos(&out).unwrap();
        let names: Vec<_> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["demo-api", "demo-web"]);
        assert_eq!(values(&out).len(), 2);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(
            seen[0].path,
            "/2.0/repositories/demo-ws?pagelen=100&sort=-updated_on&q=project.key%3D%22DEMO%22"
        );
        assert_eq!(seen[1].path, "/2.0/repositories/demo-ws?pagelen=100&page=2");
        for s in &seen {
            // base64("demo@example.test:tok")
            assert_eq!(
                s.auth.as_deref(),
                Some("Basic ZGVtb0BleGFtcGxlLnRlc3Q6dG9r")
            );
        }

        // No project: no filter.
        let (base, rx) = fake(vec![(200, r#"{"values":[]}"#.into())]);
        let out = api(&base).repos("tok", "demo-ws", "").unwrap();
        assert!(values(&out).is_empty());
        assert_eq!(
            rx.recv().unwrap().path,
            "/2.0/repositories/demo-ws?pagelen=100&sort=-updated_on"
        );
    }

    #[test]
    fn open_prs_follows_next() {
        let p1 = r#"{"values":[{"id":7,"title":"DEMO-12 widget","source":{"branch":{"name":"feature/DEMO-12"}},
            "author":{"display_name":"Dana Demo"},"links":{"html":{"href":"https://bitbucket.org/demo-ws/demo-api/pull-requests/7"}}}],
            "next":"{base}/2.0/repositories/demo-ws/demo-api/pullrequests?state=OPEN&pagelen=50&page=2"}"#;
        let p2 = r#"{"values":[{"id":8,"title":"Tidy logs"}],"page":2}"#;
        let (base, rx) = fake(vec![(200, p1.into()), (200, p2.into())]);
        let out = api(&base).open_prs("tok", "demo-ws", "demo-api").unwrap();
        let prs = super::super::parse_prs(&out, "demo-api").unwrap();
        let ids: Vec<_> = prs.iter().map(|p| p.id).collect();
        assert_eq!(ids, [7, 8]);
        assert_eq!(prs[0].author.as_deref(), Some("Dana Demo"));
        assert_eq!(prs[0].branch, "feature/DEMO-12");
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            seen[0].path,
            "/2.0/repositories/demo-ws/demo-api/pullrequests?state=OPEN&pagelen=50"
        );
        assert_eq!(
            seen[1].path,
            "/2.0/repositories/demo-ws/demo-api/pullrequests?state=OPEN&pagelen=50&page=2"
        );
    }

    #[test]
    fn a_next_page_on_another_host_is_not_followed() {
        let p1 = r#"{"values":[{"id":7}],"next":"https://elsewhere.example.test/2.0/x"}"#;
        let (base, _rx) = fake(vec![(200, p1.into())]);
        let e = api(&base)
            .open_prs("tok", "demo-ws", "demo-api")
            .unwrap_err();
        assert!(e.contains("another host"), "{e}");
    }

    #[test]
    fn rejected_token_is_an_error_not_an_empty_list() {
        let (base, _rx) = fake(vec![(401, r#"{"type":"error"}"#.into())]);
        let e = api(&base).repos("tok", "demo-ws", "").unwrap_err();
        assert_eq!(
            e,
            "Bitbucket token rejected (401): check the token and its scopes"
        );
        let (base, _rx) = fake(vec![(403, String::new())]);
        let e = api(&base)
            .open_prs("tok", "demo-ws", "demo-api")
            .unwrap_err();
        assert_eq!(
            e,
            "Bitbucket token rejected (403): check the token and its scopes"
        );
        assert_eq!(
            classify(404, "No repository\nmore"),
            "Bitbucket 404: No repository"
        );
    }

    #[test]
    fn errors_never_hold_the_token() {
        let token = "demo-secret-token";
        // A server that echoes the token back in its error body.
        let (base, _rx) = fake(vec![
            (500, format!("bad token {token}")),
            (401, token.to_owned()),
            (200, format!("not json {token}")),
        ]);
        let a = api(&base);
        let mut errors = vec![
            a.repos(token, "demo-ws", "").unwrap_err(),
            a.open_prs(token, "demo-ws", "demo-api").unwrap_err(),
            a.open_prs(token, "demo-ws", "demo-api").unwrap_err(),
        ];
        // Nothing listens: a transport error.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        errors.push(api(&closed).repos(token, "demo-ws", "").unwrap_err());
        assert!(errors[3].contains("Bitbucket unreachable"), "{}", errors[3]);
        for e in &errors {
            assert!(!e.is_empty());
            assert!(!e.contains(token), "{e}");
            assert!(!e.contains("secret"), "{e}");
        }
    }

    #[test]
    fn remote_target_reads_https_and_ssh() {
        let want = Some(("demo-ws".to_owned(), "demo-api".to_owned()));
        for url in [
            "https://bitbucket.org/demo-ws/demo-api",
            "https://bitbucket.org/demo-ws/demo-api.git",
            "https://dana@bitbucket.org/demo-ws/demo-api.git",
            "https://bitbucket.org/demo-ws/demo-api/",
            "git@bitbucket.org:demo-ws/demo-api.git",
            "git@bitbucket.org:demo-ws/demo-api",
            "  git@bitbucket.org:demo-ws/demo-api.git\n",
        ] {
            assert_eq!(remote_target(url), want, "{url}");
        }
    }

    #[test]
    fn remote_target_rejects_github() {
        for url in [
            "https://github.com/demo-ws/demo-api.git",
            "git@github.com:demo-ws/demo-api.git",
            "https://bitbucket.org.example.test/demo-ws/demo-api",
            "https://bitbucket.org/demo-ws",
            "https://bitbucket.org/demo-ws/demo-api/extra",
            "git@bitbucket.org:/demo-api.git",
            "",
        ] {
            assert_eq!(remote_target(url), None, "{url}");
        }
    }
}
