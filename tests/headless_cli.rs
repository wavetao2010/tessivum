use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use uuid::Uuid;

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    workspace().join("fixtures/headless/recorded-replay.jsonl")
}

fn data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("tessivum-headless-cli-{}", Uuid::new_v4()))
}

fn run(data_dir: &Path, resume: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tessivum"));
    command
        .current_dir(workspace())
        .arg("--session")
        .arg("cli-smoke")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--replay")
        .arg(fixture())
        .arg("--trusted-bash");
    if resume {
        command.arg("--resume");
    }
    command
        .arg("prove the CLI tool round trip")
        .output()
        .expect("tessivum binary should launch")
}

fn raw_log(data_dir: &Path) -> PathBuf {
    let logs = fs::read_dir(data_dir)
        .expect("headless run should create a data directory")
        .map(|entry| {
            entry
                .expect("data directory entry should be readable")
                .path()
        })
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1, "one raw JSONL log should be persisted");
    logs.into_iter().next().unwrap()
}

fn events(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .expect("raw JSONL log should be readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every persisted line should be JSON"))
        .filter(|record: &Value| record.get("seq").is_some())
        .collect()
}

fn assert_balanced(events: &[Value]) {
    let event_types = |expected| {
        events
            .iter()
            .filter(|event| event.get("type").and_then(Value::as_str) == Some(expected))
            .count()
    };
    let sequences = events
        .iter()
        .map(|event| {
            event["seq"]
                .as_u64()
                .expect("event seq should be an integer")
        })
        .collect::<Vec<_>>();

    assert_eq!(sequences, (0..sequences.len() as u64).collect::<Vec<_>>());
    assert_eq!(event_types("turn/start"), event_types("turn/end"));
    assert_eq!(event_types("tool/call"), event_types("tool/result"));
    assert!(
        event_types("tool/call") > 0,
        "replay should invoke trusted bash"
    );
}

#[test]
fn recorded_headless_cli_runs_and_resumes_a_trusted_bash_turn() {
    let data_dir = data_dir();
    let first = run(&data_dir, false);
    assert!(
        first.status.success(),
        "first run failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        String::from_utf8(first.stdout).unwrap(),
        "CLI tool round trip complete: CLI_TOOL_ROUND_TRIP\n"
    );

    let log = raw_log(&data_dir);
    let first_events = events(&log);
    assert_balanced(&first_events);

    let resumed = run(&data_dir, true);
    assert!(
        resumed.status.success(),
        "resume run failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        String::from_utf8(resumed.stdout).unwrap(),
        "CLI tool round trip complete: CLI_TOOL_ROUND_TRIP\n"
    );

    let resumed_events = events(&log);
    assert!(resumed_events.len() > first_events.len());
    assert_balanced(&resumed_events);
    fs::remove_dir_all(data_dir).expect("temporary headless data should be removable");
}

#[test]
fn recorded_replay_and_task_are_required_before_services_start() {
    let data_dir = data_dir();
    let missing_replay = Command::new(env!("CARGO_BIN_EXE_tessivum"))
        .current_dir(workspace())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("a task")
        .output()
        .expect("tessivum binary should launch");
    assert_eq!(missing_replay.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&missing_replay.stderr).starts_with("USAGE: "),
        "missing replay should be a stable usage diagnostic"
    );

    let missing_task = Command::new(env!("CARGO_BIN_EXE_tessivum"))
        .current_dir(workspace())
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--replay")
        .arg(fixture())
        .output()
        .expect("tessivum binary should launch");
    assert_eq!(missing_task.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&missing_task.stderr).starts_with("USAGE: "),
        "missing task should be a stable usage diagnostic"
    );
    assert!(!data_dir.exists(), "usage failures must not start services");
}
