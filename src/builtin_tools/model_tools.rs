use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};
use tessivum_core::CancellationToken;
use url::Url;

use crate::{
    attachments::{AttachmentInput, AttachmentStore, ImageMediaType},
    filesystem::{Filesystem, FsLiteralEdit, FsNodeKind, FsTarget, FsWriteGuard},
    sandbox::SandboxMode,
    session::SessionStore,
    tools::{
        ToolApproval, ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    web::{WebBody, WebFetchRequest, WebRuntime, WebSearchRequest},
    workspace::{SessionResourceResolver, WorkspaceError, WorkspaceLease},
    ContentBlock, TessivumError,
};

use super::{BuiltinToolsConfig, HostToolServices};

const READ_LIMIT: usize = 2_000;
const READ_MAX_LINE_LENGTH: usize = 2_000;
const GLOB_MAX_RESULTS: usize = 100;
const GREP_MAX_MATCHES: usize = 250;
const GREP_MAX_LINE_BYTES: usize = 2_000;
const MAX_SEARCH_CANDIDATES: usize = 20_000;
const MAX_SEARCH_FILE_BYTES: usize = 20_000_000;
const SEARCH_META_MAX_BYTES: usize = 64 * 1024;
const WEB_FETCH_MAX_OUTPUT_CHARS: usize = 200_000;
const WEB_FETCH_TRUNCATION_FOOTER: &str =
    "\n\n(Content truncated. Fetch a more specific URL or section for the full text.)";
const VCS_DIRECTORIES: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

#[derive(Clone)]
struct FileTools {
    cwd: PathBuf,
    resolver: Option<Arc<SessionResourceResolver>>,
    sessions: Option<SessionStore>,
    approval: Option<Arc<dyn ToolApproval>>,
    max_output_bytes: usize,
}

impl FileTools {
    fn filesystem(
        &self,
        context: &ToolRunContext,
    ) -> Result<(Filesystem, PathBuf, Option<WorkspaceLease>), TessivumError> {
        let lease = self
            .resolver
            .as_ref()
            .map(|resolver| resolver.resolve(&context.session))
            .transpose()
            .map_err(|error| workspace_error(context, error))?;
        let root = match &lease {
            Some(lease) => lease
                .validate_current()
                .map_err(|error| workspace_error(context, error))?,
            None => self.cwd.clone(),
        };
        Ok((Filesystem::new(&root), root, lease))
    }

    async fn allow_write(
        &self,
        context: &ToolRunContext,
        name: &str,
        schema: Value,
        arguments: &Value,
    ) -> Result<(), TessivumError> {
        check_cancelled(&context.cancellation)?;
        let requested = arguments
            .get("sandbox_permissions")
            .map(|value| serde_json::from_value::<SandboxMode>(value.clone()))
            .transpose()
            .map_err(|_| {
                invalid_arguments(
                    "sandbox_permissions is invalid",
                    json!({"path": "$.sandbox_permissions"}),
                )
            })?;
        let justification = optional_string(arguments, "justification")?;
        if requested.is_some() != justification.is_some()
            || justification.is_some_and(|justification| justification.trim().is_empty())
        {
            return Err(invalid_arguments(
                "sandbox_permissions and a non-blank justification must be provided together",
                json!({"path": "$.sandbox_permissions"}),
            ));
        }
        if requested == Some(SandboxMode::DangerFullAccess) {
            return Err(tool_error(
                "SANDBOX_MODE_UNSUPPORTED",
                "filesystem tools never permit danger-full-access; use a workspace-confined write",
                Value::Null,
            ));
        }
        let current = session_sandbox_mode(self.sessions.as_ref(), &context.session);
        if current == SandboxMode::WorkspaceWrite {
            if requested.is_some() {
                return Err(invalid_arguments(
                    "sandbox_permissions must request a strictly wider mode",
                    json!({"path": "$.sandbox_permissions"}),
                ));
            }
            return Ok(());
        }
        if requested != Some(SandboxMode::WorkspaceWrite) {
            return Err(tool_error(
                "SANDBOX_WRITE_DENIED",
                "workspace is read-only; request workspace-write with a justification",
                json!({"name": name}),
            ));
        }
        let approved = self
            .approval
            .as_ref()
            .map(|approval| async {
                approval
                    .approve(
                        context,
                        &crate::ToolSchema {
                            name: name.into(),
                            description: "Writes only inside the current workspace.".into(),
                            parameters: schema,
                        },
                        arguments,
                    )
                    .await
            })
            .ok_or_else(|| {
                tool_error(
                    "TOOL_APPROVAL_DENIED",
                    "sandbox escalation was not approved",
                    json!({"name": name}),
                )
            })?
            .await?
            .unwrap_or(false);
        check_cancelled(&context.cancellation)?;
        if !approved {
            return Err(tool_error(
                "TOOL_APPROVAL_DENIED",
                "sandbox escalation was not approved",
                json!({"name": name}),
            ));
        }
        Ok(())
    }
}

pub(super) fn register(
    runtime: &ToolRuntime,
    config: &BuiltinToolsConfig,
    services: &HostToolServices,
) -> Result<Vec<ToolRegistration>, TessivumError> {
    let files = FileTools {
        cwd: config.cwd.clone(),
        resolver: config.resolver.clone(),
        sessions: Some(services.sessions.clone()),
        approval: Some(services.approval.clone()),
        max_output_bytes: config.max_output_bytes,
    };
    let attachments = services.attachments.clone();
    let web = services.web.clone();
    Ok(vec![
        runtime.register(ToolDefinition::new(
            "read",
            "Read a UTF-8 text file and return line-numbered content.",
            read_schema(),
            ReadFile {
                files: files.clone(),
            },
        ))?,
        runtime.register(ToolDefinition::new(
            "write",
            "Create or fully replace a UTF-8 text file.",
            write_schema(),
            WriteFile {
                files: files.clone(),
            },
        ))?,
        runtime.register(ToolDefinition::new(
            "edit",
            "Edit an existing UTF-8 text file by replacing literal text.",
            edit_schema(),
            EditFile {
                files: files.clone(),
            },
        ))?,
        runtime.register(ToolDefinition::new(
            "str_replace_editor",
            "Edit an existing UTF-8 text file by replacing literal text.",
            edit_schema(),
            EditFile {
                files: files.clone(),
            },
        ))?,
        runtime.register(ToolDefinition::new(
            "read_image",
            "Read a PNG/JPEG/WebP/GIF file and return the image itself.",
            image_schema(),
            ReadImage {
                files: files.clone(),
                attachments,
            },
        ))?,
        runtime.register(ToolDefinition::new(
            "glob",
            "Find workspace files whose paths match a glob pattern.",
            glob_schema(),
            GlobFiles {
                files: files.clone(),
            },
        ))?,
        runtime.register(ToolDefinition::new(
            "grep",
            "Search workspace file contents with a regular expression.",
            grep_schema(),
            GrepFiles { files },
        ))?,
        runtime.register(ToolDefinition::new(
            "web_search",
            "Search the web for current information and source URLs.",
            web_search_schema(),
            WebSearch { web: web.clone() },
        ))?,
        runtime.register(ToolDefinition::new(
            "web_fetch",
            "Fetch the content of a specific HTTP(S) URL.",
            web_fetch_schema(),
            WebFetch { web },
        ))?,
    ])
}

fn read_schema() -> Value {
    json!({"type":"object","properties":{"file_path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["file_path"],"additionalProperties":false})
}

fn write_schema() -> Value {
    json!({"type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"},"sandbox_permissions":{"type":"string","enum":["workspace-write","danger-full-access"]},"justification":{"type":"string"}},"required":["file_path","content"],"additionalProperties":false})
}

fn edit_schema() -> Value {
    json!({"type":"object","properties":{"file_path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"},"sandbox_permissions":{"type":"string","enum":["workspace-write","danger-full-access"]},"justification":{"type":"string"}},"required":["file_path","old_string","new_string"],"additionalProperties":false})
}

fn image_schema() -> Value {
    json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"],"additionalProperties":false})
}

fn glob_schema() -> Value {
    json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"],"additionalProperties":false})
}

fn grep_schema() -> Value {
    json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string"}},"required":["pattern"],"additionalProperties":false})
}

fn web_search_schema() -> Value {
    json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false})
}

fn web_fetch_schema() -> Value {
    json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"],"additionalProperties":false})
}

struct ReadFile {
    files: FileTools,
}

#[async_trait]
impl ToolHandler for ReadFile {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let path = required_nonblank(&arguments, "file_path")?;
        let offset = positive_optional(&arguments, "offset", 1)?;
        let limit = positive_optional(&arguments, "limit", READ_LIMIT)?;
        if limit > READ_LIMIT {
            return Err(invalid_arguments(
                "limit exceeds the configured read limit",
                json!({"path": "$.limit", "max": READ_LIMIT}),
            ));
        }
        check_cancelled(&context.cancellation)?;
        let (filesystem, root, lease) = self.files.filesystem(&context)?;
        let target = filesystem.target(path)?;
        let text = filesystem
            .read_text(&target, MAX_SEARCH_FILE_BYTES)
            .await
            .map_err(|error| {
                if error.code == "FS_NOT_FOUND" {
                    TessivumError::new(
                        "READ_FAILED",
                        format!("cannot read {:?}: not found", target.display()),
                        "tools",
                        json!({"path": target.display()}),
                    )
                } else {
                    error
                }
            })?;
        check_lease(&lease, &context)?;
        let all = text.lines().collect::<Vec<_>>();
        let start = offset.saturating_sub(1);
        let mut lines = Vec::new();
        let mut returned_bytes = 0usize;
        let mut truncated = false;
        let mut truncated_by_bytes = false;
        for (index, line) in all.iter().enumerate().skip(start) {
            if lines.len() == limit {
                truncated = true;
                break;
            }
            let line = truncate_chars(line, READ_MAX_LINE_LENGTH);
            let bytes = line.len().saturating_add((!lines.is_empty()) as usize);
            if returned_bytes.saturating_add(bytes) > self.files.max_output_bytes {
                truncated = true;
                truncated_by_bytes = true;
                break;
            }
            returned_bytes += bytes;
            lines.push(json!({"number": index + 1, "text": line}));
        }
        if start.saturating_add(lines.len()) < all.len() {
            truncated = true;
        }
        let display = display_path(&root, &target);
        let end_line = lines
            .last()
            .and_then(|line| line.get("number"))
            .and_then(Value::as_u64)
            .unwrap_or(offset.saturating_sub(1) as u64);
        let footer = if truncated_by_bytes {
            format!(
                "(Output capped. Showing lines {offset}-{end_line}. Use offset={} to continue.)",
                end_line.saturating_add(1)
            )
        } else if end_line < all.len() as u64 {
            format!(
                "(Showing lines {offset}-{end_line} of {}. Use offset={} to continue.)",
                all.len(),
                end_line.saturating_add(1)
            )
        } else {
            format!("(End of file - total {} lines)", all.len())
        };
        let body = if lines.is_empty() {
            footer
        } else {
            let rows = lines
                .iter()
                .filter_map(|line| {
                    Some(format!(
                        "{}: {}",
                        line.get("number")?.as_u64()?,
                        line.get("text")?.as_str()?
                    ))
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("{rows}\n\n{footer}")
        };
        let text =
            format!("<path>{display}</path>\n<type>file</type>\n<content>\n{body}\n</content>");
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"path": display, "offset": offset, "lines": lines, "totalLines": all.len(), "truncated": truncated, "locations": [{"path": display, "line": offset}]}),
        ))
    }
}

struct WriteFile {
    files: FileTools,
}

#[async_trait]
impl ToolHandler for WriteFile {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let path = required_nonblank(&arguments, "file_path")?;
        let content = required_string(&arguments, "content")?;
        self.files
            .allow_write(&context, "write", write_schema(), &arguments)
            .await?;
        check_cancelled(&context.cancellation)?;
        let (filesystem, root, lease) = self.files.filesystem(&context)?;
        let target = filesystem.target(path)?;
        let outcome = filesystem
            .write_text_outcome(&target, content, FsWriteGuard::default())
            .await?;
        check_cancelled(&context.cancellation)?;
        check_lease(&lease, &context)?;
        let display = display_path(&root, &outcome.observation.target);
        let created = outcome.before.is_none();
        let text = format!(
            "<path>{display}</path>\n<type>file</type>\n<content>\n{} file\n</content>",
            if created { "Created" } else { "Updated" }
        );
        let diffs = outcome
            .before
            .as_ref()
            .map(|before| diff_hunks(path, before, &outcome.after))
            .unwrap_or_default();
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"path": display, "operation": if created { "create" } else { "update" }, "diffs": diffs, "locations": [{"path": display}], "bytes": outcome.observation.len}),
        ))
    }
}

struct EditFile {
    files: FileTools,
}

#[async_trait]
impl ToolHandler for EditFile {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let path = required_nonblank(&arguments, "file_path")?;
        let old = required_string(&arguments, "old_string")?;
        let new = required_string(&arguments, "new_string")?;
        if old.is_empty() {
            return Err(invalid_arguments(
                "old_string must not be empty",
                json!({"path": "$.old_string"}),
            ));
        }
        if old == new {
            return Err(invalid_arguments(
                "old_string and new_string must differ",
                json!({"path": "$.new_string"}),
            ));
        }
        let replace_all = arguments
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.files
            .allow_write(&context, "edit", edit_schema(), &arguments)
            .await?;
        check_cancelled(&context.cancellation)?;
        let (filesystem, root, lease) = self.files.filesystem(&context)?;
        let target = filesystem.target(path)?;
        let outcome = filesystem
            .edit_text_outcome_all(&target, FsLiteralEdit::new(old, new), replace_all)
            .await?;
        check_cancelled(&context.cancellation)?;
        check_lease(&lease, &context)?;
        let display = display_path(&root, &outcome.observation.target);
        let text = if replace_all {
            format!(
                "The file {display} has been updated. All occurrences were successfully replaced."
            )
        } else {
            format!("The file {display} has been updated successfully.")
        };
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"path": display, "diffs": diff_hunks(path, &outcome.before, &outcome.after), "locations": [{"path": path}], "bytes": outcome.observation.len}),
        ))
    }
}

struct ReadImage {
    files: FileTools,
    attachments: Arc<AttachmentStore>,
}

#[async_trait]
impl ToolHandler for ReadImage {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let path = required_nonblank(&arguments, "file_path")?;
        let declared = image_type(path).ok_or_else(|| {
            invalid_arguments(
                "read_image only accepts PNG/JPEG/WebP/GIF paths",
                json!({"path": "$.file_path"}),
            )
        })?;
        if !self.attachments.limits().media_types.contains(&declared) {
            return Err(tool_error(
                "UNSUPPORTED_ATTACHMENT_MEDIA_TYPE",
                "this deployment does not admit the requested image type",
                json!({"path": path}),
            ));
        }
        check_cancelled(&context.cancellation)?;
        let (filesystem, root, lease) = self.files.filesystem(&context)?;
        let target = filesystem.target(path)?;
        let byte_cap = self
            .attachments
            .limits()
            .max_image_bytes
            .min(self.attachments.limits().max_message_image_bytes);
        let data = filesystem
            .read_bytes(&target, usize::try_from(byte_cap).unwrap_or(usize::MAX))
            .await?;
        check_cancelled(&context.cancellation)?;
        let display = display_path(&root, &target);
        let reference = self
            .attachments
            .save(AttachmentInput::new(
                data,
                Path::new(&display)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
            ))
            .await
            .map_err(attachment_error)?;
        if reference.media_type != declared {
            return Err(tool_error(
                "IMAGE_TYPE_MISMATCH",
                "image extension does not match its validated bytes",
                json!({"path": display, "declared": declared.as_str(), "actual": reference.media_type.as_str()}),
            ));
        }
        check_lease(&lease, &context)?;
        check_cancelled(&context.cancellation)?;
        let image = reference.safe_metadata();
        let text = format!("<path>{display}</path>\n<type>image</type>\n<content>\n{} image, {}x{} px, {} bytes\n</content>", reference.media_type.as_str(), reference.width, reference.height, reference.bytes);
        Ok(ToolOutput::new(
            vec![
                ContentBlock::Text { text },
                ContentBlock::Image {
                    attachment: image.clone(),
                },
            ],
            false,
            json!({"path": display, "image": image, "locations": [{"path": display}]}),
        ))
    }
}

struct GlobFiles {
    files: FileTools,
}

#[async_trait]
impl ToolHandler for GlobFiles {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let pattern = required_nonblank(&arguments, "pattern")?;
        if pattern.trim().is_empty() {
            return Err(invalid_arguments(
                "pattern must not be blank",
                json!({"path": "$.pattern"}),
            ));
        }
        let path = optional_string(&arguments, "path")?;
        if path.is_some_and(|path| path.trim().is_empty()) {
            return Err(invalid_arguments(
                "path must not be blank",
                json!({"path": "$.path"}),
            ));
        }
        let expression = glob_regex(pattern)?;
        let (filesystem, root, lease) = self.files.filesystem(&context)?;
        let target = filesystem.target(path.unwrap_or("."))?;
        let files = collect_files(&filesystem, &target, &context.cancellation).await?;
        let mut paths = Vec::new();
        for file in files {
            check_cancelled(&context.cancellation)?;
            let path = display_path(&root, &file);
            if expression.is_match(&path) {
                paths.push((filesystem.modified(&file).await?, path));
            }
        }
        check_lease(&lease, &context)?;
        paths.sort_by(|(left_time, left_path), (right_time, right_path)| {
            right_time
                .cmp(left_time)
                .then_with(|| left_path.cmp(right_path))
        });
        let total = paths.len();
        let mut paths = paths.into_iter().map(|(_, path)| path).collect::<Vec<_>>();
        paths.truncate(GLOB_MAX_RESULTS);
        let (paths, truncated) = cap_paths(paths, total, total > GLOB_MAX_RESULTS);
        let rendered = if total == 0 {
            "No files found".to_owned()
        } else if truncated {
            format!(
                "{}\n\n(Showing {} of {total} paths. Narrow pattern or path to see more.)",
                paths.join("\n"),
                paths.len()
            )
        } else {
            paths.join("\n")
        };
        let text = truncate_utf8(&rendered, self.files.max_output_bytes);
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"shape": "paths", "paths": paths, "truncated": truncated, "total": total}),
        ))
    }
}

struct GrepFiles {
    files: FileTools,
}

#[async_trait]
impl ToolHandler for GrepFiles {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let pattern = required_string(&arguments, "pattern")?;
        if pattern.is_empty() {
            return Err(invalid_arguments(
                "pattern must not be empty",
                json!({"path": "$.pattern"}),
            ));
        }
        let expression = Regex::new(pattern).map_err(|error| {
            tool_error(
                "SEARCH_INVALID_PATTERN",
                "pattern is not a valid regular expression",
                json!({"path": "$.pattern", "error": error.to_string()}),
            )
        })?;
        let path = optional_string(&arguments, "path")?;
        if path.is_some_and(|path| path.trim().is_empty()) {
            return Err(invalid_arguments(
                "path must not be blank",
                json!({"path": "$.path"}),
            ));
        }
        let include = optional_string(&arguments, "include")?;
        let include = match include {
            Some(include) => Some(validate_include(include)?),
            None => None,
        };
        let (filesystem, root, lease) = self.files.filesystem(&context)?;
        let target = filesystem.target(path.unwrap_or("."))?;
        let files = collect_files(&filesystem, &target, &context.cancellation).await?;
        let mut matches = Vec::new();
        let mut total = 0usize;
        for target in files {
            check_cancelled(&context.cancellation)?;
            let display = display_path(&root, &target);
            if include
                .as_ref()
                .is_some_and(|include| !include.is_match(&display))
            {
                continue;
            }
            let bytes = match filesystem.read_bytes(&target, MAX_SEARCH_FILE_BYTES).await {
                Ok(bytes) => bytes,
                Err(error)
                    if error.code == "FS_NOT_REGULAR_FILE" || error.code == "FS_TOO_LARGE" =>
                {
                    continue
                }
                Err(error) => return Err(error),
            };
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            for (line_number, line) in text.lines().enumerate() {
                if expression.is_match(line) {
                    total = total.saturating_add(1);
                    if matches.len() < GREP_MAX_MATCHES {
                        matches.push(json!({"path": display, "lineNumber": line_number + 1, "line": truncate_utf8(line, GREP_MAX_LINE_BYTES)}));
                    }
                }
            }
        }
        check_lease(&lease, &context)?;
        let (files, truncated) =
            cap_match_groups(group_matches(&matches), total, total > matches.len());
        let text = truncate_utf8(
            &format_grep(&matches_from_groups(&files), total, truncated),
            self.files.max_output_bytes,
        );
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"shape": "matches", "files": files, "truncated": truncated, "total": total}),
        ))
    }
}

struct WebSearch {
    web: WebRuntime,
}

#[async_trait]
impl ToolHandler for WebSearch {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let query = required_nonblank(&arguments, "query")?;
        let result = self
            .web
            .search_for_session(
                WebSearchRequest {
                    query: query.to_owned(),
                    max_results: None,
                },
                context.session.clone(),
                context.cancellation.clone(),
            )
            .await?;
        let sources = result
            .sources
            .into_iter()
            .map(|source| {
                let mut projected = serde_json::Map::new();
                projected.insert(String::from("url"), Value::String(source.url));
                if !source.title.is_empty() {
                    projected.insert(String::from("title"), Value::String(source.title));
                }
                if let Some(snippet) = source.snippet.filter(|snippet| !snippet.is_empty()) {
                    projected.insert(String::from("snippet"), Value::String(snippet));
                }
                if let Some(published_at) = source
                    .published_at
                    .filter(|published_at| !published_at.is_empty())
                {
                    projected.insert(String::from("publishedAt"), Value::String(published_at));
                }
                Value::Object(projected)
            })
            .collect::<Vec<_>>();
        let lines = sources
            .iter()
            .map(|source| {
                let url = source
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let title = source
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| source_label(url));
                let suffix = source
                    .get("snippet")
                    .and_then(Value::as_str)
                    .filter(|snippet| !snippet.is_empty())
                    .map(|snippet| format!(" — {snippet}"))
                    .unwrap_or_default();
                format!("- [{title}]({url}){suffix}")
            })
            .collect::<Vec<_>>();
        let mut text = if lines.is_empty() {
            "No results found.".to_owned()
        } else {
            format!("Sources:\n{}", lines.join("\n"))
        };
        if result.truncated {
            text.push_str(&format!(
                "\n\n(Showing the first {} sources. Refine the query for more.)",
                sources.len()
            ));
        }
        text.push_str("\n\nCite the relevant URLs above as markdown links in your answer.");
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"sources": sources, "truncated": result.truncated}),
        ))
    }
}

struct WebFetch {
    web: WebRuntime,
}

#[async_trait]
impl ToolHandler for WebFetch {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let url = required_nonblank(&arguments, "url")?;
        let result = self
            .web
            .fetch(
                WebFetchRequest {
                    url: url.to_owned(),
                },
                context.cancellation.clone(),
            )
            .await?;
        let body = match result.body {
            WebBody::Html { html } => html,
            WebBody::Text { text } => text,
        };
        let (text, truncated) = format_fetch_output(
            &result.final_url,
            result.status_code,
            &body,
            result.truncated,
        );
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"url": result.final_url, "statusCode": result.status_code, "truncated": truncated}),
        ))
    }
}

fn required_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, TessivumError> {
    arguments.get(name).and_then(Value::as_str).ok_or_else(|| {
        invalid_arguments(
            &format!("{name} must be a string"),
            json!({"path": format!("$.{name}")}),
        )
    })
}

fn required_nonblank<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, TessivumError> {
    let value = required_string(arguments, name)?;
    if value.trim().is_empty() {
        return Err(invalid_arguments(
            &format!("{name} must not be blank"),
            json!({"path": format!("$.{name}")}),
        ));
    }
    Ok(value)
}

fn optional_string<'a>(arguments: &'a Value, name: &str) -> Result<Option<&'a str>, TessivumError> {
    match arguments.get(name) {
        None => Ok(None),
        Some(value) => value.as_str().map(Some).ok_or_else(|| {
            invalid_arguments(
                &format!("{name} must be a string"),
                json!({"path": format!("$.{name}")}),
            )
        }),
    }
}

fn positive_optional(
    arguments: &Value,
    name: &str,
    default: usize,
) -> Result<usize, TessivumError> {
    let Some(value) = arguments.get(name) else {
        return Ok(default);
    };
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            invalid_arguments(
                &format!("{name} must be a positive integer"),
                json!({"path": format!("$.{name}")}),
            )
        })
}

async fn collect_files(
    filesystem: &Filesystem,
    root: &FsTarget,
    cancellation: &CancellationToken,
) -> Result<Vec<FsTarget>, TessivumError> {
    let mut pending = vec![root.clone()];
    let mut files = Vec::new();
    while let Some(target) = pending.pop() {
        check_cancelled(cancellation)?;
        if is_vcs_path(&target) {
            continue;
        }
        match filesystem.lstat(&target).await?.kind {
            FsNodeKind::File => {
                files.push(target);
                if files.len() > MAX_SEARCH_CANDIDATES {
                    return Err(tool_error(
                        "SEARCH_RAW_OUTPUT_OVERFLOW",
                        "search discovered too many files; narrow path",
                        json!({"maxFiles": MAX_SEARCH_CANDIDATES}),
                    ));
                }
            }
            FsNodeKind::Directory => {
                let children = filesystem.list(&target, MAX_SEARCH_CANDIDATES).await?;
                pending.extend(children.into_iter().rev());
            }
            FsNodeKind::Symlink | FsNodeKind::Other => {}
        }
    }
    Ok(files)
}

fn is_vcs_path(target: &FsTarget) -> bool {
    Path::new(&target.display()).components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| VCS_DIRECTORIES.contains(&name))
    })
}

fn glob_regex(pattern: &str) -> Result<Regex, TessivumError> {
    let pattern = if pattern.contains('/') {
        pattern.to_owned()
    } else {
        format!("**/{pattern}")
    };
    Regex::new(&format!("^{}$", glob_fragment(&pattern)?)).map_err(|error| {
        tool_error(
            "INVALID_TOOL_ARGUMENTS",
            "glob pattern is invalid",
            json!({"path": "$.pattern", "error": error.to_string()}),
        )
    })
}

fn glob_fragment(pattern: &str) -> Result<String, TessivumError> {
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' if chars.get(index + 1) == Some(&'*') && chars.get(index + 2) == Some(&'/') => {
                output.push_str("(?:.*/)?");
                index += 3;
            }
            '*' if chars.get(index + 1) == Some(&'*') => {
                output.push_str(".*");
                index += 2;
            }
            '*' => {
                output.push_str("[^/]*");
                index += 1;
            }
            '?' => {
                output.push_str("[^/]");
                index += 1;
            }
            '/' => {
                output.push('/');
                index += 1;
            }
            '{' => {
                let mut depth = 1usize;
                let mut end = index + 1;
                while end < chars.len() && depth != 0 {
                    match chars[end] {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                if depth != 0 {
                    return Err(invalid_arguments(
                        "glob pattern has an unclosed brace",
                        json!({"path": "$.pattern"}),
                    ));
                }
                let inner = chars[index + 1..end - 1].iter().collect::<String>();
                let mut alternatives = Vec::new();
                let mut start = 0;
                let mut nested = 0usize;
                for (offset, character) in inner.char_indices() {
                    match character {
                        '{' => nested += 1,
                        '}' => nested = nested.saturating_sub(1),
                        ',' if nested == 0 => {
                            alternatives.push(&inner[start..offset]);
                            start = offset + 1;
                        }
                        _ => {}
                    }
                }
                alternatives.push(&inner[start..]);
                if alternatives.len() == 1 {
                    output.push_str(&regex::escape(&format!("{{{inner}}}")));
                } else {
                    output.push_str("(?:");
                    for (offset, alternative) in alternatives.into_iter().enumerate() {
                        if offset != 0 {
                            output.push('|');
                        }
                        output.push_str(&glob_fragment(alternative)?);
                    }
                    output.push(')');
                }
                index = end;
            }
            character => {
                output.push_str(&regex::escape(&character.to_string()));
                index += 1;
            }
        }
    }
    Ok(output)
}

fn validate_include(include: &str) -> Result<Regex, TessivumError> {
    if include.trim().is_empty() || include.starts_with('!') {
        return Err(invalid_arguments(
            "include must be one non-negated glob",
            json!({"path": "$.include"}),
        ));
    }
    let mut depth = 0usize;
    for character in include.chars() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                return Err(invalid_arguments(
                    "include must be one glob, not a comma-separated list",
                    json!({"path": "$.include"}),
                ))
            }
            _ => {}
        }
    }
    glob_regex(include)
}

fn group_matches(matches: &[Value]) -> Vec<Value> {
    let mut grouped = Vec::<(String, Vec<Value>)>::new();
    for item in matches {
        let Some(path) = item.get("path").and_then(Value::as_str) else {
            continue;
        };
        let line = json!({"lineNumber": item["lineNumber"], "line": item["line"]});
        if let Some((_, lines)) = grouped.iter_mut().find(|(known, _)| known == path) {
            lines.push(line);
        } else {
            grouped.push((path.to_owned(), vec![line]));
        }
    }
    grouped
        .into_iter()
        .map(|(path, matches)| json!({"path": path, "matches": matches}))
        .collect()
}

fn cap_paths(mut paths: Vec<String>, total: usize, mut truncated: bool) -> (Vec<String>, bool) {
    while paths.len() > 1 {
        let meta = json!({"shape": "paths", "paths": paths, "truncated": true, "total": total});
        if serde_json::to_vec(&meta).map_or(usize::MAX, |bytes| bytes.len())
            <= SEARCH_META_MAX_BYTES
        {
            break;
        }
        paths.pop();
        truncated = true;
    }
    (paths, truncated)
}

fn cap_match_groups(
    mut files: Vec<Value>,
    total: usize,
    mut truncated: bool,
) -> (Vec<Value>, bool) {
    while files.len() > 1 {
        let meta = json!({"shape": "matches", "files": files, "truncated": true, "total": total});
        if serde_json::to_vec(&meta).map_or(usize::MAX, |bytes| bytes.len())
            <= SEARCH_META_MAX_BYTES
        {
            break;
        }
        files.pop();
        truncated = true;
    }
    (files, truncated)
}

fn matches_from_groups(files: &[Value]) -> Vec<Value> {
    files
        .iter()
        .flat_map(|file| {
            let path = file.get("path").and_then(Value::as_str);
            let matches = file.get("matches").and_then(Value::as_array);
            path.into_iter().zip(matches).flat_map(|(path, matches)| {
                matches.iter().filter_map(move |item| Some(json!({"path": path, "lineNumber": item.get("lineNumber")?.as_u64()?, "line": item.get("line")?.as_str()?})))
            })
        })
        .collect()
}

fn format_grep(matches: &[Value], total: usize, truncated: bool) -> String {
    if total == 0 {
        return "No matches found".to_owned();
    }
    let header = if truncated {
        format!("Found {} of {total} matches", matches.len())
    } else {
        format!(
            "Found {total} {}",
            if total == 1 { "match" } else { "matches" }
        )
    };
    let mut groups = Vec::<(String, Vec<String>)>::new();
    for item in matches {
        let (Some(path), Some(line_number), Some(line)) = (
            item.get("path").and_then(Value::as_str),
            item.get("lineNumber").and_then(Value::as_u64),
            item.get("line").and_then(Value::as_str),
        ) else {
            continue;
        };
        let rendered = format!("Line {line_number}: {line}");
        if let Some((_, lines)) = groups.iter_mut().find(|(known, _)| known == path) {
            lines.push(rendered);
        } else {
            groups.push((path.to_owned(), vec![rendered]));
        }
    }
    let body = groups
        .into_iter()
        .map(|(path, lines)| format!("{path}\n{}", lines.join("\n")))
        .collect::<Vec<_>>()
        .join("\n\n");
    if truncated {
        format!("{header}\n\n{body}\n\n(Narrow pattern, path, or include to see more.)")
    } else {
        format!("{header}\n\n{body}")
    }
}

fn source_label(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}

fn format_fetch_output(
    url: &str,
    status_code: u16,
    body: &str,
    provider_truncated: bool,
) -> (String, bool) {
    let header = format!("Fetched {url} (HTTP {status_code})\n\n");
    let source_truncated = body.chars().count() > WEB_FETCH_MAX_OUTPUT_CHARS;
    let body = truncate_char_prefix(body, WEB_FETCH_MAX_OUTPUT_CHARS);
    let prefix = format!("{header}{body}");
    let truncated = provider_truncated
        || source_truncated
        || prefix.chars().count() > WEB_FETCH_MAX_OUTPUT_CHARS;
    if !truncated {
        return (prefix, false);
    }
    let full = format!("{prefix}{WEB_FETCH_TRUNCATION_FOOTER}");
    if full.chars().count() <= WEB_FETCH_MAX_OUTPUT_CHARS {
        return (full, true);
    }
    if WEB_FETCH_MAX_OUTPUT_CHARS < WEB_FETCH_TRUNCATION_FOOTER.chars().count() {
        return (
            truncate_char_prefix(&full, WEB_FETCH_MAX_OUTPUT_CHARS),
            true,
        );
    }
    let prefix_cap = WEB_FETCH_MAX_OUTPUT_CHARS - WEB_FETCH_TRUNCATION_FOOTER.chars().count();
    (
        format!(
            "{}{}",
            truncate_char_prefix(&prefix, prefix_cap),
            WEB_FETCH_TRUNCATION_FOOTER
        ),
        true,
    )
}

fn truncate_char_prefix(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn diff_hunks(path: &str, before: &str, after: &str) -> Vec<Value> {
    if before == after {
        return Vec::new();
    }
    let old = before.split('\n').collect::<Vec<_>>();
    let new = after.split('\n').collect::<Vec<_>>();
    let prefix = old
        .iter()
        .zip(&new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let old_start = prefix.saturating_sub(3);
    let new_start = prefix.saturating_sub(3);
    let old_end = old
        .len()
        .saturating_sub(suffix)
        .saturating_add(3)
        .min(old.len());
    let new_end = new
        .len()
        .saturating_sub(suffix)
        .saturating_add(3)
        .min(new.len());
    let old_text = old[old_start..old_end].join("\n");
    let new_text = new[new_start..new_end].join("\n");
    vec![
        json!({"path": path, "oldText": (!old_text.is_empty()).then_some(old_text), "newText": new_text}),
    ]
}

fn image_type(path: &str) -> Option<ImageMediaType> {
    match Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some(ImageMediaType::Png),
        "jpg" | "jpeg" => Some(ImageMediaType::Jpeg),
        "webp" => Some(ImageMediaType::Webp),
        "gif" => Some(ImageMediaType::Gif),
        _ => None,
    }
}

fn display_path(root: &Path, target: &FsTarget) -> String {
    Path::new(&target.display())
        .strip_prefix(root)
        .ok()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| ".".into())
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_owned()
    } else {
        format!(
            "{}... (line truncated to {max} chars)",
            value.chars().take(max).collect::<String>()
        )
    }
}

fn truncate_utf8(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), TessivumError> {
    if cancellation.is_cancelled() {
        Err(tool_error(
            "CANCELLED",
            "tool call was cancelled",
            Value::Null,
        ))
    } else {
        Ok(())
    }
}

fn check_lease(
    lease: &Option<WorkspaceLease>,
    context: &ToolRunContext,
) -> Result<(), TessivumError> {
    if let Some(lease) = lease {
        lease
            .validate_current()
            .map(|_| ())
            .map_err(|error| workspace_error(context, error))
    } else {
        Ok(())
    }
}

fn session_sandbox_mode(
    sessions: Option<&SessionStore>,
    session_id: &crate::SessionId,
) -> SandboxMode {
    sessions
        .and_then(|sessions| sessions.get(session_id))
        .and_then(|session| {
            session.events().into_iter().rev().find_map(|event| {
                (event.event_type == "sandbox/mode")
                    .then(|| event.data.get("mode").cloned())
                    .flatten()
                    .and_then(|value| serde_json::from_value(value).ok())
            })
        })
        .unwrap_or(SandboxMode::WorkspaceWrite)
}

fn invalid_arguments(message: &str, details: Value) -> TessivumError {
    tool_error("INVALID_TOOL_ARGUMENTS", message, details)
}
fn tool_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "tools", details)
}
fn workspace_error(context: &ToolRunContext, error: WorkspaceError) -> TessivumError {
    TessivumError::new(
        error.code(),
        error.to_string(),
        "tools",
        json!({"sessionId": context.session}),
    )
}
fn attachment_error(error: crate::attachments::AttachmentError) -> TessivumError {
    TessivumError::new(error.code(), error.to_string(), "attachments", Value::Null)
}
