use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

#[test]
fn the_smoke_test_refuses_to_run_without_all_four_addresses() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("smoke-test-arguments");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let called = dir.join("called");
    for tool in ["curl", "python3", "psql"] {
        let stub = dir.join(tool);
        fs::write(
            &stub,
            format!("#!/bin/sh\necho {tool} >> '{}'\nexit 1\n", called.display()),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("smoke_test.sh");
    let out = Command::new("bash")
        .arg(&script)
        .args(["http://127.0.0.1:1", "key", "db"])
        .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
        .env_remove("PGHOST")
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Usage: "),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !called.exists(),
        "reached for {}",
        fs::read_to_string(&called).unwrap_or_default()
    );
}
