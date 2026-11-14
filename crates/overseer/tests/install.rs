use std::process::Command;

/// Stands in for launchctl: every call fails, so nothing reaches launchd.
const NO_LAUNCHCTL: &str = "/usr/bin/false";

#[test]
fn bundle_writes_the_build_id_and_signs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let app = tmp.path().join("Overseer.app");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/bundle.sh");
    let bin = assert_cmd::cargo::cargo_bin("overseer");

    let out = Command::new("sh")
        .arg(script)
        .arg(&bin)
        .arg(&app)
        .env("HOME", &home)
        .env("OVERSEER_HOME", tmp.path().join("overseer"))
        .env("OVERSEER_BUILD", "test-7")
        .env("OVERSEER_LAUNCHCTL", NO_LAUNCHCTL)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).unwrap();
    let after = &plist[plist.find("OverseerBuild").unwrap()..];
    assert!(after.contains("<string>test-7</string>"), "{plist}");

    let verify = Command::new("codesign")
        .arg("--verify")
        .arg(&app)
        .output()
        .unwrap();
    assert!(verify.status.success());

    let ver = Command::new(app.join("Contents/MacOS/overseer"))
        .args(["ctl", "version"])
        .env("HOME", &home)
        .env("OVERSEER_HOME", tmp.path().join("overseer"))
        .env("OVERSEER_LAUNCHCTL", NO_LAUNCHCTL)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&ver.stdout);
    assert!(text.contains("app     0.1.0 build test-7"), "{text}");
}

/// A temp rig for the install scripts: `HOME`, `OVERSEER_HOME` and the app
/// all live under one temp dir, launchctl is a logging stub and `PATH` has
/// only the stub folder and the system folders, so nothing reaches the real
/// app, LaunchAgents folder or launchd, and nothing is downloaded.
struct Rig {
    tmp: tempfile::TempDir,
}

impl Rig {
    fn new() -> Rig {
        let rig = Rig {
            tmp: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(rig.home()).unwrap();
        std::fs::create_dir_all(rig.stubs()).unwrap();
        // Like the cli.rs stub: logs its arguments, fails print and
        // kickstart, succeeds for the rest.
        rig.write_exe(
            &rig.launchctl(),
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1\" in print|kickstart) exit 1;; esac\nexit 0\n",
                rig.path("launchctl.log").display()
            ),
        );
        rig
    }

    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.tmp.path().join(rel)
    }

    fn home(&self) -> std::path::PathBuf {
        self.path("home")
    }

    fn state(&self) -> std::path::PathBuf {
        self.path("state")
    }

    fn stubs(&self) -> std::path::PathBuf {
        self.path("stubs")
    }

    fn launchctl(&self) -> std::path::PathBuf {
        self.path("launchctl-stub.sh")
    }

    fn app(&self) -> std::path::PathBuf {
        self.home().join("Applications/Overseer.app")
    }

    fn app_bin(&self) -> std::path::PathBuf {
        self.app().join("Contents/MacOS/overseer")
    }

    fn link(&self) -> std::path::PathBuf {
        self.home().join(".local/bin/overseer")
    }

    fn launchctl_log(&self) -> String {
        std::fs::read_to_string(self.path("launchctl.log")).unwrap_or_default()
    }

    fn write_exe(&self, path: &std::path::Path, text: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn plists(&self) -> Vec<std::path::PathBuf> {
        match std::fs::read_dir(self.state().join("agent")) {
            Ok(dir) => dir
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x == "plist"))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// `sh scripts/<name>` with a cleared environment holding only the
    /// rig's variables and stdin from /dev/null.
    fn script(&self, name: &str) -> Command {
        let script = format!("{}/../../scripts/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut c = Command::new("sh");
        c.arg(script)
            .env_clear()
            .env("HOME", self.home())
            .env("SHELL", "/bin/sh")
            .env("OVERSEER_HOME", self.state())
            .env("OVERSEER_LAUNCHCTL", self.launchctl())
            .env("OVERSEER_BUILD", "test-1")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.stubs().display()),
            )
            .stdin(std::process::Stdio::null());
        c
    }

    fn install(&self) -> String {
        let out = self
            .script("install.sh")
            .env(
                "OVERSEER_INSTALL_BINARY",
                assert_cmd::cargo::cargo_bin("overseer"),
            )
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

/// Every launchctl call names this rig's label, never the bare real one.
fn assert_no_real_label(log: &str) {
    for line in log.lines().filter(|l| l.contains("uk.co.techize.overseer")) {
        assert!(line.contains("uk.co.techize.overseer-home-"), "{line}");
    }
}

#[test]
fn install_into_a_temp_home() {
    let rig = Rig::new();
    let out = rig.install();

    assert!(rig.app_bin().is_file(), "{out}");
    let verify = Command::new("codesign")
        .arg("--verify")
        .arg(rig.app())
        .output()
        .unwrap();
    assert!(verify.status.success(), "{out}");
    let plist = std::fs::read_to_string(rig.app().join("Contents/Info.plist")).unwrap();
    assert!(plist.contains("<string>test-1</string>"), "{plist}");

    let link = std::fs::symlink_metadata(rig.link()).unwrap();
    assert!(link.file_type().is_symlink());
    assert_eq!(std::fs::read_link(rig.link()).unwrap(), rig.app_bin());

    let plists = rig.plists();
    assert_eq!(plists.len(), 1, "{out}");
    let name = plists[0].file_name().unwrap().to_str().unwrap();
    let label = name.strip_suffix(".plist").unwrap();
    assert!(label.starts_with("uk.co.techize.overseer-home-"), "{name}");
    assert!(!rig.home().join("Library/LaunchAgents").exists());
    assert_no_real_label(&rig.launchctl_log());

    let summary = format!(
        "Overseer is installed.\n  App:    {app}\n  CLI:    {link} (add ~/.local/bin to PATH if it is not there)\n  Daemon: LaunchAgent {label}, logs in {logs}\nOpen Overseer from ~/Applications in Finder, or run: open ~/Applications/Overseer.app\n",
        app = rig.app().display(),
        link = rig.link().display(),
        logs = rig.state().join("logs").display(),
    );
    assert!(out.ends_with(&summary), "{out}");
}

#[test]
fn install_passes_yes_and_no_restart_to_install_agent() {
    let rig = Rig::new();
    // A stand-in binary that records its arguments; bundle.sh signs it.
    let fake = rig.path("fake-overseer");
    rig.write_exe(
        &fake,
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\necho \"installed x as demo-label (runs y)\"\n",
            rig.path("args.log").display()
        ),
    );
    for (yes, no_restart) in [("1", ""), ("", "1"), ("", "")] {
        let out = rig
            .script("install.sh")
            .env("OVERSEER_INSTALL_BINARY", &fake)
            .env("YES", yes)
            .env("NO_RESTART", no_restart)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("Daemon: LaunchAgent demo-label,"), "{text}");
    }
    let args = std::fs::read_to_string(rig.path("args.log")).unwrap();
    assert_eq!(
        args,
        "install-agent --yes\ninstall-agent --no-restart\ninstall-agent\n"
    );
}

#[test]
fn install_stops_when_install_agent_fails() {
    let rig = Rig::new();
    let fake = rig.path("fake-overseer");
    rig.write_exe(&fake, "#!/bin/sh\necho 'cannot write plist' >&2\nexit 3\n");
    let out = rig
        .script("install.sh")
        .env("OVERSEER_INSTALL_BINARY", &fake)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("Overseer is installed."), "{text}");
}

/// The cargo stand-in rustup "installs": it logs its arguments and copies
/// the test binary to where a release build would leave it.
fn write_rust_stubs(rig: &Rig) {
    let cargo = rig.path("cargo-stub");
    rig.write_exe(
        &cargo,
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{log}'\nmkdir -p \"$CARGO_TARGET_DIR/release\"\ncp '{bin}' \"$CARGO_TARGET_DIR/release/overseer\"\n",
            log = rig.path("cargo.log").display(),
            bin = assert_cmd::cargo::cargo_bin("overseer").display(),
        ),
    );
    // curl prints the "rustup" script, which records how it was run and
    // puts cargo in $CARGO_HOME/bin.
    let rustup = format!(
        "echo \"$*\" > '{sh_log}'\nmkdir -p \"$CARGO_HOME/bin\"\ncp '{cargo}' \"$CARGO_HOME/bin/cargo\"\n",
        sh_log = rig.path("rustup.log").display(),
        cargo = cargo.display(),
    );
    rig.write_exe(
        &rig.stubs().join("curl"),
        &format!(
            "#!/bin/sh\necho \"$*\" > '{log}'\ncat <<'RUSTUP'\n{rustup}RUSTUP\n",
            log = rig.path("curl.log").display()
        ),
    );
}

fn rust_script(rig: &Rig) -> Command {
    let mut c = rig.script("install.sh");
    c.env("CARGO_HOME", rig.path("cargo"))
        .env("CARGO_TARGET_DIR", rig.path("target"))
        .env("OVERSEER_CURL", rig.stubs().join("curl"));
    c
}

#[test]
fn install_without_cargo_installs_rust_first() {
    let rig = Rig::new();
    write_rust_stubs(&rig);
    let out = rust_script(&rig)
        .env("OVERSEER_INSTALL_RUST", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains(
            "Rust is not installed. Installing it for your user with rustup (no admin rights needed).\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("Add ~/.cargo/bin to your PATH to run cargo yourself.\n"),
        "{text}"
    );
    let curl = std::fs::read_to_string(rig.path("curl.log")).unwrap();
    assert_eq!(curl, "--proto =https --tlsv1.2 -sSf https://sh.rustup.rs\n");
    let rustup = std::fs::read_to_string(rig.path("rustup.log")).unwrap();
    assert_eq!(
        rustup,
        "-y --no-modify-path --profile minimal --default-toolchain none\n"
    );
    let cargo = std::fs::read_to_string(rig.path("cargo.log")).unwrap();
    assert_eq!(cargo, "build --release -p overseer\n");
    assert!(rig.app_bin().is_file(), "{text}");
    assert_eq!(std::fs::read_link(rig.link()).unwrap(), rig.app_bin());
    assert_eq!(rig.plists().len(), 1, "{text}");
    assert_no_real_label(&rig.launchctl_log());
}

#[test]
fn install_without_cargo_or_a_terminal_stops_with_the_rustup_command() {
    let rig = Rig::new();
    write_rust_stubs(&rig);
    let out = rust_script(&rig).output().unwrap();
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none"),
        "{text}"
    );
    assert!(text.contains("OVERSEER_INSTALL_RUST=1"), "{text}");
    assert!(!rig.path("curl.log").exists(), "curl ran: {text}");
    assert!(!rig.app().exists());
    assert!(rig.launchctl_log().is_empty());
}

#[test]
fn install_uses_cargo_from_cargo_home_when_it_is_not_on_path() {
    let rig = Rig::new();
    write_rust_stubs(&rig);
    let bin = rig.path("cargo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(rig.path("cargo-stub"), bin.join("cargo")).unwrap();
    let out = rust_script(&rig).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(!text.contains("Rust is not installed"), "{text}");
    assert!(!rig.path("curl.log").exists(), "curl ran: {text}");
    let cargo = std::fs::read_to_string(rig.path("cargo.log")).unwrap();
    assert_eq!(cargo, "build --release -p overseer\n");
}

#[test]
fn uninstall_in_a_custom_home_never_touches_the_real_agent() {
    let rig = Rig::new();
    let out = rig.script("uninstall.sh").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains(&format!(
            "No app binary; remove {} by hand.",
            rig.state().join("agent").display()
        )),
        "{text}"
    );
    assert!(rig.launchctl_log().is_empty());

    rig.install();
    assert_eq!(rig.plists().len(), 1);
    let out = rig.script("uninstall.sh").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(rig.plists().is_empty(), "{text}");
    assert!(std::fs::symlink_metadata(rig.link()).is_err(), "{text}");
    assert!(!rig.app().exists(), "{text}");
    assert!(rig.state().exists(), "the state folder went: {text}");
    assert_no_real_label(&rig.launchctl_log());
}

#[test]
fn uninstall_without_a_custom_home_boots_out_through_the_stub() {
    let rig = Rig::new();
    let plist = rig
        .home()
        .join("Library/LaunchAgents/uk.co.techize.overseer.plist");
    std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::fs::write(&plist, "<plist/>").unwrap();
    let out = rig
        .script("uninstall.sh")
        .env_remove("OVERSEER_HOME")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    assert_eq!(
        rig.launchctl_log(),
        format!("bootout gui/{}/uk.co.techize.overseer\n", uid.trim())
    );
    assert!(!plist.exists(), "{text}");
}
