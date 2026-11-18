//! Jira tickets and the active sprint through the Jira Cloud REST API.
//!
//! Every Jira request goes to `https://api.atlassian.com/ex/jira/<cloudId>/`.
//! Only the cloud id lookup asks the site itself. The calls block, so async
//! code runs them through [`run_blocking`]. The token is passed per call and
//! never stored, logged or put in an error.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use super::sources::{parse_jira, Ticket};
use super::sprint::{parse_ymd, Ymd};

/// The fields asked for on each ticket.
const FIELDS: [&str; 3] = ["summary", "status", "priority"];

/// Tickets per page.
const PAGE_SIZE: u32 = 100;

/// At most this many pages are read in one call.
const MAX_PAGES: usize = 20;

/// How long one HTTP request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a whole blocking call, every page included, may take.
pub const API_TIMEOUT: Duration = Duration::from_secs(60);

/// The board's running sprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveSprint {
    pub id: u64,
    pub name: String,
    pub start: Ymd,
    pub end: Ymd,
    /// The keys of every issue in the sprint, from the board.
    pub keys: BTreeSet<String>,
}

/// A Jira Cloud client for one site, account and board.
#[derive(Debug)]
pub struct JiraApi {
    site_base: String,
    email: String,
    board: String,
    api_base: String,
    cloud_id: Mutex<Option<String>>,
    agent: Agent,
}

/// `ureq::Agent` without a `Debug` of its own.
pub(crate) struct Agent(pub(crate) ureq::Agent);

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Agent")
    }
}

impl JiraApi {
    /// `site` is the host, such as `demo.atlassian.net`. An empty
    /// `cloud_id` is looked up from the site on first use.
    pub fn new(site: &str, email: &str, board: &str, cloud_id: &str) -> JiraApi {
        let cloud_id = cloud_id.trim();
        JiraApi {
            site_base: format!("https://{}", crate::config::site_host(site)),
            email: email.trim().to_owned(),
            board: board.trim().to_owned(),
            api_base: "https://api.atlassian.com".to_owned(),
            cloud_id: Mutex::new((!cloud_id.is_empty()).then(|| cloud_id.to_owned())),
            agent: Agent(ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build()),
        }
    }

    #[cfg(test)]
    pub fn with_bases(self, api_base: String, site_base: String) -> JiraApi {
        JiraApi {
            api_base,
            site_base,
            ..self
        }
    }

    fn auth(&self, token: &str) -> String {
        let raw = format!("{}:{token}", self.email);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    fn jira_url(&self, rest: &str) -> Result<String, String> {
        Ok(format!(
            "{}/ex/jira/{}/{rest}",
            self.api_base,
            self.cloud_id()?
        ))
    }

    /// The site's cloud id: the configured one, else the site's
    /// `/_edge/tenant_info`, asked once and kept.
    pub fn cloud_id(&self) -> Result<String, String> {
        let mut cached = self.cloud_id.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = cached.as_ref() {
            return Ok(id.clone());
        }
        let url = format!("{}/_edge/tenant_info", self.site_base);
        let body = match self.agent.0.get(&url).call() {
            // Atlassian answers 404 for a site name it does not host.
            Err(ureq::Error::Status(404, _)) => {
                let host = self.site_base.trim_start_matches("https://");
                return Err(format!("Jira site not found: check jira_site ({host})"));
            }
            r => read(r)?,
        };
        let v: Value = serde_json::from_str(&body).map_err(|e| format!("Jira tenant info: {e}"))?;
        let id = v
            .get("cloudId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("Jira tenant info: no cloudId")?
            .to_owned();
        tracing::info!("jira cloud id {id}");
        *cached = Some(id.clone());
        Ok(id)
    }

    /// Every ticket `jql` finds, following `nextPageToken`. Blocking.
    pub fn tickets(&self, token: &str, jql: &str) -> Result<Vec<Ticket>, String> {
        let url = self.jira_url("rest/api/3/search/jql")?;
        let auth = self.auth(token);
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut body = json!({"jql": jql, "fields": FIELDS, "maxResults": PAGE_SIZE});
            if let Some(t) = &next {
                body["nextPageToken"] = json!(t);
            }
            let text = read(
                self.agent
                    .0
                    .post(&url)
                    .set("Authorization", &auth)
                    .set("Accept", "application/json")
                    .send_json(body),
            )?;
            out.extend(parse_jira(&text)?);
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("jira output: {e}"))?;
            let last = v.get("isLast").and_then(Value::as_bool).unwrap_or(false);
            next = v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if last || next.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// The board's active sprint with the keys of its issues, or `None`
    /// when it has none. Blocking.
    pub fn active_sprint(&self, token: &str) -> Result<Option<ActiveSprint>, String> {
        let url = self.jira_url(&format!(
            "rest/agile/1.0/board/{}/sprint?state=active",
            self.board
        ))?;
        let Some(mut sprint) = parse_active_sprint(&self.get(&url, token)?)? else {
            return Ok(None);
        };
        sprint.keys = self.sprint_keys(token, sprint.id)?;
        Ok(Some(sprint))
    }

    /// The keys of every issue in sprint `id`, page by page.
    fn sprint_keys(&self, token: &str, id: u64) -> Result<BTreeSet<String>, String> {
        let mut keys = BTreeSet::new();
        let mut start = 0usize;
        for _ in 0..MAX_PAGES {
            let url = self.jira_url(&format!(
                "rest/agile/1.0/sprint/{id}/issue?fields=summary&maxResults={PAGE_SIZE}&startAt={start}"
            ))?;
            let (page, total) = parse_sprint_issues(&self.get(&url, token)?)?;
            start += page.len();
            let done = page.is_empty() || total.is_some_and(|t| start >= t);
            keys.extend(page);
            if done {
                return Ok(keys);
            }
        }
        tracing::warn!("jira: sprint {id} issues stopped after {MAX_PAGES} pages");
        Ok(keys)
    }

    /// A GET's body. Blocking.
    fn get(&self, url: &str, token: &str) -> Result<String, String> {
        read(
            self.agent
                .0
                .get(url)
                .set("Authorization", &self.auth(token))
                .set("Accept", "application/json")
                .call(),
        )
    }
}

/// The body of a successful response, or the error to show.
fn read(r: Result<ureq::Response, ureq::Error>) -> Result<String, String> {
    match r {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| format!("Jira response unreadable: {e}")),
        Err(ureq::Error::Status(code, resp)) => {
            Err(classify(code, &resp.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("Jira unreachable: {t}")),
    }
}

/// The message for a non-success HTTP status.
pub fn classify(status: u16, body: &str) -> String {
    if status == 401 || status == 403 {
        return format!("Jira token rejected ({status}): check the token and its scopes");
    }
    match body_summary(body) {
        Some(first) => format!("Jira answered {status}: {first}"),
        None => format!("Jira answered {status}"),
    }
}

/// The first non-empty line of an error body, cut to 200 characters, or
/// `None` when there is none or the body is markup (an HTML error page
/// says nothing a person can act on).
pub(crate) fn body_summary(body: &str) -> Option<String> {
    let first = body.lines().map(str::trim).find(|l| !l.is_empty())?;
    if first.starts_with('<') {
        return None;
    }
    Some(first.chars().take(200).collect())
}

/// The date part of an ISO time such as `2026-11-17T09:00:00.000Z`.
fn date_of(v: &Value, key: &str) -> Option<Ymd> {
    let s = v.get(key)?.as_str()?;
    parse_ymd(s.split(['T', 't', ' ']).next()?)
}

/// Read `values[]` of the board's sprint list. The latest start wins;
/// sprints without both dates are skipped.
pub fn parse_active_sprint(json: &str) -> Result<Option<ActiveSprint>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("jira sprint: {e}"))?;
    let values = v
        .get("values")
        .and_then(Value::as_array)
        .ok_or("jira sprint: no values list")?;
    Ok(values
        .iter()
        .filter_map(|s| {
            Some(ActiveSprint {
                id: s.get("id")?.as_u64()?,
                name: s.get("name")?.as_str()?.to_owned(),
                start: date_of(s, "startDate")?,
                end: date_of(s, "endDate")?,
                keys: BTreeSet::new(),
            })
        })
        .max_by_key(|s| s.start))
}

/// One page of a sprint's issues: their keys and the `total` Jira reports.
pub fn parse_sprint_issues(json: &str) -> Result<(Vec<String>, Option<usize>), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("jira sprint issues: {e}"))?;
    let issues = v
        .get("issues")
        .and_then(Value::as_array)
        .ok_or("jira sprint issues: no issues list")?;
    let keys = issues
        .iter()
        .filter_map(|i| i.get("key")?.as_str().map(str::to_owned))
        .collect();
    let total = v.get("total").and_then(Value::as_u64).map(|t| t as usize);
    Ok((keys, total))
}

/// Run a blocking Jira call off the async runtime, giving up after
/// [`API_TIMEOUT`].
pub async fn run_blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    run_blocking_for("Jira", f).await
}

/// [`run_blocking`] for a call to `service`, which the errors name.
pub async fn run_blocking_for<T, F>(service: &str, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    match tokio::time::timeout(API_TIMEOUT, tokio::task::spawn_blocking(f)).await {
        Err(_) => Err(format!(
            "{service} did not answer within {} s",
            API_TIMEOUT.as_secs()
        )),
        Ok(Err(e)) => Err(format!("{service} call failed: {e}")),
        Ok(Ok(r)) => r,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// One request the fake server saw.
    pub(crate) struct Seen {
        pub(crate) method: String,
        pub(crate) path: String,
        pub(crate) auth: Option<String>,
        pub(crate) body: String,
    }

    /// Answer each connection with the next canned `(status, body)`, one
    /// request per connection, and hand back what was asked. `{base}` in a
    /// body becomes the server's own base URL.
    pub(crate) fn fake(replies: Vec<(u16, String)>) -> (String, mpsc::Receiver<Seen>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let own = base.clone();
        std::thread::spawn(move || {
            for (status, body) in replies {
                let body = body.replace("{base}", &own);
                let Ok((stream, _)) = l.accept() else { return };
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let (mut len, mut auth) = (0usize, None);
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        let v = v.trim().to_owned();
                        if k.eq_ignore_ascii_case("content-length") {
                            len = v.parse().unwrap();
                        } else if k.eq_ignore_ascii_case("authorization") {
                            auth = Some(v);
                        }
                    }
                }
                let mut buf = vec![0; len];
                r.read_exact(&mut buf).unwrap();
                let _ = tx.send(Seen {
                    method,
                    path,
                    auth,
                    body: String::from_utf8(buf).unwrap(),
                });
                let mut w = stream;
                let _ = write!(
                    w,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, rx)
    }

    fn api(base: &str) -> JiraApi {
        JiraApi::new("demo.atlassian.net", "demo@example.test", "7", "demo-cloud")
            .with_bases(base.to_owned(), base.to_owned())
    }

    fn issue(key: &str) -> String {
        format!(
            r#"{{"key":"{key}","fields":{{"summary":"t {key}","status":{{"name":"To Do"}},"priority":{{"name":"High"}}}}}}"#
        )
    }

    #[test]
    fn classify_rejected() {
        assert_eq!(
            classify(401, "x"),
            "Jira token rejected (401): check the token and its scopes"
        );
        assert_eq!(
            classify(403, ""),
            "Jira token rejected (403): check the token and its scopes"
        );
        assert_eq!(classify(500, "boom\nmore"), "Jira answered 500: boom");
        let long = "y".repeat(300);
        assert_eq!(
            classify(502, &long),
            format!("Jira answered 502: {}", "y".repeat(200))
        );
    }

    #[test]
    fn active_sprint_picks_latest_start() {
        let json = r#"{"values":[
            {"id":1,"name":"Sprint 41","state":"active",
             "startDate":"2026-11-03T09:00:00.000Z","endDate":"2026-11-14T17:00:00.000Z"},
            {"id":2,"name":"Sprint 42","state":"active",
             "startDate":"2026-11-17T09:00:00.000+00:00","endDate":"2026-11-28T17:00:00.000+00:00"},
            {"id":3,"name":"Undated","state":"active"}
        ]}"#;
        let s = parse_active_sprint(json).unwrap().unwrap();
        assert_eq!(s.name, "Sprint 42");
        assert_eq!(
            s.start,
            Ymd {
                y: 2026,
                m: 11,
                d: 17
            }
        );
        assert_eq!(
            s.end,
            Ymd {
                y: 2026,
                m: 11,
                d: 28
            }
        );
        assert_eq!(parse_active_sprint(r#"{"values":[]}"#).unwrap(), None);
        assert!(parse_active_sprint("nope").is_err());
    }

    #[test]
    fn paging_follows_next_token() {
        let p1 = format!(
            r#"{{"issues":[{},{}],"nextPageToken":"p2","isLast":false}}"#,
            issue("DEMO-1"),
            issue("DEMO-2")
        );
        let p2 = format!(r#"{{"issues":[{}],"isLast":true}}"#, issue("DEMO-3"));
        let (base, rx) = fake(vec![(200, p1), (200, p2)]);
        let got = api(&base).tickets("tok", "project = DEMO").unwrap();
        let keys: Vec<_> = got.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, ["DEMO-1", "DEMO-2", "DEMO-3"]);
        let seen: Vec<Seen> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        for s in &seen {
            assert_eq!(s.method, "POST");
            assert!(s.path.starts_with("/ex/jira/demo-cloud/"), "{}", s.path);
            assert_eq!(s.path, "/ex/jira/demo-cloud/rest/api/3/search/jql");
            // base64("demo@example.test:tok")
            assert_eq!(
                s.auth.as_deref(),
                Some("Basic ZGVtb0BleGFtcGxlLnRlc3Q6dG9r")
            );
        }
        let first: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(first["jql"], "project = DEMO");
        assert_eq!(
            first["fields"],
            serde_json::json!(["summary", "status", "priority"])
        );
        assert_eq!(first["maxResults"], 100);
        assert!(first.get("nextPageToken").is_none());
        assert!(
            seen[1].body.contains(r#""nextPageToken":"p2""#),
            "{}",
            seen[1].body
        );
    }

    #[test]
    fn unauthorised_is_an_error_not_empty() {
        let (base, _rx) = fake(vec![(401, r#"{"errorMessages":[]}"#.into())]);
        let e = api(&base).tickets("tok", "project = DEMO").unwrap_err();
        assert_eq!(
            e,
            "Jira token rejected (401): check the token and its scopes"
        );
    }

    #[test]
    fn active_sprint_asks_the_board_then_lists_its_issues() {
        let body = r#"{"values":[{"id":42,"name":"Sprint 42",
            "startDate":"2026-11-17T09:00:00.000Z","endDate":"2026-11-28T17:00:00.000Z"}]}"#;
        let page1 = r#"{"startAt":0,"maxResults":100,"total":3,"issues":[{"key":"DEMO-1"},{"key":"DEMO-2"}]}"#;
        let page2 = r#"{"startAt":2,"maxResults":100,"total":3,"issues":[{"key":"DEMO-9"}]}"#;
        let (base, rx) = fake(vec![
            (200, body.into()),
            (200, page1.into()),
            (200, page2.into()),
        ]);
        let s = api(&base).active_sprint("tok").unwrap().unwrap();
        assert_eq!((s.id, s.name.as_str()), (42, "Sprint 42"));
        assert_eq!(
            s.keys.iter().map(String::as_str).collect::<Vec<_>>(),
            ["DEMO-1", "DEMO-2", "DEMO-9"]
        );
        let paths: Vec<_> = rx.try_iter().map(|s| (s.method, s.path)).collect();
        let get = |p: &str| {
            (
                "GET".to_owned(),
                format!("/ex/jira/demo-cloud/rest/agile/1.0/{p}"),
            )
        };
        assert_eq!(
            paths,
            [
                get("board/7/sprint?state=active"),
                get("sprint/42/issue?fields=summary&maxResults=100&startAt=0"),
                get("sprint/42/issue?fields=summary&maxResults=100&startAt=2"),
            ]
        );
    }

    #[test]
    fn no_active_sprint_lists_no_issues() {
        let (base, rx) = fake(vec![(200, r#"{"values":[]}"#.into())]);
        assert_eq!(api(&base).active_sprint("tok").unwrap(), None);
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn cloud_id_is_looked_up_once() {
        let (base, rx) = fake(vec![(200, r#"{"cloudId":"found-cloud"}"#.into())]);
        let a = JiraApi::new("demo.atlassian.net", "demo@example.test", "7", "")
            .with_bases(base.clone(), base);
        assert_eq!(a.cloud_id().unwrap(), "found-cloud");
        // The fake answers only once, so a second lookup would fail.
        assert_eq!(a.cloud_id().unwrap(), "found-cloud");
        let seen = rx.recv().unwrap();
        assert_eq!(seen.path, "/_edge/tenant_info");
        assert_eq!(seen.auth, None);
    }

    #[test]
    fn unknown_site_says_to_check_jira_site() {
        let (base, _rx) = fake(vec![(
            404,
            "<!DOCTYPE html>\n<html>Not found</html>".into(),
        )]);
        let a = JiraApi::new("demo.atlassian.net", "demo@example.test", "7", "")
            .with_bases(base.clone(), base.clone());
        let host = base.trim_start_matches("https://").to_owned();
        assert_eq!(
            a.cloud_id().unwrap_err(),
            format!("Jira site not found: check jira_site ({host})")
        );
    }

    #[test]
    fn classify_drops_html_bodies() {
        assert_eq!(
            classify(404, "<!DOCTYPE html>\n<html><body>Oops</body></html>"),
            "Jira answered 404"
        );
        assert_eq!(classify(500, ""), "Jira answered 500");
        assert_eq!(
            classify(500, "\n  \nreal reason"),
            "Jira answered 500: real reason"
        );
    }

    #[test]
    fn configured_cloud_id_needs_no_lookup() {
        let a = api("http://127.0.0.1:9");
        assert_eq!(a.cloud_id().unwrap(), "demo-cloud");
    }

    #[tokio::test]
    async fn run_blocking_passes_the_result_through() {
        assert_eq!(run_blocking(|| Ok(3)).await, Ok(3));
        assert_eq!(
            run_blocking(|| Err::<(), _>("no".to_owned())).await,
            Err("no".to_owned())
        );
    }
}
