#[path = "../src/cli.rs"]
mod cli;

use std::{num::NonZeroU64, path::PathBuf};

use clap::error::ErrorKind;
use cli::{parse_cli, CliCommand, ExitClass, HeadlessCommand, PluginAction};

fn headless(args: &[&str]) -> HeadlessCommand {
    let cli = parse_cli(args.iter().copied()).expect("headless invocation should parse");
    match cli.command {
        CliCommand::Headless(command) => command,
        command => panic!("expected Headless, got {command:?}"),
    }
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
    assert_eq!(add.data_dir, PathBuf::from("state"));
    assert_eq!(add.action, PluginAction::Add("@scope/plugin@1.2.3".into()));

    let remove = parse_cli(["tessivum", "plugin", "remove", "@scope/plugin"]).unwrap();
    let CliCommand::Plugin(remove) = remove.command else {
        panic!("expected plugin command")
    };
    assert_eq!(remove.data_dir, PathBuf::from(".tessivum"));
    assert_eq!(remove.action, PluginAction::Remove("@scope/plugin".into()));
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
    let error = parse_cli(["tessivum", "sdk", "--data-dir", "state"])
        .expect_err("SDK does not own a data directory option");
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
