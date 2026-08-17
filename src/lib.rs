//! Wire-stable Tessivum product protocol DTOs.

pub mod agent;
pub mod agent_loop;
pub mod attachments;
pub mod builtin_tools;
pub mod cli;
pub mod code_runtime;
pub mod credentials;
pub mod error;
pub mod filesystem;
pub mod headless;
pub mod invariants;
pub mod llm;
pub mod oracle;
pub mod persistence_jsonl;
pub mod persistence_sqlite;
pub mod projection;
pub mod protocol;
pub mod sandbox;
pub mod session;
pub mod session_query;
pub mod settings;
pub mod storage;
pub mod subprocess;
pub mod system_prompt;
pub mod telemetry;
pub mod tools;

pub use error::TessivumError;
pub use protocol::*;
