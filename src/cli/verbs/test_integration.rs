use super::{Cli, Command, execute};
use clap::Parser;

#[tokio::test]
async fn parses_and_executes_status_workflow() {
    let temp_dir = tempfile::tempdir().expect("create temporary config directory");
    let config_path = temp_dir.path().join("fleet.toml");
    std::fs::write(
        &config_path,
        r#"
[fleet]
name = "integration-test-fleet"
"#,
    )
    .expect("write test fleet config");

    let cli = Cli::try_parse_from([
        "forgefleet",
        "--config",
        config_path.to_str().expect("UTF-8 config path"),
        "status",
    ])
    .expect("status command should parse");

    assert!(matches!(cli.command, Some(Command::Status)));
    execute(cli)
        .await
        .expect("parsed status command should execute");
}
