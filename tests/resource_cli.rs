use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_shiyue-cli"))
        .args(args)
        .env("SHIYUE_TEST_ROOT", root)
        .output()
        .unwrap()
}

#[test]
fn json_envelopes_keep_stdout_clean_and_use_documented_exit_codes() {
    let root = std::env::temp_dir().join(format!(
        "shiyue-resource-cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();

    let output = run(
        &root,
        &[
            "resource",
            "add",
            "https://koboyo.com/icons?q=app+icon",
            "--note",
            "App icon 资源",
            "--json",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schema_version"], 1);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["id"], "1");
    assert_eq!(envelope["data"]["status"], "pending_review");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("{\"data\""));

    let pending = run(&root, &["resource", "pending", "--json"]);
    let envelope: serde_json::Value = serde_json::from_slice(&pending.stdout).unwrap();
    assert_eq!(envelope["data"].as_array().unwrap().len(), 1);

    let missing = run(&root, &["resource", "get", "999", "--json"]);
    assert_eq!(missing.status.code(), Some(3));
    let envelope: serde_json::Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(envelope["error"]["code"], "RESOURCE_NOT_FOUND");

    std::fs::remove_dir_all(root).unwrap();
}
