mod android;
mod commit;
mod js_app;
mod swift_app;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "buidl", about = "build pipeline CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print a Conventional Commits message generated from `git diff --cached`.
    Commit,
    /// Auto-release an Android (Kotlin) library: stage + commit + push + patch-bump
    /// tag + `./gradlew publishAllPublicationsToGitHubPackagesRepository`. Detects
    /// openapi-generator-emitted kotlin projects and additionally syncs
    /// build.gradle.kts's `version = "..."` line.
    Android,
    /// Auto-release a Swift Package: stage + commit + push + bump tag. Consumers
    /// resolve via the git tag — no registry publish step.
    SwiftApp,
    /// Auto-release a JS/TS project: stage + commit + push + bump tag. Detects
    /// openapi-generator-emitted typescript-fetch projects and additionally
    /// syncs the package.json + README.md version markers.
    JsApp,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Commit => commit::run(),
        Cmd::Android => android::run(),
        Cmd::SwiftApp => swift_app::run(),
        Cmd::JsApp => js_app::run(),
    }
}
