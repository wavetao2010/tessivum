//! Wire-stable Tessivum product protocol DTOs.

pub mod agent;
pub mod agent_loop;
pub mod builtin_tools;
pub mod cli;
pub mod error;
pub mod llm;
pub mod oracle;
pub mod persistence_jsonl;
pub mod protocol;
pub mod session;
pub mod system_prompt;
pub mod tools;

pub use error::TessivumError;
pub use protocol::*;
