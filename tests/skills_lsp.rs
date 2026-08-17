#![cfg(unix)]

use std::{
    future::pending,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tessivum::{
    lsp::{
        LspLimits, LspPosition, LspProvider, LspRequest, LspRuntime, StdioLspConfig,
        StdioLspProvider,
    },
    skills::{
        FilesystemSkillProvider, LoadedSkill, SkillInvocationPolicy, SkillListing, SkillLocator,
        SkillPolicyStage, SkillProvider, SkillRuntime,
    },
    TessivumError,
};
use tessivum_core::{CancellationToken, ContextHandle};
use uuid::Uuid;

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("temporary directory creates");
        Self(path)
    }

    fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct StaticSkill {
    listing: SkillListing,
    body: String,
    fail_list: Arc<AtomicBool>,
}

impl StaticSkill {
    fn new(name: &str, body: &str) -> Self {
        Self {
            listing: SkillListing::new(
                name,
                format!("{name} description"),
                format!("locator:{name}"),
                format!("resources:{name}"),
            ),
            body: body.into(),
            fail_list: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl SkillProvider for StaticSkill {
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SkillListing>, TessivumError> {
        if cancellation.is_cancelled() {
            return Err(TessivumError::new(
                "SKILL_CANCELLED",
                "cancelled",
                "skills",
                Value::Null,
            ));
        }
        if self.fail_list.load(Ordering::Acquire) {
            return Err(TessivumError::new(
                "SKILL_LIST_FAILED",
                "list failed",
                "skills",
                Value::Null,
            ));
        }
        Ok(vec![self.listing.clone()])
    }

    async fn get(
        &self,
        locator: &SkillLocator,
        cancellation: CancellationToken,
    ) -> Result<LoadedSkill, TessivumError> {
        if cancellation.is_cancelled() {
            return Err(TessivumError::new(
                "SKILL_CANCELLED",
                "cancelled",
                "skills",
                Value::Null,
            ));
        }
        if locator != &self.listing.locator {
            return Err(TessivumError::new(
                "SKILL_NOT_FOUND",
                "unknown locator",
                "skills",
                Value::Null,
            ));
        }
        Ok(LoadedSkill {
            name: self.listing.name.clone(),
            description: self.listing.description.clone(),
            locator: self.listing.locator.clone(),
            resource_base: self.listing.resource_base.clone(),
            body: self.body.clone(),
            resources: Vec::new(),
        })
    }
}

struct DenyAfterLoad;

#[async_trait]
impl SkillInvocationPolicy for DenyAfterLoad {
    async fn allow(
        &self,
        stage: SkillPolicyStage,
        _listing: &SkillListing,
        _loaded: Option<&LoadedSkill>,
        _cancellation: CancellationToken,
    ) -> Result<bool, TessivumError> {
        Ok(stage != SkillPolicyStage::AfterLoad)
    }
}

#[tokio::test]
async fn skills_catalog_policy_shadow_and_invalidation_are_confined() {
    let root = TempDir::new("skills");
    let project = root.path().join("project");
    let cwd = project.join("nested");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = SkillRuntime::new();
    let global = Arc::new(StaticSkill::new("same", "global body"));
    let local = Arc::new(StaticSkill::new("same", "local body"));
    let unstable = Arc::new(StaticSkill::new("unstable", "cached body"));
    let _global = runtime
        .register_global("global", global.clone(), 100)
        .unwrap();
    let _local = runtime
        .register("local", local.clone(), &project, -100)
        .unwrap();
    let _unstable = runtime
        .register_global("unstable", unstable.clone(), 0)
        .unwrap();
    let catalog = runtime.catalog(&cwd, cancellation()).await.unwrap();
    assert!(catalog.complete);
    assert_eq!(
        catalog
            .skills
            .iter()
            .find(|entry| entry.skill.name == "same")
            .unwrap()
            .provider,
        "local"
    );
    assert_eq!(
        runtime
            .get(&cwd, "same", cancellation())
            .await
            .unwrap()
            .body,
        "local body"
    );
    assert_eq!(
        runtime
            .invoke(&cwd, "same", &DenyAfterLoad, cancellation())
            .await
            .unwrap_err()
            .code,
        "SKILL_DENIED"
    );
    unstable.fail_list.store(true, Ordering::Release);
    let stale = runtime.catalog(&cwd, cancellation()).await.unwrap();
    assert!(!stale.complete);
    assert!(stale
        .skills
        .iter()
        .any(|entry| entry.skill.name == "unstable"));
    let before = stale.revision;
    let after = runtime.invalidate(Some("unstable")).unwrap();
    assert!(after > before);
    let invalidated = runtime.catalog(&cwd, cancellation()).await.unwrap();
    assert!(!invalidated.complete);
    assert!(!invalidated
        .skills
        .iter()
        .any(|entry| entry.skill.name == "unstable"));
}

#[tokio::test]
async fn filesystem_skill_provider_catalogs_and_confines_resources() {
    let root = TempDir::new("filesystem-skill");
    let skill = root.path().join("demo");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill\n---\nBody <safe>\n",
    )
    .unwrap();
    std::fs::write(skill.join("guide.txt"), "resource body").unwrap();
    let runtime = SkillRuntime::new();
    let provider = FilesystemSkillProvider::from_root(root.path()).unwrap();
    let _registration = runtime
        .register_global("filesystem", Arc::new(provider), 0)
        .unwrap();
    let catalog = runtime.catalog(root.path(), cancellation()).await.unwrap();
    assert_eq!(catalog.skills.len(), 1);
    assert_eq!(catalog.skills[0].skill.name, "demo");
    let loaded = runtime
        .get(root.path(), "demo", cancellation())
        .await
        .unwrap();
    assert_eq!(loaded.body, "Body <safe>\n");
    assert_eq!(loaded.resources[0].path, "guide.txt");
    assert_eq!(
        runtime
            .read_resource(root.path(), "demo", "guide.txt", cancellation())
            .await
            .unwrap()
            .text,
        "resource body"
    );
    assert_eq!(
        runtime
            .read_resource(root.path(), "demo", "../outside", cancellation())
            .await
            .unwrap_err()
            .code,
        "INVALID_SKILL_RESOURCE_PATH"
    );
}

#[derive(Default)]
struct RecordingLsp {
    result: Value,
    calls: Mutex<Vec<LspRequest>>,
    disposed: AtomicBool,
}

impl RecordingLsp {
    fn with_result(result: Value) -> Self {
        Self {
            result,
            ..Self::default()
        }
    }
}

#[async_trait]
impl LspProvider for RecordingLsp {
    async fn request(
        &self,
        request: LspRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        if cancellation.is_cancelled() {
            return Err(TessivumError::new(
                "LSP_CANCELLED",
                "cancelled",
                "lsp",
                Value::Null,
            ));
        }
        self.calls.lock().push(request);
        Ok(self.result.clone())
    }

    async fn dispose(&self) -> Result<(), TessivumError> {
        self.disposed.store(true, Ordering::Release);
        Ok(())
    }
}

struct NeverLsp;

#[async_trait]
impl LspProvider for NeverLsp {
    async fn request(
        &self,
        _request: LspRequest,
        _cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        pending().await
    }
}

#[tokio::test]
async fn lsp_registry_selects_extensions_and_enforces_closed_bounds() {
    let rust = Arc::new(RecordingLsp::with_result(json!({"provider": "rust"})));
    let python = Arc::new(RecordingLsp::with_result(json!({"provider": "python"})));
    let runtime = LspRuntime::new();
    let _rust = runtime.register("rust", [".rs"], rust.clone()).unwrap();
    let _python = runtime.register("python", ["py"], python.clone()).unwrap();
    assert_eq!(
        runtime
            .definition(
                "lib.RS",
                LspPosition {
                    line: 2,
                    character: 3
                },
                cancellation()
            )
            .await
            .unwrap(),
        json!({"provider": "rust"})
    );
    assert_eq!(
        runtime
            .definition(
                "main.py",
                LspPosition {
                    line: 0,
                    character: 0
                },
                cancellation()
            )
            .await
            .unwrap(),
        json!({"provider": "python"})
    );
    assert_eq!(rust.calls.lock().len(), 1);
    assert_eq!(
        runtime
            .register("partial", ["go", "rs"], Arc::new(RecordingLsp::default()))
            .unwrap_err()
            .code,
        "DUPLICATE_LSP_EXTENSION"
    );
    assert_eq!(
        runtime
            .definition(
                "main.go",
                LspPosition {
                    line: 0,
                    character: 0
                },
                cancellation()
            )
            .await
            .unwrap_err()
            .code,
        "LSP_UNAVAILABLE"
    );
    let position = LspPosition::from_utf8(4, "a😀b", "a😀".len()).unwrap();
    assert_eq!(
        position,
        LspPosition {
            line: 4,
            character: 3
        }
    );
    assert_eq!(
        LspPosition::from_utf8(0, "😀", 1).unwrap_err().code,
        "INVALID_LSP_POSITION"
    );
    runtime.dispose().await.unwrap();
    assert!(rust.disposed.load(Ordering::Acquire));
    assert!(python.disposed.load(Ordering::Acquire));
    assert_eq!(
        runtime
            .definition(
                "lib.rs",
                LspPosition {
                    line: 0,
                    character: 0
                },
                cancellation()
            )
            .await
            .unwrap_err()
            .code,
        "LSP_CLOSED"
    );
    let timeout = LspRuntime::with_limits(LspLimits {
        request_timeout: Duration::from_millis(10),
        max_result_bytes: 1024,
    })
    .unwrap();
    let _never = timeout
        .register("never", ["rs"], Arc::new(NeverLsp))
        .unwrap();
    assert_eq!(
        timeout
            .definition(
                "main.rs",
                LspPosition {
                    line: 0,
                    character: 0
                },
                cancellation()
            )
            .await
            .unwrap_err()
            .code,
        "LSP_TIMEOUT"
    );
    let bounded = LspRuntime::with_limits(LspLimits {
        request_timeout: Duration::from_secs(1),
        max_result_bytes: 4,
    })
    .unwrap();
    let _bounded = bounded
        .register(
            "bounded",
            ["rs"],
            Arc::new(RecordingLsp::with_result(json!({"long": true}))),
        )
        .unwrap();
    assert_eq!(
        bounded
            .definition(
                "main.rs",
                LspPosition {
                    line: 0,
                    character: 0
                },
                cancellation()
            )
            .await
            .unwrap_err()
            .code,
        "LSP_RESULT_LIMIT"
    );
    let cancelled = cancellation();
    assert!(cancelled.cancel());
    assert_eq!(
        bounded
            .definition(
                "main.rs",
                LspPosition {
                    line: 0,
                    character: 0
                },
                cancelled
            )
            .await
            .unwrap_err()
            .code,
        "LSP_CANCELLED"
    );
}

#[tokio::test]
async fn stdio_lsp_initializes_defines_confines_and_exits_on_dispose() {
    let workspace = TempDir::new("stdio-lsp");
    let source = workspace.path().join("unicode.rs");
    let outside = workspace
        .path()
        .parent()
        .unwrap()
        .join(format!("outside-{}.rs", Uuid::new_v4()));
    let exited = workspace.path().join("exited");
    std::fs::write(&source, "a😀b\n").unwrap();
    std::fs::write(&outside, "outside\n").unwrap();
    let script = r#"import json
import sys
marker = __MARKER__
def receive():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b'\r\n', b'\n'):
            break
        name, value = line.decode().split(':', 1)
        if name.lower() == 'content-length':
            length = int(value.strip())
    return json.loads(sys.stdin.buffer.read(length))
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode()
    sys.stdout.buffer.write(b'Content-Length: ' + str(len(body)).encode() + b'\r\n\r\n' + body)
    sys.stdout.buffer.flush()
while True:
    request = receive()
    if request is None:
        break
    method = request.get('method')
    if method == 'initialize':
        send({'jsonrpc': '2.0', 'id': request['id'], 'result': {'capabilities': {'positionEncoding': 'utf-16'}}})
    elif method == 'textDocument/definition':
        send({'jsonrpc': '2.0', 'id': request['id'], 'result': {'character': request['params']['position']['character']}})
    elif method == 'shutdown':
        send({'jsonrpc': '2.0', 'id': request['id'], 'result': None})
    elif method == 'exit':
        open(marker, 'w').write('exited')
        break
"#.replace("__MARKER__", &serde_json::to_string(&exited.to_string_lossy()).unwrap());
    let mut config = StdioLspConfig::new("python3", workspace.path());
    config.args = vec!["-u".into(), "-c".into(), script];
    config.request_timeout = Duration::from_secs(2);
    let provider = StdioLspProvider::spawn(config).await.unwrap();
    let runtime = LspRuntime::new();
    let _registration = runtime
        .register("stdio", ["rs"], Arc::new(provider.clone()))
        .unwrap();
    let position = LspPosition::from_utf8(0, "a😀b", "a😀".len()).unwrap();
    assert_eq!(
        runtime
            .definition(&source, position, cancellation())
            .await
            .unwrap(),
        json!({"character": 3})
    );
    assert_eq!(
        provider
            .request(
                LspRequest::definition(
                    &outside,
                    LspPosition {
                        line: 0,
                        character: 0
                    }
                ),
                cancellation()
            )
            .await
            .unwrap_err()
            .code,
        "LSP_WORKSPACE_DENIED"
    );
    runtime.dispose().await.unwrap();
    assert!(provider.is_closed());
    assert_eq!(std::fs::read_to_string(exited).unwrap(), "exited");
    let _ = std::fs::remove_file(outside);
}
