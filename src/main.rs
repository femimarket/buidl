mod android;
mod commit;

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
    /// tag + `./gradlew publishAndReleaseToMavenCentral`.
    Android,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Commit => commit::run(),
        Cmd::Android => android::run(),
    }
}
