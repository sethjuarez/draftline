use std::process::Command;

use serde_json::Value;

fn draftline(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_draftline"))
        .args(args)
        .output()
        .unwrap()
}

fn stderr_json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stderr).unwrap()
}

#[test]
fn capabilities_outputs_json() {
    let output = draftline(&["capabilities", "--json"]);

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["inspect"], true);
}

#[test]
fn usage_errors_are_invalid_arguments_json() {
    for args in [
        &["capabilities"][..],
        &["capabilities", "--json", "extra"][..],
        &["explain-error", "--json"][..],
        &["explain-error", "--json", "dirty_workspace", "extra"][..],
        &["not-a-command", "--json"][..],
    ] {
        let output = draftline(args);

        assert!(!output.status.success());
        assert_eq!(stderr_json(&output)["code"], "invalid_arguments");
    }
}

#[test]
fn explain_error_requires_known_code() {
    let output = draftline(&["explain-error", "--json", "dirty_workspace"]);

    assert!(output.status.success());
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["code"], "dirty_workspace");

    let output = draftline(&["explain-error", "--json", "not_a_code"]);

    assert!(!output.status.success());
    assert_eq!(stderr_json(&output)["code"], "invalid_arguments");
}
