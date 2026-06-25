// JSON-driven build orchestrator. Reads a build.json describing repos to
// release (currently only `swiftapps[]`), and runs the canonical commit →
// README regen → push → tag flow per entry. `kind: "openapi"` entries also
// pre-wipe the output dir and spawn openapi-generator before the commit pass.
//
// Per-language modules (swift_app.rs / android.rs / js_app.rs / commit.rs)
// are kept on disk as reference but no longer wired into the build — the
// canonical flow lives here, using helpers from lib.rs.

use buidl::{BuildConfig, SPEC};
use clap::Parser;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "buidl", about = "JSON-driven build orchestrator")]
struct Cli {
    /// Path to the build.json describing the pipeline.
    #[arg(long, default_value = "build.json")]
    config: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    let raw = std::fs::read_to_string(&cli.config)
        .unwrap_or_else(|e| panic!("reading {}: {e}", cli.config.display()));
    let cfg: BuildConfig = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("parsing {}: {e}", cli.config.display()));

    for entry in &cfg.swiftapps {
        let path = Path::new(&entry.path);
        println!("\n==> {}", entry.path);

        // `kind: "openapi"` → wipe (if requested) + spawn openapi-generator
        // before the commit pass so leftover files from a prior spec don't
        // survive and the regen output matches the live spec.
        if entry.kind.as_deref() == Some("openapi") {
            let data = entry.data.as_ref()
                .unwrap_or_else(|| panic!("openapi entry {} missing `data`", entry.path));
            let templates = data.templates.as_deref()
                .unwrap_or_else(|| panic!("openapi entry {} missing `data.templates`", entry.path));

            if data.delete_all_except_git {
                buidl::wipe_except_git(path);
            }

            // Hardcoded swift6 properties — same as the old build.sh swift
            // loop. Lives here, not in lib.rs, because each language flavor
            // has its own flag set. Move to build.json `data.*` once a
            // second flavor lands.
            buidl::run_openapi_generator(&[
                "-g", "swift6",
                "-i", SPEC,
                "-o", &entry.path,
                "-t", templates,
                "--additional-properties", "projectName=Api,responseAs=AsyncAwait",
            ]);
        }

        // Canonical "stage → diff → commit-msg → bump → README → commit →
        // push → tag" sequence. Identical for every swift entry regardless of
        // kind. Skips cleanly when nothing changed OR when the only changes
        // were ignored/binary churn (pushing a "chore: update" tag for that
        // case was always wrong).
        let repo = git2::Repository::open(path)
            .unwrap_or_else(|e| panic!("opening git repo at {}: {e}", path.display()));

        let mut index = buidl::stage_all(&repo);
        let (head_tree, files) = buidl::diff_files(&repo, &index);
        if files.is_empty() {
            println!("ℹ️ No changes to commit. Nothing to release.");
            continue;
        }

        buidl::print_staged(&files);
        let commit_msg = buidl::commit_msg_for_diff(&repo, &index, head_tree.as_ref(), &files);

        let kind = buidl::bump_kind(&commit_msg);
        let new_tag = buidl::compute_new_tag(&repo, kind);

        // README regen reads the whole repo, rewrites README.md, re-stages
        // it into the same index. Joined with this same commit by
        // `commit_with_msg` below.
        buidl::regenerate_readme(&mut index);

        println!("🚀 Publishing {new_tag} to GitHub...");
        println!("{commit_msg}");

        buidl::commit_with_msg(&repo, &mut index, &commit_msg);

        println!("⏳ Pushing code...");
        buidl::git_push_main(path);
        println!("🏷️ Creating tag {new_tag} and pushing...");
        buidl::tag_and_push(&repo, &new_tag, path);

        println!("✅ Successfully pushed {new_tag} to GitHub!");
    }
}
