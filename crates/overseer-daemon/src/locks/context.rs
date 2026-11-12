//! What a target placeholder needs from the machine: the repo and branch of
//! a folder, the current ArgoCD and kube contexts, the Terraform workspace.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::env::BaseEnv;

/// Lookups for the placeholders that depend on the machine, not the command.
/// `None` means unknown, which drops the hit that needed it.
pub trait Context {
    /// The repo's folder name for a folder inside it (a worktree gives the
    /// main repo's name).
    fn repo(&self, dir: &Path) -> Option<String>;
    /// The checked-out branch; `None` when detached.
    fn branch(&self, dir: &Path) -> Option<String>;
    fn argo_ctx(&self) -> Option<String>;
    /// `flag` is the command's `--context` value, which wins when given.
    fn kube_ctx(&self, flag: Option<&str>) -> Option<String>;
    fn tf_workspace(&self, dir: &Path) -> Option<String>;
}

/// The real lookups. Commands run with `base` plus `env` (the hook's
/// variables, which win), and each is killed at `deadline`. Blocking: call
/// it off the async runtime.
pub struct SystemContext {
    pub env: Vec<(String, String)>,
    pub deadline: Instant,
    pub base: BaseEnv,
}

impl SystemContext {
    /// A variable from the hook's environment, else the base environment.
    fn var(&self, name: &str) -> Option<String> {
        if let Some((_, v)) = self.env.iter().find(|(k, _)| k == name) {
            return Some(v.clone()).filter(|v| !v.is_empty());
        }
        let base = self.base.get();
        base.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .filter(|v| !v.is_empty())
    }

    /// The first line of a command's stdout, or `None` when it fails, prints
    /// nothing or is still running at the deadline.
    fn run(&self, prog: &str, args: &[&OsStr]) -> Option<String> {
        if Instant::now() >= self.deadline {
            return None;
        }
        let base = self.base.get();
        let mut c = Command::new(prog);
        c.args(args)
            .current_dir("/")
            .env_clear()
            .envs(base.iter().map(|(k, v)| (k, v)))
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = c.spawn().ok()?;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < self.deadline => {
                    std::thread::sleep(POLL);
                }
                _ => {
                    // The child leads its own group; kill it and anything it started.
                    unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
                    let _ = child.wait();
                    return None;
                }
            }
        };
        if !status.success() {
            return None;
        }
        let mut out = String::new();
        child.stdout.take()?.read_to_string(&mut out).ok()?;
        let line = out.lines().next()?.trim();
        (!line.is_empty()).then(|| line.to_owned())
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Option<String> {
        let mut all: Vec<&OsStr> = vec![OsStr::new("-C"), dir.as_os_str()];
        all.extend(args.iter().map(OsStr::new));
        self.run("git", &all)
    }
}

/// How often a running lookup is checked against the deadline.
const POLL: Duration = Duration::from_millis(2);

impl Context for SystemContext {
    fn repo(&self, dir: &Path) -> Option<String> {
        let common = PathBuf::from(self.git(dir, &["rev-parse", "--git-common-dir"])?);
        let common = normalize(&dir.join(common));
        // `<repo>/.git` normally; a bare repo's common dir is the repo itself.
        let repo = if common.file_name()? == ".git" {
            common.parent()?.to_path_buf()
        } else {
            common
        };
        let name = repo.file_name()?.to_string_lossy();
        let name = name.strip_suffix(".git").unwrap_or(&name);
        (!name.is_empty()).then(|| name.to_owned())
    }

    fn branch(&self, dir: &Path) -> Option<String> {
        self.git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|b| b != "HEAD")
    }

    fn argo_ctx(&self) -> Option<String> {
        if let Some(server) = self.var("ARGOCD_SERVER") {
            return Some(server);
        }
        let path = match self.var("ARGOCD_CONFIG") {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(self.var("HOME")?).join(".config/argocd/config"),
        };
        let text = std::fs::read_to_string(path).ok()?;
        text.lines().find_map(|l| {
            let v = l.strip_prefix("current-context:")?.trim();
            let v = v.trim_matches(|c| c == '"' || c == '\'');
            (!v.is_empty()).then(|| v.to_owned())
        })
    }

    fn kube_ctx(&self, flag: Option<&str>) -> Option<String> {
        if let Some(f) = flag.filter(|f| !f.is_empty()) {
            return Some(f.to_owned());
        }
        self.run(
            "kubectl",
            &[OsStr::new("config"), OsStr::new("current-context")],
        )
    }

    fn tf_workspace(&self, dir: &Path) -> Option<String> {
        if let Some(w) = self.var("TF_WORKSPACE") {
            return Some(w);
        }
        let file = std::fs::read_to_string(dir.join(".terraform/environment")).ok();
        let w = file.as_deref().map(str::trim).filter(|w| !w.is_empty());
        Some(w.unwrap_or("default").to_owned())
    }
}

/// `..` and `.` removed without touching the filesystem.
pub(crate) fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Dir {
            let p = std::env::temp_dir().join(format!("ovs-ctx-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            // Canonical, so /var and /private/var do not differ.
            Dir(p.canonicalize().unwrap())
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(home: &Path, dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn ctx(home: &Path, path: &str, env: &[(&str, &str)], within: Duration) -> SystemContext {
        SystemContext {
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            deadline: Instant::now() + within,
            base: BaseEnv::ready(vec![
                ("PATH".into(), path.into()),
                ("HOME".into(), home.into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ]),
        }
    }

    fn path() -> String {
        std::env::var("PATH").unwrap()
    }

    #[test]
    fn repo_and_branch_from_a_repo_and_its_worktree() {
        let d = Dir::new();
        let main = d.0.join("demo-api");
        std::fs::create_dir_all(main.join("src")).unwrap();
        git(&d.0, &main, &["init", "-q", "-b", "main"]);
        git(
            &d.0,
            &main,
            &[
                "-c",
                "user.name=Demo",
                "-c",
                "user.email=demo@example.test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        let wt = d.0.join("demo-api-wt");
        git(
            &d.0,
            &main,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "feature-x",
            ],
        );

        let c = ctx(&d.0, &path(), &[], Duration::from_secs(10));
        assert_eq!(c.repo(&main).as_deref(), Some("demo-api"));
        assert_eq!(c.repo(&main.join("src")).as_deref(), Some("demo-api"));
        assert_eq!(c.branch(&main).as_deref(), Some("main"));
        assert_eq!(c.repo(&wt).as_deref(), Some("demo-api"));
        assert_eq!(c.branch(&wt).as_deref(), Some("feature-x"));
        assert_eq!(c.repo(&d.0.join("missing")), None);
        assert_eq!(c.repo(&d.0), None, "not a repo");
    }

    #[test]
    fn a_passed_deadline_runs_nothing() {
        let d = Dir::new();
        git(&d.0, &d.0, &["init", "-q"]);
        let c = ctx(&d.0, &path(), &[], Duration::ZERO);
        assert_eq!(c.repo(&d.0), None);
    }

    #[test]
    fn a_slow_lookup_is_killed_at_the_deadline() {
        let d = Dir::new();
        let bin = d.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("kubectl");
        std::fs::write(&stub, "#!/bin/sh\nsleep 5\necho late\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = format!("{}:/bin:/usr/bin", bin.display());
        let c = ctx(&d.0, &p, &[], Duration::from_millis(200));
        let start = Instant::now();
        assert_eq!(c.kube_ctx(None), None);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn kube_context_from_kubectl_unless_the_flag_is_given() {
        let d = Dir::new();
        let bin = d.0.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stub = bin.join("kubectl");
        // Echoes KUBECONFIG so the test sees the hook's variable reach it.
        std::fs::write(&stub, "#!/bin/sh\necho \"staging-eu$KUBECONFIG\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let p = format!("{}:/bin:/usr/bin", bin.display());
        let c = ctx(&d.0, &p, &[], Duration::from_secs(5));
        assert_eq!(c.kube_ctx(None).as_deref(), Some("staging-eu"));
        assert_eq!(c.kube_ctx(Some("uat-eu")).as_deref(), Some("uat-eu"));
        let c = ctx(&d.0, &p, &[("KUBECONFIG", "-two")], Duration::from_secs(5));
        assert_eq!(c.kube_ctx(None).as_deref(), Some("staging-eu-two"));
    }

    #[test]
    fn argo_context_from_server_then_config() {
        let d = Dir::new();
        let cfg = d.0.join(".config/argocd");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("config"),
            "contexts:\n- name: other\n  server: other\ncurrent-context: uat\nservers: []\n",
        )
        .unwrap();
        let c = ctx(&d.0, &path(), &[], Duration::from_secs(5));
        assert_eq!(c.argo_ctx().as_deref(), Some("uat"));

        let alt = d.0.join("alt-config");
        std::fs::write(&alt, "current-context: \"uat-two\"\n").unwrap();
        let c = ctx(
            &d.0,
            &path(),
            &[("ARGOCD_CONFIG", alt.to_str().unwrap())],
            Duration::from_secs(5),
        );
        assert_eq!(c.argo_ctx().as_deref(), Some("uat-two"));

        let c = ctx(
            &d.0,
            &path(),
            &[("ARGOCD_SERVER", "argocd.example.test")],
            Duration::from_secs(5),
        );
        assert_eq!(c.argo_ctx().as_deref(), Some("argocd.example.test"));

        let empty = Dir::new();
        let c = ctx(&empty.0, &path(), &[], Duration::from_secs(5));
        assert_eq!(c.argo_ctx(), None);
    }

    #[test]
    fn tf_workspace_from_env_then_file_then_default() {
        let d = Dir::new();
        let c = ctx(&d.0, &path(), &[], Duration::from_secs(5));
        assert_eq!(c.tf_workspace(&d.0).as_deref(), Some("default"));
        std::fs::create_dir_all(d.0.join(".terraform")).unwrap();
        std::fs::write(d.0.join(".terraform/environment"), "staging\n").unwrap();
        assert_eq!(c.tf_workspace(&d.0).as_deref(), Some("staging"));
        let c = ctx(
            &d.0,
            &path(),
            &[("TF_WORKSPACE", "uat")],
            Duration::from_secs(5),
        );
        assert_eq!(c.tf_workspace(&d.0).as_deref(), Some("uat"));
    }
}
