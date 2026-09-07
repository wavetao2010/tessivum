#![recursion_limit = "256"]

//! Wire-stable Tessivum product protocol DTOs.

pub mod agent;
pub mod agent_loop;
pub mod agent_mode;
pub mod api;
pub mod approval;
pub mod attachments;
pub mod boot_theme;
pub mod bridge;
pub mod builtin_tools;
pub mod cli;
pub mod cloudflare_tunnel;
pub mod code_runtime;
pub mod compaction;
pub mod compatible_api;
pub mod composition;
pub mod credentials;
pub mod error;
pub mod filesystem;
pub mod frontend;
pub mod goal;
pub mod headless;
pub mod host;
pub mod invariants;
pub mod jobs;
pub mod legacy;
pub mod llm;
pub mod lsp;
pub mod mcp;
pub mod openai_responses;
pub mod oracle;
pub mod permissions;
pub mod persistence_jsonl;
pub mod persistence_sqlite;
pub mod planning;
pub mod plugin_manager;
pub mod plugins;
pub mod projection;
pub mod protocol;
pub mod question;
pub mod remote_access;
pub mod sandbox;
pub mod schedule;
pub mod sdk;
pub mod session;
pub mod session_query;
pub mod settings;
pub mod skills;
pub mod storage;
pub mod subagent;
pub mod subprocess;
pub mod system_prompt;
pub mod telemetry;
pub mod tools;
pub mod web;
pub mod workflow;
pub mod workspace;

pub use error::TessivumError;
pub use openai_responses::{
    OpenAiResponsesAdapter, ProviderSnapshot, ResponsesModel, ResponsesRoute,
    ResponsesRouteResolver, RESPONSES_IMAGE_MODALITY, RESPONSES_TEXT_MODALITY,
};
pub use protocol::*;

pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(Into::into)
}

pub(crate) fn process_path(path: &std::path::Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy();
        if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{path}").into();
        }
        if let Some(path) = path.strip_prefix(r"\\?\") {
            return path.into();
        }
    }
    path.to_path_buf()
}
