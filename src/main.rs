// JSON-driven build orchestrator. build.json is a flat list of entries; the
// language of each entry is auto-detected from files on disk. The only
// optional `kind` is `"openapi"`, which triggers a pre-wipe + openapi-generator
// pass before the canonical commit/README/push/tag flow.

use buidl::{BuildConfig, RepoKind};
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

    for entry in &cfg {
        let path = Path::new(&entry.path);
        println!("\n==> {}", entry.path);

        // `kind: "openapi"` → seed generator with last tag, wipe (if
        // requested), then spawn openapi-generator before the commit pass.
        if entry.kind.as_deref() == Some("openapi") {
            let data = entry.data.as_ref()
                .unwrap_or_else(|| panic!("openapi entry {} missing `data`", entry.path));
            let templates = data.templates.as_deref()
                .unwrap_or_else(|| panic!("openapi entry {} missing `data.templates`", entry.path));

            // Read the last tag BEFORE wipe (wipe keeps .git/, so tags
            // survive, but it's clearer to grab it first).
            let pre_repo = git2::Repository::open(path)
                .unwrap_or_else(|e| panic!("opening repo at {} (to read last tag): {e}", path.display()));
            let last_tag_raw = buidl::last_tag(&pre_repo);

            if data.delete_all_except_git {
                buidl::wipe_except_git(path);
            }
            buidl::run_openapi_generator_for_templates(&entry.path, templates, &last_tag_raw);
        }

        // Auto-detect language from files. Drives the publish step below.
        let kind = buidl::detect_kind(path);
        println!("[buidl] detected kind: {kind:?}");

        // Canonical "stage → diff → commit-msg → bump → README → commit →
        // push → tag" sequence. Identical for every kind.
        let repo = git2::Repository::open(path)
            .unwrap_or_else(|e| panic!("opening git repo at {}: {e}", path.display()));

        if let Some(remote_url) = &entry.remote {
            buidl::ensure_remote(&repo, remote_url);
        }

        let mut index = buidl::stage_all(&repo);
        let (head_tree, files) = buidl::diff_files(&repo, &index);
        if files.is_empty() {
            println!("ℹ️ No changes to commit. Nothing to release.");
            continue;
        }

        // Commit message uses the full staged diff minus anything matching
        // `commitIgnore.glob` (typically lockfiles or generated noise that
        // bloat the prompt without informing the message). `print_staged`
        // labels each file with whether it reaches the LLM, so the log
        // reflects the actual filtered set, not just the raw inventory.
        let ignore_globs: Option<&[String]> =
            entry.commit_ignore.as_ref().map(|c| c.glob.as_slice());
        buidl::print_staged(&files, ignore_globs);
        let commit_msg = buidl::commit_msg_for_diff(
            &repo, &index, head_tree.as_ref(), &files, ignore_globs,
        );

        let bump = buidl::bump_kind(&commit_msg);
        let new_tag = buidl::compute_new_tag(&repo, bump);

        // For openapi entries, openapi-generator baked `last_tag` into
        // version markers (package.json, README.md, build.gradle.kts).
        // Rewrite those to the NEW tag so HEAD and tag stay in sync —
        // otherwise every subsequent run sees a `LAST → NEW` diff in
        // package.json and bumps again, forever.
        if entry.kind.as_deref() == Some("openapi") {
            buidl::sync_openapi_versions(kind, path, &new_tag);
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)
                .unwrap_or_else(|e| panic!("re-staging after sync_openapi_versions: {e}"));
            index.write()
                .unwrap_or_else(|e| panic!("writing index after sync_openapi_versions: {e}"));
        }

        // README regen is opt-in via the `readme` field. Absent → skip
        // entirely. Present → regen using only files whose path matches one
        // of `readme.glob`. The new README joins this same commit via
        // `index.add_path` inside `regenerate_readme`.
        if let Some(readme_cfg) = &entry.readme {
            buidl::regenerate_readme(&repo, &mut index, &readme_cfg.glob);
        }

        println!("🚀 Publishing {new_tag} to GitHub...");
        println!("{commit_msg}");

        buidl::commit_with_msg(&repo, &mut index, &commit_msg);

        println!("⏳ Pushing code...");
        buidl::git_push_main(path);
        println!("🏷️ Creating tag {new_tag} and pushing...");
        buidl::tag_and_push(&repo, &new_tag, path);

        // Per-kind extra publish step. Swift, Rust, Js, Generic are done —
        // git tag IS the artifact. Android and Kotlin libraries get an
        // additional gradle publish to GitHub Packages.
        match kind {
            RepoKind::Android | RepoKind::Kotlin => {
                buidl::gradle_publish_github_packages(path, &new_tag);
            }
            RepoKind::Swift | RepoKind::Rust | RepoKind::Js | RepoKind::Generic => {
                // Nothing to do — tag IS the artifact.
            }
        }

        // Optional GitHub release tied to the tag we just pushed. Only fires
        // when the canonical flow actually cut a new tag (no-op runs exit at
        // the empty-diff guard above). Asset globs in `release.glob` are
        // expanded relative to the repo dir and uploaded as release assets.
        if let Some(release_cfg) = &entry.release {
            buidl::gh_release_create(path, &new_tag, &release_cfg.glob);
        }

        println!("✅ Successfully pushed {new_tag} to GitHub!");
    }
}
