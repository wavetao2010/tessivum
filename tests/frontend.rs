use std::{
    fs,
    path::{Path, PathBuf},
};

use axum::{
    body::Body,
    http::{header, Method, Response, StatusCode},
};
use http_body_util::BodyExt;
use tessivum::frontend::{FrontendError, FrontendHtmlTap, FrontendStatic};
use uuid::Uuid;

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-frontend-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, path: &str, body: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn body(response: Response<Body>) -> String {
    String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap()
}

fn dist() -> Fixture {
    let fixture = Fixture::new("dist");
    fixture.write(
        "index.html",
        "<html><head><title>app</title></head><body>app</body></html>",
    );
    fixture
}

fn client_manifest(name: &str, id: &str, display_name: &str) -> String {
    format!(
        r#"{{"name":"{name}","exports":{{"./client":"./dist/client.js"}},"dsh":{{"client":{{"platform":"web","id":"{id}","name":"{display_name}","inject":["logger"],"immediately":true}}}}}}"#
    )
}

#[tokio::test]
async fn static_responses_have_exact_statuses_mime_head_and_spa_behavior() {
    let dist = dist();
    dist.write("app.js", "console.log('app')");
    dist.write("blob.unknown", "opaque");
    let frontend = FrontendStatic::new(dist.path()).unwrap();

    let app = frontend.serve(Method::GET, "/app.js");
    assert_eq!(app.status(), StatusCode::OK);
    assert_eq!(app.headers()[header::CONTENT_TYPE], "text/javascript");
    assert_eq!(body(app).await, "console.log('app')");

    let unknown = frontend.serve(Method::GET, "/blob.unknown");
    assert_eq!(unknown.status(), StatusCode::OK);
    assert_eq!(
        unknown.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );

    let traversal = frontend.serve(Method::GET, "/%2e%2e/secret.txt");
    assert_eq!(traversal.status(), StatusCode::FORBIDDEN);

    let missing = frontend.serve(Method::GET, "/a/react/route");
    assert_eq!(missing.status(), StatusCode::OK);
    assert!(body(missing).await.contains("window.__DSH_BOOT__"));

    let head = frontend.serve(Method::HEAD, "/app.js");
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::CONTENT_TYPE], "text/javascript");
    assert_eq!(head.headers()[header::CONTENT_LENGTH], "18");
    assert!(body(head).await.is_empty());

    assert_eq!(
        frontend.serve(Method::POST, "/app.js").status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn packages_produce_a_deterministic_hashed_graph_and_exact_plugin_route() {
    let dist = dist();
    let packages = Fixture::new("packages");
    packages.write(
        "alpha/package.json",
        &client_manifest("alpha-package", "alpha", "Alpha"),
    );
    packages.write("alpha/dist/client.js", "export const alpha = 1");
    packages.write(
        "beta/package.json",
        &client_manifest("beta-package", "beta", "Beta"),
    );
    packages.write("beta/dist/client.js", "export const beta = 1");
    let frontend = FrontendStatic::new(dist.path()).unwrap();

    let alpha = packages.path().join("alpha");
    let beta = packages.path().join("beta");
    let graph = frontend
        .scan_packages([beta.as_path(), alpha.as_path()])
        .unwrap();
    assert_eq!(
        graph
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    let entry = &graph.entries[0];
    assert_eq!(entry.package, "alpha-package");
    assert_eq!(entry.inject, Some(vec![String::from("logger")]));
    assert_eq!(entry.name, "Alpha");
    assert_eq!(entry.url, "/plugins/alpha/client.js");
    assert_eq!(entry.immediately, Some(true));
    assert!(entry.rev.starts_with("sha256:"));

    let reordering = frontend
        .scan_packages([alpha.as_path(), beta.as_path()])
        .unwrap();
    assert_eq!(graph, reordering);

    let plugin = frontend.serve(Method::GET, "/plugins/alpha/client.js");
    assert_eq!(plugin.status(), StatusCode::OK);
    assert_eq!(plugin.headers()[header::CONTENT_TYPE], "text/javascript");
    assert_eq!(plugin.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(
        plugin.headers()[header::ETAG],
        format!("\"{}\"", graph.entries[0].rev)
    );
    assert_eq!(body(plugin).await, "export const alpha = 1");
    assert_eq!(
        frontend
            .serve(Method::GET, "/plugins/missing/client.js")
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        frontend
            .serve(Method::GET, "/plugins/missing/other.js")
            .status(),
        StatusCode::NOT_FOUND
    );

    packages.write("alpha/dist/client.js", "export const alpha = 2");
    let rebuilt = frontend.rebuild().unwrap();
    assert_ne!(rebuilt.rev, graph.rev);
    assert_ne!(rebuilt.entries[0].rev, graph.entries[0].rev);
}

#[test]
fn malformed_web_rows_are_aggregated_without_replacing_the_live_graph() {
    let dist = dist();
    let packages = Fixture::new("invalid-packages");
    packages.write(
        "missing-export/package.json",
        r#"{"name":"missing-export","dsh":{"client":{"platform":"web"}}}"#,
    );
    packages.write("bad-client/package.json", r#"{"dsh":{"client":[]}}"#);
    let frontend = FrontendStatic::new(dist.path()).unwrap();
    let error = frontend.scan_packages([packages.path()]).unwrap_err();
    let FrontendError::InvalidPackageManifests { errors } = error else {
        panic!("activation must aggregate manifest errors");
    };
    assert_eq!(errors.len(), 2);
    assert!(errors.iter().any(|error| error.message.contains("exports")));
    assert!(errors
        .iter()
        .any(|error| error.message.contains("dsh.client")));
    assert!(frontend.graph().entries.is_empty());
}

#[test]
fn ordered_taps_precede_nothing_before_escaped_boot_script() {
    let dist = dist();
    let packages = Fixture::new("escaped-boot");
    packages.write(
        "plugin/package.json",
        &client_manifest("evil-package", "evil", "<evil"),
    );
    packages.write("plugin/dist/client.js", "export {};");
    let frontend = FrontendStatic::new(dist.path()).unwrap();
    frontend.scan_packages([packages.path()]).unwrap();
    let _second = frontend
        .register_tap(FrontendHtmlTap::new("second", 20, |html| {
            html.replace("</head>", "<meta id=\"second\"></head>")
        }))
        .unwrap();
    let _first = frontend
        .register_tap(FrontendHtmlTap::new("first", 10, |html| {
            html.replace("</head>", "<meta id=\"first\"></head>")
        }))
        .unwrap();

    let rendered = frontend.render_index().unwrap();
    let script = rendered.find("<script>window.__DSH_BOOT__=").unwrap();
    let first = rendered.find("id=\"first\"").unwrap();
    let second = rendered.find("id=\"second\"").unwrap();
    assert!(script < first && first < second);
    assert!(!rendered.contains("<evil"));
    assert!(rendered.contains("\\u003cevil"));
}

#[tokio::test]
async fn hmr_is_opt_in_bounded_and_publishes_rebuilt_graphs() {
    let dist = dist();
    for capacity in [0, 65] {
        assert!(matches!(
            FrontendStatic::new_with_hmr(dist.path(), capacity),
            Err(FrontendError::InvalidHmrQueue)
        ));
    }

    let packages = Fixture::new("hmr");
    packages.write(
        "plugin/package.json",
        &client_manifest("hmr-package", "hmr", "HMR"),
    );
    packages.write("plugin/dist/client.js", "export const version = 1");
    let frontend = FrontendStatic::new_with_hmr(dist.path(), 1).unwrap();
    let mut updates = frontend
        .subscribe_hmr()
        .expect("development HMR subscribes");

    let first = frontend.scan_packages([packages.path()]).unwrap();
    packages.write("plugin/dist/client.js", "export const version = 2");
    let rebuilt = frontend.rebuild().unwrap();
    assert!(matches!(
        updates.recv().await,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(1))
    ));
    assert_eq!(updates.recv().await.unwrap().graph, rebuilt);
    assert_ne!(first.rev, rebuilt.rev);
    assert!(FrontendStatic::new(dist.path())
        .unwrap()
        .subscribe_hmr()
        .is_none());
}

#[tokio::test]
async fn conditional_client_exports_ignore_types_and_prefer_browser_targets() {
    let dist = dist();
    let packages = Fixture::new("conditional-exports");
    packages.write(
        "published/package.json",
        r#"{"name":"published","exports":{"./client":{"types":"./dist/client.d.ts","default":"./dist/client.js"}},"dsh":{"client":{"platform":"web"}}}"#,
    );
    packages.write("published/dist/client.js", "export const published = true");
    packages.write(
        "priority/package.json",
        r#"{"name":"priority","exports":{"./client":{"default":"./dist/default.js","import":"./dist/import.js","browser":"./dist/browser.js"}},"dsh":{"client":{"platform":"web"}}}"#,
    );
    packages.write(
        "priority/dist/default.js",
        "export const target = 'default'",
    );
    packages.write("priority/dist/import.js", "export const target = 'import'");
    packages.write(
        "priority/dist/browser.js",
        "export const target = 'browser'",
    );
    let frontend = FrontendStatic::new(dist.path()).unwrap();

    let graph = frontend.scan_packages([packages.path()]).unwrap();
    assert_eq!(
        graph
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["priority", "published"]
    );
    assert_eq!(
        body(frontend.serve(Method::GET, "/plugins/published/client.js")).await,
        "export const published = true"
    );
    assert_eq!(
        body(frontend.serve(Method::GET, "/plugins/priority/client.js")).await,
        "export const target = 'browser'"
    );
}
