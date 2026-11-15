use std::process::Command;

#[cfg(unix)]
use std::process::Stdio;

#[test]
fn primary_and_compatibility_commands_keep_version_and_help() {
    for (binary, name) in [
        (env!("CARGO_BIN_EXE_tyx"), "tyx"),
        (env!("CARGO_BIN_EXE_ty-extended"), "ty-extended"),
    ] {
        let version = Command::new(binary).arg("--version").output().unwrap();
        assert!(version.status.success());
        assert_eq!(
            String::from_utf8(version.stdout).unwrap().trim(),
            format!("{name} {}", env!("CARGO_PKG_VERSION"))
        );
        let help = Command::new(binary).arg("--help").output().unwrap();
        assert!(help.status.success());
        assert!(
            String::from_utf8(help.stdout)
                .unwrap()
                .contains("Usage: tyx check")
        );
        let invalid = Command::new(binary)
            .arg("invalid-command")
            .output()
            .unwrap();
        assert_eq!(invalid.status.code(), Some(2));
    }
}

#[test]
fn compatibility_launcher_requires_its_sibling_instead_of_searching_path() {
    let root = tempfile::tempdir().unwrap();
    let launcher = root.path().join(if cfg!(windows) {
        "ty-extended.exe"
    } else {
        "ty-extended"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_ty-extended"), &launcher).unwrap();
    let output = Command::new(launcher).arg("check").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("could not start adjacent tyx")
    );
}

#[cfg(unix)]
#[test]
fn compatibility_launcher_preserves_pid_arguments_stdio_and_exit_status() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let launcher = root.path().join("ty-extended");
    std::fs::copy(env!("CARGO_BIN_EXE_ty-extended"), &launcher).unwrap();
    let primary = root.path().join("tyx");
    std::fs::write(&primary, "#!/bin/sh\nprintf '%s\\n' \"$$\" \"$1\" \"$2\"\nprintf 'forwarded stderr\\n' >&2\n/bin/cat\nexit 37\n").unwrap();
    std::fs::set_permissions(primary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(launcher)
        .args(["path with spaces", "café 雪"])
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"forwarded stdin\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(37));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{pid}\npath with spaces\ncafé 雪\nforwarded stdin\n")
    );
    assert_eq!(output.stderr, b"forwarded stderr\n");
}
