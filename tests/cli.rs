//! End-to-end tests that run the real mempig binary.

use std::process::Command;

fn mempig(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_mempig"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run mempig")
}

#[test]
fn default_view_shows_memory_and_pigs() {
    let out = mempig(&["-n", "5"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("mempig"), "{text}");
    assert!(text.contains("Memory"), "{text}");
    assert!(text.contains("The pigs"), "{text}");
    assert!(
        !text.contains("\x1b["),
        "NO_COLOR should disable ANSI codes"
    );
}

#[test]
fn grouped_and_user_views_run() {
    for args in [&["-g"][..], &["-u", "-n", "3"], &["--no-color", "-g", "-u"]] {
        let out = mempig(args);
        assert!(out.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("Memory"));
    }
}

#[test]
fn filter_finds_nothing_gracefully() {
    let out = mempig(&["-f", "no-such-process-zzz"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("No processes matched."));
}

#[test]
fn kill_unknown_name_fails() {
    let out = mempig(&["kill", "no-such-process-zzz", "-y"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no process matching"));
}

#[test]
fn kill_terminates_own_child() {
    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = child.id().to_string();
    let out = mempig(&["kill", &pid, "-y"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status = child.wait().unwrap();
    assert!(!status.success(), "sleep should have been terminated");
}

#[test]
fn version_flag() {
    let out = mempig(&["--version"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("mempig "));
}
