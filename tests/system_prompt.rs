use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
};

use serde_json::json;
use tessivum::ToolSchema;
use tessivum_core::ContextHandle;

pub use tessivum::{TessivumError, ToolSchema as ReexportedToolSchema};
pub use ReexportedToolSchema as ToolSchemaForModule;

#[path = "../src/system_prompt.rs"]
mod system_prompt;

use system_prompt::{system_prompt_service_key, PromptRegistration, PromptSection, SystemPrompt};

fn section(id: &str, order: i64, text: &str) -> PromptSection {
    PromptSection::new(id, order, text)
}

fn tool(name: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: format!("{name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

#[test]
fn assembly_orders_all_sections_and_preserves_tools() {
    let prompt = SystemPrompt::new();
    let _last = prompt
        .register(section("registered-z", 20, "registered z"))
        .expect("registered section is accepted");
    let _first = prompt
        .register(section("registered-a", 10, "registered a"))
        .expect("registered section is accepted");
    let tools = vec![tool("shell"), tool("read")];

    let assembly = prompt
        .assemble(
            vec![
                section("runtime-z", 10, "runtime z"),
                section("runtime-a", 10, "runtime a"),
                section("empty", 15, ""),
            ],
            tools.clone(),
        )
        .expect("runtime sections assemble");

    assert_eq!(
        assembly.text,
        "registered a\n\nruntime a\n\nruntime z\n\nregistered z"
    );
    assert_eq!(assembly.tools, tools);
}

#[test]
fn blank_and_duplicate_ids_are_rejected() {
    let prompt = SystemPrompt::new();
    assert_eq!(
        prompt
            .register(section(" \t", 0, "invalid"))
            .expect_err("blank registered IDs are rejected")
            .code,
        "INVALID_PROMPT_SECTION_ID"
    );

    let _registered = prompt
        .register(section("unique", 0, "first"))
        .expect("first registered ID is accepted");
    assert_eq!(
        prompt
            .register(section("unique", 1, "second"))
            .expect_err("duplicate registered IDs are rejected")
            .code,
        "DUPLICATE_PROMPT_SECTION_ID"
    );
    assert_eq!(
        prompt
            .assemble(
                vec![section("runtime", 0, "one"), section("runtime", 1, "two")],
                vec![],
            )
            .expect_err("duplicate runtime IDs are rejected")
            .code,
        "DUPLICATE_PROMPT_SECTION_ID"
    );
    assert_eq!(
        prompt
            .assemble(vec![section("\n", 0, "invalid")], vec![])
            .expect_err("blank runtime IDs are rejected")
            .code,
        "INVALID_PROMPT_SECTION_ID"
    );
}

#[test]
fn registration_removal_is_idempotent_and_lifetime_owned() {
    let prompt = SystemPrompt::new();
    let registration = prompt
        .register(section("owned", 0, "owned text"))
        .expect("registration succeeds");
    assert_eq!(
        prompt
            .assemble(Vec::<PromptSection>::new(), vec![])
            .unwrap()
            .text,
        "owned text"
    );

    assert!(registration.remove());
    assert!(!registration.remove());
    assert_eq!(
        prompt
            .assemble(Vec::<PromptSection>::new(), vec![])
            .unwrap()
            .text,
        ""
    );

    {
        let _temporary = prompt
            .register(section("temporary", 0, "temporary text"))
            .expect("temporary registration succeeds");
        assert_eq!(
            prompt
                .assemble(Vec::<PromptSection>::new(), vec![])
                .unwrap()
                .text,
            "temporary text"
        );
    }
    assert_eq!(
        prompt
            .assemble(Vec::<PromptSection>::new(), vec![])
            .unwrap()
            .text,
        ""
    );
}

#[test]
fn observers_contain_failures_and_can_reenter_the_registry() {
    let prompt = SystemPrompt::new();
    let panic_calls = Arc::new(AtomicUsize::new(0));
    let panic_calls_for_observer = Arc::clone(&panic_calls);
    let _failing = prompt.subscribe(move || {
        panic_calls_for_observer.fetch_add(1, Ordering::SeqCst);
        panic!("observer failures are isolated");
    });

    let callback_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls_for_observer = Arc::clone(&callback_calls);
    let retained_registration: Arc<Mutex<Option<PromptRegistration>>> = Arc::new(Mutex::new(None));
    let retained_registration_for_observer = Arc::clone(&retained_registration);
    let reentrant_prompt = prompt.clone();
    let _reentrant = prompt.subscribe(move || {
        if callback_calls_for_observer.fetch_add(1, Ordering::SeqCst) == 0 {
            let registration = reentrant_prompt
                .register(section("reentrant", 0, "reentrant text"))
                .expect("observer may register without deadlocking");
            *retained_registration_for_observer
                .lock()
                .expect("test registration storage is available") = Some(registration);
        }
    });

    let _outer = prompt
        .register(section("outer", 1, "outer text"))
        .expect("failing observer does not fail registration");

    assert_eq!(panic_calls.load(Ordering::SeqCst), 2);
    assert_eq!(callback_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        prompt
            .assemble(Vec::<PromptSection>::new(), vec![])
            .unwrap()
            .text,
        "reentrant text\n\nouter text"
    );
}

#[test]
fn concurrent_snapshots_are_consistent() {
    let prompt = SystemPrompt::new();
    let _base = prompt
        .register(section("base", 0, "base"))
        .expect("base registration succeeds");
    let workers = (0..16)
        .map(|_| {
            let prompt = prompt.clone();
            thread::spawn(move || {
                for _ in 0..128 {
                    let assembly = prompt
                        .assemble(vec![section("runtime", 1, "runtime")], vec![])
                        .expect("snapshot assembles");
                    assert_eq!(assembly.text, "base\n\nruntime");
                }
            })
        })
        .collect::<Vec<_>>();

    for worker in workers {
        worker.join().expect("snapshot worker does not panic");
    }
}

#[test]
fn context_handle_publishes_the_system_prompt_service() {
    let context = ContextHandle::root();
    let _provider = SystemPrompt::new()
        .publish(&context)
        .expect("system prompt service publishes into context");
    let service = context
        .get::<SystemPrompt>(&system_prompt_service_key())
        .expect("system prompt lookup succeeds")
        .expect("published system prompt is visible");
    let tools = vec![tool("read")];

    let assembly = service
        .with(|prompt| prompt.assemble(vec![section("runtime", 0, "runtime")], tools.clone()))
        .expect("current service handle is callable")
        .expect("published prompt assembles");

    assert_eq!(
        system_prompt_service_key().diagnostic_key(),
        "harness.system-prompt@1"
    );
    assert_eq!(assembly.text, "runtime");
    assert_eq!(assembly.tools, tools);
}
