//! Filesystem-touching tests. Pure functions are unit-tested in `lib.rs`;
//! everything here either sets up a temp dir of files (for `detect_kind`,
//! `wipe_except_git`) or initializes a real git2 repository (for `last_tag`
//! and `compute_new_tag`).
//!
//! All tests run in their own `tempfile::TempDir` and clean up on drop.

use std::fs;
use std::path::Path;
use tempfile::TempDir;

use buidl::{
    BuildConfig, DEFAULT_VERSION, RepoKind, commit_msg_for_diff, compute_new_tag, detect_kind,
    last_tag, regenerate_readme, wipe_except_git,
};

// ── build.json deserialization ─────────────────────────────────────────────

#[test]
fn build_config_parses_minimal_entry() {
    let cfg: BuildConfig = serde_json::from_str(
        r#"[{"path":"/foo/bar"}]"#
    ).unwrap();
    assert_eq!(cfg.len(), 1);
    assert_eq!(cfg[0].path, "/foo/bar");
    assert_eq!(cfg[0].remote, None);
    assert_eq!(cfg[0].kind, None);
    assert!(cfg[0].data.is_none());
    assert!(cfg[0].readme.is_none());
}

#[test]
fn build_config_parses_readme_with_glob_list() {
    let cfg: BuildConfig = serde_json::from_str(
        r#"[{"path":"/foo","readme":{"glob":["Prod/*.swift","*.kt"]}}]"#
    ).unwrap();
    let cfg_readme = cfg[0].readme.as_ref().expect("readme field");
    assert_eq!(cfg_readme.glob, vec!["Prod/*.swift".to_string(), "*.kt".to_string()]);
}

#[test]
fn build_config_parses_readme_with_empty_glob() {
    // Degenerate but valid — opts into README regen but matches nothing.
    let cfg: BuildConfig = serde_json::from_str(
        r#"[{"path":"/foo","readme":{"glob":[]}}]"#
    ).unwrap();
    assert!(cfg[0].readme.is_some());
    assert_eq!(cfg[0].readme.as_ref().unwrap().glob.len(), 0);
}

#[test]
fn build_config_parses_commit_ignore_camel_case() {
    // Field name in JSON is camelCase `commitIgnore`; struct field is
    // snake_case `commit_ignore` via #[serde(rename)].
    let cfg: BuildConfig = serde_json::from_str(
        r#"[{"path":"/foo","commitIgnore":{"glob":["*.lock","dist/**"]}}]"#
    ).unwrap();
    let ci = cfg[0].commit_ignore.as_ref().expect("commitIgnore field");
    assert_eq!(ci.glob, vec!["*.lock".to_string(), "dist/**".to_string()]);
}

#[test]
fn build_config_parses_full_buidl_entry_shape() {
    // The exact shape the user added for the buidl repo — both readme.glob
    // and commitIgnore.glob present.
    let cfg: BuildConfig = serde_json::from_str(r#"
        [{
            "path":"/Users/u/rustapps/buidl",
            "remote":"https://github.com/femimarket/buidl",
            "readme":{"glob":["*.rs","*.toml"]},
            "commitIgnore":{"glob":["*.lock"]}
        }]
    "#).unwrap();
    assert_eq!(cfg[0].path, "/Users/u/rustapps/buidl");
    assert_eq!(cfg[0].readme.as_ref().unwrap().glob, vec!["*.rs".to_string(), "*.toml".to_string()]);
    assert_eq!(cfg[0].commit_ignore.as_ref().unwrap().glob, vec!["*.lock".to_string()]);
}

#[test]
fn build_config_parses_release_with_glob() {
    // The exact shape the user wants for SwiftLyricEditor: optional release
    // field with asset globs that get uploaded to the just-pushed tag.
    let cfg: BuildConfig = serde_json::from_str(r#"
        [{
            "path":"/Users/u/swiftapps/LyricEditor",
            "remote":"https://github.com/femimarket/SwiftLyricEditor",
            "readme":{"glob":["*.swift"]},
            "release":{"glob":["qwen3-aligner-0.6b"]}
        }]
    "#).unwrap();
    let rel = cfg[0].release.as_ref().expect("release field parsed");
    assert_eq!(rel.glob, vec!["qwen3-aligner-0.6b".to_string()]);
}

#[test]
fn build_config_release_absent_yields_none() {
    let cfg: BuildConfig = serde_json::from_str(r#"[{"path":"/foo"}]"#).unwrap();
    assert!(cfg[0].release.is_none());
}

#[test]
fn build_config_parses_release_with_multiple_asset_globs() {
    let cfg: BuildConfig = serde_json::from_str(r#"
        [{"path":"/foo","release":{"glob":["dist/*.bin","artifacts/*.tar.gz"]}}]
    "#).unwrap();
    let rel = cfg[0].release.as_ref().unwrap();
    assert_eq!(rel.glob, vec!["dist/*.bin".to_string(), "artifacts/*.tar.gz".to_string()]);
}

#[test]
fn build_config_parses_openapi_entry_full_shape() {
    let cfg: BuildConfig = serde_json::from_str(r#"
        [{
            "path":"/foo",
            "remote":"https://github.com/x/y.git",
            "kind":"openapi",
            "data":{"templates":"/tpl/swift6","deleteAllExceptGit":true}
        }]
    "#).unwrap();
    assert_eq!(cfg[0].path, "/foo");
    assert_eq!(cfg[0].remote.as_deref(), Some("https://github.com/x/y.git"));
    assert_eq!(cfg[0].kind.as_deref(), Some("openapi"));
    let data = cfg[0].data.as_ref().unwrap();
    assert_eq!(data.templates.as_deref(), Some("/tpl/swift6"));
    assert!(data.delete_all_except_git);
}

// ── detect_kind ────────────────────────────────────────────────────────────

fn touch(dir: &Path, rel: &str, contents: &str) {
    let abs = dir.join(rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(abs, contents).unwrap();
}

#[test]
fn detect_rust_via_cargo_toml() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Rust);
}

#[test]
fn detect_swift_via_package_swift() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "Package.swift", "// swift-tools-version:5.5\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Swift);
}

#[test]
fn detect_swift_via_xcodeproj_dir() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("MyApp.xcodeproj")).unwrap();
    touch(tmp.path(), "MyApp.xcodeproj/project.pbxproj", "// !$*UTF8*$!\n{}\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Swift);
}

#[test]
fn detect_android_via_root_gradle_application_plugin() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "build.gradle.kts",
          "plugins { id(\"com.android.application\") version \"8.7.0\" }\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Android);
}

#[test]
fn detect_android_via_app_subdir_gradle() {
    // Standard android layout: root build.gradle.kts has nothing identifying,
    // but app/build.gradle.kts applies com.android.application.
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "build.gradle.kts", "// root project\n");
    touch(tmp.path(), "app/build.gradle.kts",
          "plugins { id(\"com.android.application\") }\nandroid {}\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Android);
}

#[test]
fn detect_android_via_library_plugin() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "build.gradle.kts",
          "plugins { id(\"com.android.library\") }\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Android);
}

#[test]
fn detect_kotlin_via_multiplatform_plugin() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "build.gradle.kts",
          "plugins { kotlin(\"multiplatform\") version \"2.2.20\" }\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Kotlin);
}

#[test]
fn detect_js_via_package_json() {
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "package.json", "{\"name\":\"x\",\"version\":\"0.0.1\"}\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Js);
}

#[test]
fn detect_generic_when_no_signature_matches() {
    let tmp = TempDir::new().unwrap();
    // ComfyUI-shaped repo: workflow.json + python.
    touch(tmp.path(), "workflow.json", "{\"nodes\":[]}\n");
    touch(tmp.path(), "main.py", "print('hi')\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Generic);
}

#[test]
fn detect_generic_for_empty_dir() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(detect_kind(tmp.path()), RepoKind::Generic);
}

#[test]
fn detect_priority_rust_beats_js_in_polyglot() {
    // detect_kind checks Cargo.toml first — a polyglot repo with both Rust
    // and Node sources is classified as Rust. Documents intentional ordering.
    let tmp = TempDir::new().unwrap();
    touch(tmp.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
    touch(tmp.path(), "package.json", "{}\n");
    assert_eq!(detect_kind(tmp.path()), RepoKind::Rust);
}

// ── wipe_except_git ────────────────────────────────────────────────────────

#[test]
fn wipe_keeps_dot_git_and_removes_everything_else() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join(".git/refs")).unwrap();
    touch(tmp.path(), ".git/HEAD", "ref: refs/heads/main\n");
    touch(tmp.path(), "Cargo.toml", "[package]\n");
    touch(tmp.path(), "src/main.rs", "fn main(){}\n");
    fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
    touch(tmp.path(), "target/debug/marker", "x\n");

    wipe_except_git(tmp.path());

    let remaining: Vec<String> = fs::read_dir(tmp.path()).unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(remaining, vec![".git"]);
    assert!(tmp.path().join(".git/HEAD").is_file(), ".git contents preserved");
}

// ── git-backed: last_tag + compute_new_tag ─────────────────────────────────

/// Init a repo with a single initial commit + configurable list of tags.
fn init_repo_with_tags(tags: &[&str]) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let repo = git2::Repository::init(tmp.path()).unwrap();
    // Need an identity for commits.
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.email", "test@buidl.local").unwrap();
    cfg.set_str("user.name", "buidl-test").unwrap();

    // Create an initial commit (tags need something to point at).
    touch(tmp.path(), "README.md", "init\n");
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let sig = repo.signature().unwrap();
    let commit_oid = repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    let commit_obj = repo.find_object(commit_oid, None).unwrap();

    for tag in tags {
        repo.tag_lightweight(tag, &commit_obj, false).unwrap();
    }
    tmp
}

#[test]
fn last_tag_falls_back_to_default_when_no_tags_exist() {
    let tmp = init_repo_with_tags(&[]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    let stripped = DEFAULT_VERSION.strip_prefix('v').unwrap();
    assert_eq!(last_tag(&repo), stripped);
}

#[test]
fn last_tag_returns_bare_semver_for_v_prefixed_tag() {
    let tmp = init_repo_with_tags(&["v1.2.3"]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    assert_eq!(last_tag(&repo), "1.2.3");
}

#[test]
fn last_tag_accepts_both_v_prefixed_and_bare_semver() {
    // Mixed conventions in the wild (the swift API repo had both `v0.7.0`
    // and a bare `1.0.0` left over from earlier tooling).
    let tmp = init_repo_with_tags(&["v0.5.0", "1.0.0", "v0.9.0"]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    // Max by semver tuple: (1,0,0) > (0,9,0) > (0,5,0).
    assert_eq!(last_tag(&repo), "1.0.0");
}

#[test]
fn last_tag_skips_non_semver_tags_silently() {
    // `release-candidate-1` and `latest` aren't semver — they should be
    // filtered out, and the remaining `v0.3.0` should win.
    let tmp = init_repo_with_tags(&["release-candidate-1", "latest", "v0.3.0"]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    assert_eq!(last_tag(&repo), "0.3.0");
}

#[test]
fn compute_new_tag_uses_default_when_no_tags() {
    let tmp = init_repo_with_tags(&[]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    assert_eq!(compute_new_tag(&repo, "patch"), DEFAULT_VERSION);
    assert_eq!(compute_new_tag(&repo, "minor"), DEFAULT_VERSION);
    assert_eq!(compute_new_tag(&repo, "major"), DEFAULT_VERSION);
}

#[test]
fn compute_new_tag_patch_bump() {
    let tmp = init_repo_with_tags(&["v1.2.3"]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    assert_eq!(compute_new_tag(&repo, "patch"), "v1.2.4");
}

#[test]
fn compute_new_tag_minor_bump_resets_patch() {
    let tmp = init_repo_with_tags(&["v1.2.3"]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    assert_eq!(compute_new_tag(&repo, "minor"), "v1.3.0");
}

#[test]
fn compute_new_tag_major_bump_resets_minor_and_patch() {
    let tmp = init_repo_with_tags(&["v1.2.3"]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    assert_eq!(compute_new_tag(&repo, "major"), "v2.0.0");
}

#[test]
fn compute_new_tag_picks_max_across_unsorted_tags() {
    let tmp = init_repo_with_tags(&["v0.1.0", "v0.10.0", "v0.2.0"]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    // v0.10.0 is the max semver, not v0.2.0 (string-sort would mis-rank).
    assert_eq!(compute_new_tag(&repo, "patch"), "v0.10.1");
}

// ── semantic guards: empty filter must NOT silently invert into "include all"
// ───────────────────────────────────────────────────────────────────────────
//
// These tests deliberately do NOT mock LM Studio. If the early-return guards
// in `commit_msg_for_diff` and `regenerate_readme` are removed or broken, the
// functions will attempt an HTTP call to `localhost:1234` and panic with a
// connection error — which is what we want: the test fails loudly to surface
// the regression. When the guards work, no HTTP call is attempted and the
// assertions pass without LM Studio being present.

#[test]
fn commit_msg_returns_chore_update_when_commit_ignore_covers_everything() {
    // Exact user-reported case: `commitIgnore: { glob: ["*"] }` should mean
    // "no LLM, just a chore commit" — NOT "send the full diff to the LLM"
    // (which libgit2 would do with no pathspecs added).
    let tmp = init_repo_with_tags(&[]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    let index = repo.index().unwrap();
    let files = vec![
        ("Cargo.lock".to_string(), false),
        ("src/main.rs".to_string(), false),
        ("README.md".to_string(), false),
    ];
    let ignore = vec!["*".to_string()];
    let msg = commit_msg_for_diff(&repo, &index, None, &files, Some(&ignore));
    assert_eq!(msg, "chore: update");
}

#[test]
fn commit_msg_returns_chore_update_when_all_files_are_binary() {
    // No commitIgnore at all, but every staged file is binary → nothing
    // can be fed to the model → fall back to a chore commit instead of
    // invoking the LLM with no content.
    let tmp = init_repo_with_tags(&[]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    let index = repo.index().unwrap();
    let files = vec![
        ("icon.png".to_string(), true),
        ("logo.jpg".to_string(), true),
    ];
    let msg = commit_msg_for_diff(&repo, &index, None, &files, None);
    assert_eq!(msg, "chore: update");
}

#[test]
fn commit_msg_returns_chore_update_when_inventory_is_empty() {
    // Degenerate case — caller passed no files at all.
    let tmp = init_repo_with_tags(&[]);
    let repo = git2::Repository::open(tmp.path()).unwrap();
    let index = repo.index().unwrap();
    let msg = commit_msg_for_diff(&repo, &index, None, &[], None);
    assert_eq!(msg, "chore: update");
}

#[test]
fn regenerate_readme_is_noop_when_globs_match_nothing() {
    // README.md must be untouched when the readme globs don't match any
    // tracked file. Otherwise we'd write a hallucinated README from an
    // empty source corpus.
    let tmp = TempDir::new().unwrap();
    let repo = git2::Repository::init(tmp.path()).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.email", "test@buidl.local").unwrap();
        cfg.set_str("user.name", "buidl-test").unwrap();
    }
    touch(tmp.path(), "main.rs", "fn main() {}\n");
    touch(tmp.path(), "README.md", "ORIGINAL CONTENT — do not overwrite\n");
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("main.rs")).unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();

    // Globs match no tracked file (no swift sources here).
    let globs = vec!["*.swift".to_string()];
    regenerate_readme(&repo, &mut index, &globs);

    let after = fs::read_to_string(tmp.path().join("README.md")).unwrap();
    assert_eq!(after, "ORIGINAL CONTENT — do not overwrite\n");
}

#[test]
fn regenerate_readme_is_noop_when_glob_list_is_empty() {
    // `readme: { glob: [] }` — degenerate but valid. README.md must stay.
    let tmp = TempDir::new().unwrap();
    let repo = git2::Repository::init(tmp.path()).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.email", "test@buidl.local").unwrap();
        cfg.set_str("user.name", "buidl-test").unwrap();
    }
    touch(tmp.path(), "main.rs", "fn main() {}\n");
    touch(tmp.path(), "README.md", "UNTOUCHED\n");
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("main.rs")).unwrap();
    index.add_path(Path::new("README.md")).unwrap();
    index.write().unwrap();

    regenerate_readme(&repo, &mut index, &[]);

    let after = fs::read_to_string(tmp.path().join("README.md")).unwrap();
    assert_eq!(after, "UNTOUCHED\n");
}

#[test]
fn regenerate_readme_is_noop_when_index_is_empty() {
    // Empty repo (no tracked files at all) — there's nothing to summarize.
    // Caller's responsibility to not even reach this, but the guard makes
    // it safe regardless.
    let tmp = TempDir::new().unwrap();
    let repo = git2::Repository::init(tmp.path()).unwrap();
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.email", "test@buidl.local").unwrap();
        cfg.set_str("user.name", "buidl-test").unwrap();
    }
    let mut index = repo.index().unwrap();

    // Don't touch a README at all; just confirm regen doesn't panic / create one.
    let globs = vec!["*.rs".to_string()];
    regenerate_readme(&repo, &mut index, &globs);

    assert!(!tmp.path().join("README.md").exists());
}
