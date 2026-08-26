#[path = "../src/cli.rs"]
mod cli;

use std::{
    env,
    ffi::OsString,
    fs,
    num::NonZeroU64,
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

use clap::error::ErrorKind;
use cli::{parse_cli, resolve_data_root, CliCommand, ExitClass, HeadlessCommand, PluginAction};

fn headless(args: &[&str]) -> HeadlessCommand {
    let cli = parse_cli(args.iter().copied()).expect("headless invocation should parse");
    match cli.command {
        CliCommand::Headless(command) => command,
        command => panic!("expected Headless, got {command:?}"),
    }
}
static PROCESS_STATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct ProcessState {
    cwd: PathBuf,
    home: Option<OsString>,
    tessivum_home: Option<OsString>,
}

impl ProcessState {
    fn capture() -> Self {
        Self {
            cwd: env::current_dir().expect("current directory is available"),
            home: env::var_os("HOME"),
            tessivum_home: env::var_os("TESSIVUM_HOME"),
        }
    }
}

impl Drop for ProcessState {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.cwd);
        restore_variable("HOME", self.home.take());
        restore_variable("TESSIVUM_HOME", self.tessivum_home.take());
    }
}

fn restore_variable(name: &str, value: Option<OsString>) {
    if let Some(value) = value {
        env::set_var(name, value);
    } else {
        env::remove_var(name);
    }
}

fn with_process_state(test: impl FnOnce()) {
    let _lock = PROCESS_STATE
        .lock()
        .expect("process state lock is available");
    let _state = ProcessState::capture();
    test();
}

fn temp_dir(name: &str) -> PathBuf {
    let root = env::temp_dir().join(format!("tessivum-cli-{name}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("temporary directory creates");
    root
}

#[test]
fn upstream_headless_example_keeps_launcher_data_separate_from_task() {
    let command = headless(&[
        "tessivum",
        "--profile",
        "headless",
        "--patch",
        "base.toml",
        "--data-dir",
        "state",
        "--session",
        "session-7",
        "--resume",
        "--trusted-bash",
        "--replay",
        "recording.jsonl",
        "--provider",
        "recorded",
        "--model",
        "fixture-model",
        "--max-tokens",
        "64",
        "prove",
        "the",
        "round",
        "trip",
    ]);

    assert_eq!(command.patches, vec![PathBuf::from("base.toml")]);
    assert_eq!(command.data_dir, Some(PathBuf::from("state")));
    assert_eq!(command.session.as_deref(), Some("session-7"));
    assert!(command.resume);
    assert!(command.trusted_bash);
    assert_eq!(command.replay, Some(PathBuf::from("recording.jsonl")));
    assert_eq!(command.provider, "recorded");
    assert_eq!(command.model, "fixture-model");
    assert_eq!(command.max_tokens, Some(NonZeroU64::new(64).unwrap()));
    assert_eq!(command.task, "prove the round trip");
}

#[test]
fn repeated_patches_keep_order_and_defaults_select_recorded() {
    let command = headless(&[
        "tessivum",
        "--patch",
        "base.toml",
        "--patch=local.toml",
        "--replay",
        "recording.jsonl",
        "ship",
        "this",
    ]);

    assert_eq!(
        command.patches,
        vec![PathBuf::from("base.toml"), PathBuf::from("local.toml")]
    );
    assert_eq!(command.provider, "recorded");
    assert_eq!(command.model, "recorded");
    assert_eq!(command.task, "ship this");
}

#[test]
fn live_provider_requires_an_explicit_model() {
    let error = parse_cli(["tessivum", "--provider", "openai-responses", "task"])
        .expect_err("live routes need a wire model");
    assert_eq!(error.exit_code(), 2);
    assert!(error.to_string().contains("--model"));

    let command = headless(&[
        "tessivum",
        "--provider",
        "openai-responses",
        "--model",
        "relay-codex",
        "task",
    ]);
    assert!(command.replay.is_none());
    assert_eq!(command.provider, "openai-responses");
    assert_eq!(command.model, "relay-codex");
}

#[test]
fn direct_web_and_profile_web_are_the_same_future_command() {
    for args in [
        ["tessivum", "web"].as_slice(),
        ["tessivum", "--profile", "web"].as_slice(),
    ] {
        match parse_cli(args.iter().copied()).unwrap().command {
            CliCommand::Web(command) => {
                assert!(command.patches.is_empty());
                assert!(command.data_dir.is_none());
            }
            command => panic!("expected Web, got {command:?}"),
        }
    }
}

#[test]
fn web_accepts_one_data_directory_option_in_every_supported_position() {
    for args in [
        ["tessivum", "--data-dir", "state", "web"].as_slice(),
        ["tessivum", "web", "--data-dir", "state"].as_slice(),
        ["tessivum", "--profile", "web", "--data-dir", "state"].as_slice(),
    ] {
        let CliCommand::Web(command) = parse_cli(args.iter().copied()).unwrap().command else {
            panic!("expected Web")
        };
        assert_eq!(command.data_dir, Some(PathBuf::from("state")));
    }
}

#[test]
fn web_patch_overlays_keep_argv_order_for_both_web_spellings() {
    for args in [
        [
            "tessivum",
            "web",
            "--patch",
            "base.yml",
            "--patch=local.yml",
        ]
        .as_slice(),
        [
            "tessivum",
            "--profile",
            "web",
            "--patch",
            "base.yml",
            "--patch=local.yml",
        ]
        .as_slice(),
    ] {
        match parse_cli(args.iter().copied()).unwrap().command {
            CliCommand::Web(command) => assert_eq!(
                command.patches,
                vec![PathBuf::from("base.yml"), PathBuf::from("local.yml")]
            ),
            command => panic!("expected Web, got {command:?}"),
        }
    }
}

#[test]
fn plugin_commands_own_their_profile_and_mutation() {
    let add = parse_cli([
        "tessivum",
        "plugin",
        "--data-dir",
        "state",
        "add",
        "@scope/plugin@1.2.3",
    ])
    .unwrap();
    let CliCommand::Plugin(add) = add.command else {
        panic!("expected plugin command")
    };
    assert_eq!(add.data_dir, Some(PathBuf::from("state")));
    assert_eq!(add.action, PluginAction::Add("@scope/plugin@1.2.3".into()));

    let remove = parse_cli(["tessivum", "plugin", "remove", "@scope/plugin"]).unwrap();
    let CliCommand::Plugin(remove) = remove.command else {
        panic!("expected plugin command")
    };
    assert_eq!(remove.data_dir, None);
    assert_eq!(remove.action, PluginAction::Remove("@scope/plugin".into()));
}

#[test]
fn sdk_accepts_a_data_directory() {
    let CliCommand::Sdk(command) = parse_cli(["tessivum", "sdk", "--data-dir", "state"])
        .expect("SDK invocation should parse")
        .command
    else {
        panic!("expected SDK command");
    };
    assert_eq!(command.data_dir, Some(PathBuf::from("state")));
}

#[test]
fn data_root_precedence_resolves_explicit_relative_paths_from_cwd() {
    let root = temp_dir("data-root-precedence");
    let cwd = root.join("workspace");
    let home = root.join("home");
    let environment_root = root.join("environment");
    fs::create_dir_all(&cwd).expect("workspace creates");
    fs::create_dir_all(&home).expect("home creates");
    let expected_cwd = cwd.canonicalize().expect("workspace canonicalizes");

    with_process_state(|| {
        env::set_current_dir(&cwd).expect("workspace becomes current");
        env::set_var("HOME", &home);
        env::set_var("TESSIVUM_HOME", &environment_root);

        let explicit = resolve_data_root(Some(PathBuf::from("state")))
            .expect("explicit relative data root resolves");
        assert_eq!(explicit.cwd, expected_cwd);
        assert_eq!(explicit.data_dir, expected_cwd.join("state"));

        let environment = resolve_data_root(None).expect("environment data root resolves");
        assert_eq!(environment.data_dir, environment_root);

        env::remove_var("TESSIVUM_HOME");
        let home_default = resolve_data_root(None).expect("home data root resolves");
        assert_eq!(home_default.data_dir, home.join(".tessivum"));
    });

    fs::remove_dir_all(root).expect("temporary directory removes");
}

#[test]
fn data_root_rejects_invalid_environment_and_home() {
    let root = temp_dir("invalid-data-root");
    let cwd = root.join("workspace");
    let home = root.join("home");
    fs::create_dir_all(&cwd).expect("workspace creates");
    fs::create_dir_all(&home).expect("home creates");

    with_process_state(|| {
        env::set_current_dir(&cwd).expect("workspace becomes current");
        env::set_var("HOME", &home);
        env::set_var("TESSIVUM_HOME", "relative-state");
        assert!(resolve_data_root(None)
            .expect_err("relative TESSIVUM_HOME must fail")
            .to_string()
            .contains("TESSIVUM_HOME must be an absolute"));

        env::remove_var("TESSIVUM_HOME");
        env::set_var("HOME", "relative-home");
        assert!(resolve_data_root(None)
            .expect_err("relative HOME must fail")
            .to_string()
            .contains("HOME must be an absolute"));

        env::remove_var("HOME");
        assert!(resolve_data_root(None)
            .expect_err("missing HOME must fail")
            .to_string()
            .contains("HOME is not set"));
    });

    fs::remove_dir_all(root).expect("temporary directory removes");
}

#[test]
fn data_root_detects_an_unmigrated_project_directory_without_copying() {
    let root = temp_dir("legacy-data-root");
    let cwd = root.join("workspace");
    let home = root.join("home");
    fs::create_dir_all(cwd.join(".tessivum")).expect("legacy data root creates");
    fs::create_dir_all(&home).expect("home creates");

    with_process_state(|| {
        env::set_current_dir(&cwd).expect("workspace becomes current");
        env::set_var("HOME", &home);
        env::remove_var("TESSIVUM_HOME");

        let error = resolve_data_root(None).expect_err("legacy data root requires migration");
        let message = error.to_string();
        assert!(message.contains("--data-dir"));
        assert!(message.contains("move it"));
        assert!(!home.join(".tessivum").exists());
    });

    fs::remove_dir_all(root).expect("temporary directory removes");
}

#[test]
fn help_and_version_remain_clap_display_outcomes() {
    assert_eq!(
        parse_cli(["tessivum", "--help"]).unwrap_err().kind(),
        ErrorKind::DisplayHelp
    );
    assert_eq!(
        parse_cli(["tessivum", "--version"]).unwrap_err().kind(),
        ErrorKind::DisplayVersion
    );
}

#[test]
fn launcher_flags_after_task_are_not_absorbed_as_task_text() {
    let error = parse_cli(["tessivum", "describe", "this", "--trusted-bash"])
        .expect_err("launcher flag after the task must fail");

    assert_eq!(error.kind(), ErrorKind::InvalidValue);
    assert!(error.to_string().contains("before the task"));
}

#[test]
fn invalid_headless_combinations_are_usage_errors() {
    for args in [
        ["tessivum"].as_slice(),
        ["tessivum", "   "].as_slice(),
        ["tessivum", "--resume", "task"].as_slice(),
        ["tessivum", "--max-tokens", "0", "task"].as_slice(),
        ["tessivum", "--provider", "recorded", "task"].as_slice(),
    ] {
        assert_eq!(parse_cli(args.iter().copied()).unwrap_err().exit_code(), 2);
    }

    let error = parse_cli(["tessivum", "sdk", "--patch", "base.toml"])
        .expect_err("future commands own no headless launcher arguments");
    assert_eq!(error.exit_code(), 2);
}

#[test]
fn default_rust_entrypoint_selects_headless_and_exit_codes_are_stable() {
    let command = headless(&[
        "tessivum",
        "--replay",
        "recording.jsonl",
        "preserve",
        "word",
        "boundaries",
    ]);
    assert_eq!(command.task, "preserve word boundaries");
    assert_eq!(ExitClass::Usage.code(), 2);
    assert_eq!(ExitClass::Runtime.code(), 1);
    assert_eq!(ExitClass::Cancelled.code(), 130);
}
