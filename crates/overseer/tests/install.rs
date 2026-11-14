use std::process::Command;

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
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&ver.stdout);
    assert!(text.contains("app     0.1.0 build test-7"), "{text}");
}
