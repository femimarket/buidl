use serde::Deserialize;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Top-level shape of `build.json`. New top-level fields (kotlin/android/js
/// arrays, eventually `spec` / `templatesRoot` / `apiserver` etc.) get added
/// here as the orchestrator grows beyond swift.
#[derive(Deserialize)]
pub struct BuildConfig {
    #[serde(default)]
    pub swiftapps: Vec<Entry>,
}

/// One repo to release. `path` is the local checkout; `remote` is informational
/// for now (the existing git config in the repo is the source of truth for the
/// push URL). `kind: "openapi"` means pre-wipe + run openapi-generator before
/// the canonical commit pass; absent or anything else means just commit pass.
#[derive(Deserialize)]
pub struct Entry {
    pub path: String,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub data: Option<EntryData>,
}

/// Per-entry knobs consumed when `kind == "openapi"`.
#[derive(Deserialize, Default)]
pub struct EntryData {
    #[serde(default)]
    pub templates: Option<String>,
    #[serde(rename = "deleteAllExceptGit", default)]
    pub delete_all_except_git: bool,
}

/// Upstream API spec consumed by every openapi-kind entry. Hardcoded for now;
/// move to build.json's top level if/when a second spec source shows up.
pub const SPEC: &str = "http://localhost:80/api-docs/openapi.json";

/// First tag a release cuts when no tag exists yet. Pre-1.0 signals the API
/// isn't stable — bump major to 1.0.0 when you'd feel bad breaking it.
pub const DEFAULT_VERSION: &str = "v0.1.0";

/// Read every tracked file, ask LM Studio to regenerate README.md from the
/// whole codebase, write it, and re-stage. The existing README is skipped so
/// the model doesn't anchor to it. Binary blobs are silently skipped (can't
/// feed bytes to the model); they'll appear in `git ls-files` but not in the
/// prompt. Project `.gitignore` is the source of truth for what's tracked.
pub fn regenerate_readme(index: &mut git2::Index) {
    let paths: Vec<String> = index.iter()
        .map(|e| {
            let raw = e.path.clone();
            std::str::from_utf8(&e.path)
                .unwrap_or_else(|err| panic!("git path is not utf8 ({raw:?}): {err}"))
                .to_string()
        })
        .filter(|p| p != "README.md")
        .collect();

    let mut sections: Vec<String> = Vec::with_capacity(paths.len());
    let mut included: Vec<(String, usize)> = Vec::with_capacity(paths.len());
    for path in &paths {
        let bytes = std::fs::read(path)
            .unwrap_or_else(|e| panic!("reading staged file {path:?}: {e}"));
        // Binary blobs (PNG, etc.) can't go in the prompt. Skip silently —
        // they're committed for runtime use, not for README content.
        let Ok(content) = std::str::from_utf8(&bytes) else { continue };
        included.push((path.clone(), content.len()));
        sections.push(format!("=== {path} ===\n{content}\n"));
    }

    eprintln!("[buidl] README regen — files included:");
    for (p, sz) in &included {
        eprintln!("[buidl]   {sz:>8}  {p}");
    }
    let repo_text = sections.join("\n");
    eprintln!(
        "[buidl] README regen: {} files, {} chars (~{}k tokens)",
        included.len(), repo_text.len(), repo_text.len() / 4 / 1000,
    );

    let prompt = format!(
        "You are generating a README.md for the software project below. The \
         existing README is excluded so you write fresh — don't anchor to \
         anything pre-existing.\n\n\
         Produce a comprehensive, accurate README that documents the project \
         as it actually exists today: what it does, how to install/build/run, \
         how to use it, the architecture and key files, any non-obvious \
         conventions. Cite specific file paths where it helps a reader \
         orient. \n\n\
         Output ONLY the markdown body of README.md — no commentary, no \
         preface, no surrounding code fences. Start with a `# <project name>` \
         heading.\n\n\
         REPOSITORY:\n{repo_text}"
    );
    let new_readme = lm(&prompt);
    let cleaned = strip_outer_fence(&new_readme);
    std::fs::write("README.md", cleaned)
        .unwrap_or_else(|e| panic!("writing README.md in {:?}: {e}", std::env::current_dir()));
    index.add_path(Path::new("README.md"))
        .unwrap_or_else(|e| panic!("re-staging README.md in {:?}: {e}", std::env::current_dir()));
    index.write()
        .unwrap_or_else(|e| panic!("writing index after README regen: {e}"));
}

/// If the model wrapped the entire response in a ```` ```markdown ... ``` ````
/// fence (or plain ``` ... ```), strip it. Without this the README starts and
/// ends with literal fence lines.
fn strip_outer_fence(s: &str) -> String {
    let trimmed = s.trim();
    let mut lines: Vec<&str> = trimmed.lines().collect();
    if lines.first().is_some_and(|l| l.trim_start().starts_with("```"))
        && lines.last().is_some_and(|l| l.trim() == "```")
    {
        lines.remove(0);
        lines.pop();
        return lines.join("\n");
    }
    trimmed.to_string()
}

/// POST a user message to local LM Studio and return the assistant's reply.
pub fn lm(prompt: &str) -> String {
    // chars/4 is the standard rough estimate for English/code tokens.
    eprintln!("[buidl] prompt: {} chars (~{} tokens)", prompt.len(), prompt.len() / 4);
    print!("thinking... ");
    io::stdout().flush().expect("flushing stdout");

    let body = serde_json::json!({
        "model": "qwen/qwen3.6-35b-a3b",
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": 0.2,
        "max_tokens": 8000,
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let resp: serde_json::Value = reqwest::blocking::Client::builder()
        // README regen and large refactor diffs run multi-minute completions
        // on the local Qwen3. 60s wasn't enough for either; 30 min covers
        // both with margin and is bounded by the process anyway.
        .timeout(Duration::from_secs(1800))
        .build()
        .expect("building HTTP client")
        .post("http://localhost:1234/v1/chat/completions")
        .json(&body)
        .send().expect("POST to LM Studio (is it running on localhost:1234?)")
        .error_for_status().expect("LM Studio HTTP error")
        .json().expect("decoding LM Studio response");
    println!();

    let message = resp.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("LM Studio response missing choices[0].message.content: {resp}"))
        .trim()
        .to_string();
    if message.is_empty() {
        panic!("LM Studio returned empty message");
    }
    message
}

/// Stage everything in `repo`'s workdir. `add_all` with `DEFAULT` already
/// respects the project's `.gitignore`, so we don't keep a parallel ignore
/// list — if something noisy lands in the diff, the project's `.gitignore`
/// is the place to fix it.
pub fn stage_all(repo: &git2::Repository) -> git2::Index {
    let mut index = repo.index().expect("getting index");
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).expect("staging all");
    index.write().expect("writing index");
    index
}

/// Diff HEAD's tree against the given index. Returns the tree (possibly None
/// for an unborn branch) and the list of (path, is_binary) tuples extracted
/// from each delta. Empty list → no changes to commit.
pub fn diff_files<'r>(
    repo: &'r git2::Repository,
    index: &git2::Index,
) -> (Option<git2::Tree<'r>>, Vec<(String, bool)>) {
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let diff = repo.diff_tree_to_index(head_tree.as_ref(), Some(index), None)
        .expect("diffing HEAD to index");
    let files: Vec<(String, bool)> = diff.deltas().map(|d| {
        let path = d.new_file().path().or_else(|| d.old_file().path())
            .expect("delta has neither new nor old path")
            .to_string_lossy().into_owned();
        let is_binary = d.new_file().is_binary() || d.old_file().is_binary();
        (path, is_binary)
    }).collect();
    (head_tree, files)
}

/// Print the staged-file inventory — one line per path.
pub fn print_staged(files: &[(String, bool)]) {
    eprintln!("[buidl] {} staged file(s):", files.len());
    for (p, _) in files {
        eprintln!("[buidl]   {p}");
    }
}

/// Ask LM Studio for a Conventional Commits message from the staged diff.
/// `is_binary` deltas are skipped from the prompt (model can't read bytes);
/// everything else is fed as a unified patch. Validates the type before
/// returning so a malformed model reply panics here, not at tag-bump time.
pub fn commit_msg_for_diff(
    repo: &git2::Repository,
    index: &git2::Index,
    head_tree: Option<&git2::Tree<'_>>,
    files: &[(String, bool)],
) -> String {
    let mut opts = git2::DiffOptions::new();
    for (path, bin) in files {
        if !*bin { opts.pathspec(path); }
    }
    let filtered = repo.diff_tree_to_index(head_tree, Some(index), Some(&mut opts))
        .expect("computing filtered diff");
    let mut diff_text = String::new();
    filtered.print(git2::DiffFormat::Patch, |_d, _h, line| {
        let content = std::str::from_utf8(line.content())
            .expect("diff line is utf8 (binary deltas excluded)");
        match line.origin() {
            '+' | '-' | ' ' => {
                diff_text.push(line.origin());
                diff_text.push_str(content);
            }
            _ => diff_text.push_str(content),
        }
        true
    }).expect("rendering filtered diff");

    let prompt = format!(
        "Write a Conventional Commits message (type(scope): description). \
         Types: feat|fix|docs|style|refactor|perf|test|build|ci|chore. \
         Output a single line, no quotes, no markdown, no explanation.\n\nDIFF:\n{diff_text}"
    );
    let message = lm(&prompt);

    let first_line = message.lines().next().expect("lm output has at least one line");
    let colon = first_line.find(':')
        .unwrap_or_else(|| panic!("model did not return Conventional Commits format: {message:?}"));
    let prefix = first_line[..colon].trim_end_matches('!');
    let type_part = prefix.split_once('(').map(|(t, _)| t).unwrap_or(prefix);
    const TYPES: &[&str] = &["feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore"];
    if !TYPES.contains(&type_part) {
        panic!("model returned non-Conventional type {type_part:?} in: {message:?}");
    }
    message
}

/// Convert a Conventional Commits header to a semver bump kind:
///  - `!` before `:` or "BREAKING CHANGE" in body → major
///  - `feat`                                      → minor
///  - anything else                               → patch
pub fn bump_kind(commit_msg: &str) -> &'static str {
    let first_line = commit_msg.lines().next().expect("commit_msg has at least one line");
    let head = first_line.split(':').next().expect("split always yields one item");
    if head.ends_with('!') || commit_msg.contains("BREAKING CHANGE") {
        "major"
    } else {
        let type_part = head.split_once('(').map(|(t, _)| t).unwrap_or(head);
        if type_part == "feat" { "minor" } else { "patch" }
    }
}

/// Highest existing tag by semver tuple, then bump by `kind`. Returns
/// DEFAULT_VERSION if no tags exist. Deterministic even when multiple tags
/// point at the same commit (which used to break `git describe`).
pub fn compute_new_tag(repo: &git2::Repository, kind: &str) -> String {
    let last = repo.tag_names(None).expect("listing tags")
        .iter().flatten().flatten()
        .map(|t| {
            // Accept both `vX.Y.Z` and `X.Y.Z`; anything else panics with the
            // offending tag so you know exactly which tag to delete.
            let v = t.strip_prefix('v').unwrap_or(t);
            let mut p = v.split('.');
            let major = p.next()
                .unwrap_or_else(|| panic!("tag {t:?} missing major"));
            let minor = p.next()
                .unwrap_or_else(|| panic!("tag {t:?} missing minor (expected vMAJOR.MINOR.PATCH)"));
            let patch = p.next()
                .unwrap_or_else(|| panic!("tag {t:?} missing patch (expected vMAJOR.MINOR.PATCH)"));
            (
                major.parse::<u32>()
                    .unwrap_or_else(|e| panic!("tag {t:?} major {major:?} not u32: {e}")),
                minor.parse::<u32>()
                    .unwrap_or_else(|e| panic!("tag {t:?} minor {minor:?} not u32: {e}")),
                patch.parse::<u32>()
                    .unwrap_or_else(|e| panic!("tag {t:?} patch {patch:?} not u32: {e}")),
            )
        })
        .max();
    match (last, kind) {
        (None, _) => DEFAULT_VERSION.to_string(),
        (Some((major, _, _)), "major") => format!("v{}.0.0", major + 1),
        (Some((major, minor, _)), "minor") => format!("v{major}.{}.0", minor + 1),
        (Some((major, minor, patch)), _) => format!("v{major}.{minor}.{}", patch + 1),
    }
}

/// Write the index as a tree, then create a commit on HEAD pointing at it.
/// Author/committer from git config; no hooks, no GPG signing. Handles the
/// unborn-branch case (initial commit on a brand-new repo).
pub fn commit_with_msg(
    repo: &git2::Repository,
    index: &mut git2::Index,
    msg: &str,
) {
    let tree_oid = index.write_tree().expect("writing tree from index");
    let tree = repo.find_tree(tree_oid).expect("finding tree");
    let sig = repo.signature().expect("getting signature from git config");
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &parents)
        .expect("creating commit");
}

/// Shell out `git push origin main` in `cwd`. We use the binary rather than
/// libgit2's `Remote::push` because libgit2 doesn't read the user's
/// credential helper, SSH agent, or GitHub keychain — git CLI handles all of
/// that. `cwd` lets us avoid mutating the process-wide working directory.
pub fn git_push_main(cwd: &Path) {
    let status = Command::new("git").args(["push", "origin", "main"])
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("spawning `git push origin main` in {cwd:?}: {e}"));
    if !status.success() {
        panic!("`git push origin main` in {cwd:?} failed with {status}");
    }
}

/// Tag HEAD lightweight + `git push origin <tag>` in `cwd`.
pub fn tag_and_push(repo: &git2::Repository, tag: &str, cwd: &Path) {
    let head_commit = repo.head().expect("getting HEAD")
        .peel_to_commit().expect("peeling HEAD to commit");
    repo.tag_lightweight(tag, head_commit.as_object(), false)
        .unwrap_or_else(|e| panic!("creating tag {tag} in {cwd:?}: {e}"));
    let status = Command::new("git").args(["push", "origin", tag])
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("spawning `git push origin {tag}` in {cwd:?}: {e}"));
    if !status.success() {
        panic!("`git push origin {tag}` in {cwd:?} failed with {status}");
    }
}

/// Delete every direct child of `dir` except `.git/`. Bash equivalent:
///   `find "$dir" -mindepth 1 -maxdepth 1 ! -name '.git' -exec rm -rf {} +`
/// Used before openapi-generator runs so leftover files from a previous
/// spec don't survive when the spec stops referring to them.
pub fn wipe_except_git(dir: &Path) {
    let read = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
    for entry in read {
        let entry = entry
            .unwrap_or_else(|e| panic!("reading dir entry in {dir:?}: {e}"));
        if entry.file_name() == ".git" { continue; }
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .unwrap_or_else(|e| panic!("rm -rf {path:?}: {e}"));
        } else {
            std::fs::remove_file(&path)
                .unwrap_or_else(|e| panic!("rm {path:?}: {e}"));
        }
    }
}

/// Run `openapi-generator generate` with the supplied args. The caller passes
/// every flag (generator, input spec, output dir, template dir, properties).
/// We don't impose any defaults — those are language-specific and belong in
/// main.rs where the JSON-driven orchestrator dispatches by entry kind.
pub fn run_openapi_generator(args: &[&str]) {
    eprintln!("[buidl] openapi-generator generate {}", args.join(" "));
    let status = Command::new("openapi-generator")
        .arg("generate").args(args)
        .status()
        .unwrap_or_else(|e| panic!("spawning `openapi-generator generate {}`: {e}", args.join(" ")));
    if !status.success() {
        panic!("`openapi-generator generate {}` failed with {status}", args.join(" "));
    }
}
