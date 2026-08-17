use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use serde::Serialize;
use tessivum::plugins::{
    CompatibilityClass, CompatibilityReport, PluginDiagnostic, PluginRouter, PluginRuntime,
};

#[derive(Debug, Parser)]
#[command(
    name = "plugin-report",
    about = "Inspect Tessivum plugin compatibility without loading code"
)]
struct Args {
    /// Package directories, manifests, or .wasm artifacts to inspect.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Explicit entry runtime. This overrides detection and is validated without fallback.
    #[arg(long)]
    runtime: Option<PluginRuntime>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    reports: Vec<CompatibilityReport>,
    diagnostics: Vec<PluginDiagnostic>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let router = PluginRouter::new();
    let mut reports = Vec::with_capacity(args.paths.len());
    let mut diagnostics = Vec::new();

    for path in args.paths {
        match router.inspect(path, args.runtime) {
            Ok(report) => {
                if report.compatibility == CompatibilityClass::Unsupported {
                    diagnostics.push(PluginDiagnostic {
                        code: "PLUGIN_UNSUPPORTED".into(),
                        message: report.reasons.last().cloned().unwrap_or_else(|| "plugin is unsupported".into()),
                        help: "Publish a browser half with dsh.client or remove the unsupported dependency.".into(),
                    });
                }
                reports.push(report);
            }
            Err(error) => diagnostics.push(error.diagnostic()),
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            reports,
            diagnostics: diagnostics.clone(),
        })
        .expect("report is serializable")
    );
    if diagnostics.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
