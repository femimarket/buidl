use serde::Deserialize;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// `build.json` is a flat list of repos. The language is auto-detected per
/// entry from the files in the repo — no need to declare swift/kotlin/android/
/// js/rust/comfyui upfront. The only optional `kind` is `"openapi"`, which
/// flags the entry as needing an `openapi-generator` pass before the commit.
pub type BuildConfig = Vec<Entry>;

/// One repo to release. `path` is the local checkout; `remote` is informational
/// (the existing git config in the repo is the source of truth for the push
/// URL). `kind: "openapi"` triggers a pre-wipe + openapi-generator pass; absent
/// or anything else means just commit/push/tag.
///
/// `readme` opts into LLM-generated README rewrites:
/// - Field absent → skip README regen entirely.
/// - Field present with `glob` → regen README using only files whose path
///   matches at least one glob pattern.
///
/// `commitIgnore` opts files OUT of the commit-message diff:
/// - Field absent → commit-msg LLM sees the full staged diff (default).
/// - Field present with `glob` → files matching any glob are excluded from
///   the prompt. Lockfiles, generated code, and large data files are
///   common candidates.
#[derive(Deserialize)]
pub struct Entry {
    pub path: String,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub data: Option<EntryData>,
    #[serde(default)]
    pub readme: Option<ReadmeConfig>,
    #[serde(default, rename = "commitIgnore")]
    pub commit_ignore: Option<CommitIgnore>,
    #[serde(default)]
    pub release: Option<ReleaseConfig>,
}

/// Per-entry knobs consumed when `kind == "openapi"`.
#[derive(Deserialize, Default)]
pub struct EntryData {
    #[serde(default)]
    pub templates: Option<String>,
    #[serde(rename = "deleteAllExceptGit", default)]
    pub delete_all_except_git: bool,
}

/// What sources to feed the README regeneration LLM. `glob` is a list of glob
/// patterns; only files whose repo-relative path matches at least one pattern
/// is included in the prompt. Empty list → no files → empty README prompt
/// (degenerate — you probably want non-empty patterns).
///
/// Pattern semantics: glob 0.3 `Pattern::matches_with` with
/// `require_literal_separator: true`. Patterns without a `/` are anchored
/// anywhere (`*.swift` matches `foo.swift` and `Sources/Foo.swift`); patterns
/// with a `/` (`Sources/*.swift`, `Prod/**/*.swift`) match exactly what they
/// describe.
#[derive(Deserialize, Default)]
pub struct ReadmeConfig {
    #[serde(default)]
    pub glob: Vec<String>,
}

/// What sources to EXCLUDE from the commit-message LLM prompt. Same pattern
/// semantics as `ReadmeConfig.glob` — typical use is `["*.lock"]` to keep
/// dep churn out of the prompt.
#[derive(Deserialize, Default)]
pub struct CommitIgnore {
    #[serde(default)]
    pub glob: Vec<String>,
}

/// Assets to attach to a GitHub release tied to the just-pushed tag. Each
/// `glob` is expanded via the `glob` crate; absolute paths are taken
/// as-is, relative paths are resolved against the entry's repo dir.
///
/// `release` is only honored when the canonical flow actually cuts a new
/// tag — runs that exit with "Nothing to release" never reach the gh step.
/// Requires the `gh` CLI on PATH and an authenticated session.
#[derive(Deserialize, Default)]
pub struct ReleaseConfig {
    #[serde(default)]
    pub glob: Vec<String>,
}

/// What kind of repo this is, detected from the files on disk. Drives the
/// publish step (gradle for android/kotlin, nothing extra for the rest). The
/// `kind=openapi` entry in build.json is orthogonal — that's about whether to
/// RUN the generator first; this enum describes what's already there to
/// commit and ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoKind {
    Swift,
    Kotlin,
    Android,
    Js,
    Rust,
    /// Anything not matching one of the build-tool signatures above —
    /// ComfyUI workflows, plain docs repos, mustache templates, whatever.
    /// Treated as "just commit/push/tag, no language-specific publish step."
    Generic,
}

/// From the staged-file inventory, pick the paths that should be fed to the
/// commit-message LLM as a unified patch. Binary files are always dropped
/// (model can't read bytes); `ignore_globs`, if `Some`, additionally drops
/// anything matching at least one glob. Extracted from `commit_msg_for_diff`
/// so the filtering logic is unit-testable without an LLM call.
pub fn select_commit_paths<'a>(
    files: &'a [(String, bool)],
    ignore_globs: Option<&[String]>,
) -> Vec<&'a str> {
    files.iter()
        .filter(|(_, bin)| !bin)
        .filter(|(p, _)| match ignore_globs {
            Some(globs) => !matches_any_glob(p, globs),
            None => true,
        })
        .map(|(p, _)| p.as_str())
        .collect()
}

/// From the staged-file inventory in the index, pick the paths to feed to the
/// README-regen LLM. README.md itself is always dropped (otherwise we anchor
/// to the existing README and defeat the point of regen); `globs` filters to
/// only matching paths. Extracted so the filter is unit-testable without an
/// LLM call.
pub fn select_readme_paths<'a>(index_paths: &'a [String], globs: &[String]) -> Vec<&'a str> {
    index_paths.iter()
        .filter(|p| p.as_str() != "README.md")
        .filter(|p| matches_any_glob(p, globs))
        .map(String::as_str)
        .collect()
}

/// True if `path` matches at least one of the `globs`. Patterns without a `/`
/// are anchored anywhere (`*.swift` matches both `foo.swift` and
/// `Sources/Foo.swift`). Patterns with a `/` use standard glob semantics with
/// `require_literal_separator: true` — `Sources/*.swift` matches only direct
/// children, `Sources/**/*.swift` recurses. Empty `globs` returns false.
pub fn matches_any_glob(path: &str, globs: &[String]) -> bool {
    const OPTS: glob::MatchOptions = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    globs.iter().any(|g| {
        let normalized = if g.contains('/') { g.to_string() } else { format!("**/{g}") };
        glob::Pattern::new(&normalized)
            .unwrap_or_else(|e| panic!("invalid glob {g:?}: {e}"))
            .matches_with(path, OPTS)
    })
}

/// Inspect `path` and decide what kind of repo we're looking at. Order matters:
/// Cargo.toml is checked first so a polyglot repo with both Cargo.toml and
/// package.json is classified as Rust (the user can split the repo if that's
/// wrong). Anything not matching a known build-tool signature is Generic.
pub fn detect_kind(path: &Path) -> RepoKind {
    if path.join("Cargo.toml").is_file() { return RepoKind::Rust; }
    if path.join("Package.swift").is_file() { return RepoKind::Swift; }
    if has_xcodeproj(path) { return RepoKind::Swift; }
    if let Some(k) = detect_gradle(path) { return k; }
    if path.join("package.json").is_file() { return RepoKind::Js; }
    RepoKind::Generic
}

fn has_xcodeproj(path: &Path) -> bool {
    let Ok(read) = std::fs::read_dir(path) else { return false; };
    read.flatten().any(|e| {
        e.file_name().to_string_lossy().ends_with(".xcodeproj")
    })
}

/// Distinguish android (com.android.application/library plugin) from kotlin
/// multiplatform (kotlin("multiplatform") plugin). Reads root and `app/`
/// build.gradle.kts since android apps usually keep the plugin in app/.
fn detect_gradle(path: &Path) -> Option<RepoKind> {
    let candidates = [
        path.join("build.gradle.kts"),
        path.join("build.gradle"),
        path.join("app").join("build.gradle.kts"),
        path.join("app").join("build.gradle"),
    ];
    let mut found_gradle = false;
    for c in &candidates {
        let Ok(text) = std::fs::read_to_string(c) else { continue };
        found_gradle = true;
        if text.contains("com.android.application") || text.contains("com.android.library") {
            return Some(RepoKind::Android);
        }
        if text.contains("kotlin(\"multiplatform\")") || text.contains("kotlin('multiplatform')") {
            return Some(RepoKind::Kotlin);
        }
    }
    if found_gradle {
        // gradle file exists buthttps://localhost/api-docs/openapi.json neither marker — best guess is kotlin
        // multiplatform since plain kotlin/jvm projects are rare here.
        return Some(RepoKind::Kotlin);
    }
    None
}

/// Upstream API spec consumed by every openapi-kind entry. Hardcoded for now;
/// move to build.json's top level if/when a second spec source shows up.
pub const SPEC: &str = "https://localhost:443/api-docs/openapi.json";

/// Local path used by `fetch_spec_to_tmp` to drop the fetched spec. Stable
/// path so repeated runs overwrite the same file rather than littering /tmp.
pub const SPEC_LOCAL_PATH: &str = "/tmp/buidl-spec.json";

/// First tag a release cuts when no tag exists yet. Pre-1.0 signals the API
/// isn't stable — bump major to 1.0.0 when you'd feel bad breaking it.
pub const DEFAULT_VERSION: &str = "v0.1.0";

/// Read every tracked file, ask LM Studio to regenerate README.md from the
/// whole codebase, write it, and re-stage. The existing README is skipped so
/// the model doesn't anchor to it. Binary blobs are silently skipped (can't
/// feed bytes to the model); they'll appear in `git ls-files` but not in the
/// prompt. Project `.gitignore` is the source of truth for what's tracked.
pub fn regenerate_readme(
    repo: &git2::Repository,
    index: &mut git2::Index,
    globs: &[String],
) {
    let workdir = repo.workdir()
        .unwrap_or_else(|| panic!("repo {:?} is bare; can't regen README", repo.path()));

    let all_paths: Vec<String> = index.iter()
        .map(|e| {
            let raw = e.path.clone();
            std::str::from_utf8(&e.path)
                .unwrap_or_else(|err| panic!("git path is not utf8 ({raw:?}): {err}"))
                .to_string()
        })
        .collect();
    let paths: Vec<String> = select_readme_paths(&all_paths, globs)
        .into_iter().map(str::to_string).collect();

    // Short-circuit: no source files matched. Asking the model to invent a
    // README from nothing would just produce hallucinated content and then
    // we'd commit it. Treat as a no-op — caller's `readme.glob` configured
    // the regen out for this run, intentionally or not.
    if paths.is_empty() {
        eprintln!("[buidl] README regen: 0 files matched globs {globs:?}; skipping");
        return;
    }

    let mut sections: Vec<String> = Vec::with_capacity(paths.len());
    let mut included: Vec<(String, usize)> = Vec::with_capacity(paths.len());
    for path in &paths {
        let abs = workdir.join(path);
        let bytes = std::fs::read(&abs)
            .unwrap_or_else(|e| panic!("reading staged file {abs:?}: {e}"));
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
    let readme_abs = workdir.join("README.md");
    std::fs::write(&readme_abs, cleaned)
        .unwrap_or_else(|e| panic!("writing {readme_abs:?}: {e}"));
    index.add_path(Path::new("README.md"))
        .unwrap_or_else(|e| panic!("re-staging README.md in repo {:?}: {e}", repo.path()));
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

/// Lines we want in any Swift project's `.gitignore`. `.build/` and `.swiftpm/`
/// matter for staging — both contain nested git repos (SPM dependency checkouts
/// and the SPM workspace state) that libgit2's `add_all` errors on. The
/// `xcuserdata/` patterns matter for safety — schemes there can carry launch
/// args / env vars, which is where secrets tend to leak in.
pub const SWIFT_GITIGNORE_LINES: &[&str] = &[
    ".build/",
    ".swiftpm/",
    "DerivedData/",
    "build/",
    "xcuserdata/",
    "*.xcuserdatad/",
    ".DS_Store",
];

/// True if any path component matches a Swift-ignore name. Mirrors what the
/// `SWIFT_GITIGNORE_LINES` directory patterns express (sans-leading-slash
/// gitignore patterns match the basename anywhere in the tree).
fn path_matches_swift_ignores(path: &std::path::Path) -> bool {
    for comp in path.components() {
        if let std::path::Component::Normal(os) = comp {
            if let Some(s) = os.to_str() {
                match s {
                    ".build" | ".swiftpm" | "DerivedData" | "build" | "xcuserdata" | ".DS_Store" => return true,
                    _ if s.ends_with(".xcuserdatad") => return true,
                    _ => {}
                }
            }
        }
    }
    false
}

/// For Swift entries: append any missing `SWIFT_GITIGNORE_LINES` to the repo's
/// `.gitignore` (creating the file if absent, preserving whatever's there), AND
/// untrack any already-committed files that now match those ignore rules — a
/// pure `.gitignore` edit has no effect on files already in the index.
pub fn ensure_swift_gitignore(repo: &git2::Repository) {
    use std::collections::HashSet;
    let path = repo.workdir()
        .unwrap_or_else(|| panic!("ensure_swift_gitignore: repo has no workdir"));

    let gi = path.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    let have: HashSet<&str> = existing.lines().map(|l| l.trim()).collect();
    let missing: Vec<&str> = SWIFT_GITIGNORE_LINES.iter().copied()
        .filter(|l| !have.contains(l)).collect();
    if !missing.is_empty() {
        let mut out = existing.clone();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        for line in &missing {
            out.push_str(line);
            out.push('\n');
        }
        std::fs::write(&gi, out)
            .unwrap_or_else(|e| panic!("writing .gitignore at {}: {e}", gi.display()));
        println!("[buidl] added {} swift .gitignore line(s): {}", missing.len(), missing.join(", "));
    }

    let mut index = repo.index().expect("getting index");
    let to_untrack: Vec<std::path::PathBuf> = index.iter()
        .map(|e| std::path::PathBuf::from(String::from_utf8_lossy(&e.path).into_owned()))
        .filter(|p| path_matches_swift_ignores(p))
        .collect();
    if !to_untrack.is_empty() {
        for p in &to_untrack {
            index.remove_path(p)
                .unwrap_or_else(|e| panic!("untracking {}: {e}", p.display()));
        }
        index.write().expect("writing index after untracking ignored files");
        println!("[buidl] untracked {} previously-committed ignored file(s)", to_untrack.len());
    }
}

/// If the repo has no `origin` remote, set it to `url`. Existing remote left
/// alone — no validation of its URL. The `remote` field in build.json is the
/// trigger; absent → this isn't called.
pub fn ensure_remote(repo: &git2::Repository, url: &str) {
    if repo.find_remote("origin").is_ok() {
        return;
    }
    repo.remote("origin", url)
        .unwrap_or_else(|e| panic!("setting remote origin to {url}: {e}"));
    println!("[buidl] set remote origin → {url}");
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

/// Print the staged-file inventory with a per-file tag showing whether it
/// reaches the commit-message LLM (`→ LLM`) or is dropped (`binary` /
/// `commitIgnore`). Tag classification uses the same `select_commit_paths`
/// the LLM call uses, so the print is always faithful to what the model
/// actually sees.
pub fn print_staged(files: &[(String, bool)], ignore_globs: Option<&[String]>) {
    let to_llm = select_commit_paths(files, ignore_globs);
    let to_llm_set: std::collections::HashSet<&str> = to_llm.iter().copied().collect();
    let mut binary = 0;
    let mut ignored = 0;
    for (path, is_binary) in files {
        if !to_llm_set.contains(path.as_str()) {
            if *is_binary { binary += 1; } else { ignored += 1; }
        }
    }
    eprintln!(
        "[buidl] {} staged file(s): {} → LLM, {} binary, {} commitIgnore",
        files.len(), to_llm.len(), binary, ignored,
    );
    for (path, is_binary) in files {
        let tag = if to_llm_set.contains(path.as_str()) { "→ LLM" }
                  else if *is_binary { "skip (binary)" }
                  else { "skip (commitIgnore)" };
        eprintln!("[buidl]   {tag:>20}  {path}");
    }
}

/// Ask LM Studio for a Conventional Commits message from the staged diff.
/// `is_binary` deltas are skipped from the prompt (model can't read bytes);
/// `ignore_globs` (if `Some`) further excludes paths matching any glob —
/// typical use is `["*.lock"]` to keep dep-resolution churn out of the
/// prompt. Validates the model's reply before returning so a malformed
/// Conventional Commits header panics here, not at tag-bump time.
pub fn commit_msg_for_diff(
    repo: &git2::Repository,
    index: &git2::Index,
    head_tree: Option<&git2::Tree<'_>>,
    files: &[(String, bool)],
    ignore_globs: Option<&[String]>,
) -> String {
    // Short-circuit: if every file is binary or matches commitIgnore, the
    // filtered diff would be empty. Two semantic problems if we proceed:
    // (a) libgit2 treats "no pathspecs added" as "include EVERY path", which
    //     would silently feed the model the full diff — the OPPOSITE of what
    //     `commitIgnore: ["*"]` is meant to do;
    // (b) even with an actually-empty diff, asking the model to summarize
    //     nothing produces nonsense. Fall back to a hardcoded chore commit.
    let paths = select_commit_paths(files, ignore_globs);
    if paths.is_empty() {
        eprintln!(
            "[buidl] no files reach the commit-message LLM (binary + commitIgnore covered everything); using `chore: update`"
        );
        return "chore: update".to_string();
    }
    let mut opts = git2::DiffOptions::new();
    for path in &paths {
        opts.pathspec(path);
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

/// Run `openapi-generator generate` for the given output dir + templates dir.
/// The generator name and language-specific flags are derived from the
/// templates path's last component(s): `.../swift6` → swift6, `.../typescript-
/// fetch` → typescript-fetch, `.../multiplatform` (parent `libraries/`) →
/// kotlin multiplatform. `last_tag_raw` (no `v` prefix) is fed to typescript-
/// fetch's `npmVersion` and kotlin's `--artifact-version` so the regen output
/// version matches what `buidl` will tag — without this, the generator
/// defaults to `1.0.0` and we get infinite version-bump churn.
pub fn run_openapi_generator_for_templates(out: &str, templates: &str, last_tag_raw: &str) {
    let tpath = std::path::Path::new(templates);
    let leaf = tpath.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let parent = tpath.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("");

    let mut args: Vec<String> = vec![
        "-i".into(), SPEC.into(),
        "-o".into(), out.into(),
        "-t".into(), templates.into(),
    ];
    if leaf == "swift6" {
        args.extend(["-g".into(), "swift6".into()]);
        args.extend(["--additional-properties".into(),
                     "projectName=Api,responseAs=AsyncAwait".into()]);
    } else if leaf == "typescript-fetch" {
        args.extend(["-g".into(), "typescript-fetch".into()]);
        args.extend(["--git-host".into(), "github.com".into()]);
        args.extend(["--git-user-id".into(), "femimarket".into()]);
        args.extend(["--git-repo-id".into(), "jsapi".into()]);
        args.extend(["--additional-properties".into(), format!(
            "npmName=jsapi,npmVersion={last_tag_raw},supportsES6=true,modelPropertyNaming=camelCase,withInterfaces=true,useSingleRequestParameter=true,stringEnums=true"
        )]);
        args.extend(["--type-mappings".into(), "AnyType=any".into()]);
    } else if leaf == "multiplatform" || parent == "libraries" {
        args.extend(["-g".into(), "kotlin".into()]);
        args.extend(["--git-host".into(), "github.com".into()]);
        args.extend(["--git-user-id".into(), "femimarket".into()]);
        args.extend(["--git-repo-id".into(), "kotlinapi".into()]);
        args.extend(["--group-id".into(), "io.github.femimarket".into()]);
        args.extend(["--artifact-id".into(), "kotlinapi".into()]);
        args.extend(["--artifact-version".into(), last_tag_raw.into()]);
        args.extend(["--package-name".into(), "market.femi.api".into()]);
        args.extend(["--additional-properties".into(),
                     "library=multiplatform,dateLibrary=kotlinx-datetime,useCoroutines=true,modelPropertyNaming=camelCase,generateOneOfAnyOfWrappers=true".into()]);
        args.extend(["--import-mappings".into(),
                     "kotlin.uuid.Uuid=kotlin.uuid.Uuid,kotlinx.serialization.json.JsonElement=kotlinx.serialization.json.JsonElement".into()]);
        args.extend(["--type-mappings".into(),
                     "binary=kotlin.String,UUID=kotlin.uuid.Uuid,File=ByteArray,AnyType=kotlinx.serialization.json.JsonElement".into()]);
    } else {
        panic!("can't derive openapi-generator from templates {templates:?} \
                (expected leaf `swift6`, `typescript-fetch`, or path under `libraries/multiplatform`)");
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    eprintln!("[buidl] openapi-generator generate {}", arg_refs.join(" "));
    let status = Command::new("openapi-generator")
        .arg("generate").args(&arg_refs)
        .status()
        .unwrap_or_else(|e| panic!("spawning openapi-generator: {e}"));
    if status.success() && (leaf == "multiplatform" || parent == "libraries") {
        // Kotlin generator's gradle wrapper ships as a binary resource (not
        // template-overridable). It comes out chmod-644 (so `./gradlew`
        // fails Permission denied) and pinned to gradle 8.14.3 whose
        // bundled Kotlin compiler crashes on JDK 25. Patch both here so the
        // downstream `gradle_publish_github_packages` actually works.
        let gradlew = std::path::Path::new(out).join("gradlew");
        if gradlew.is_file() {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&gradlew).unwrap_or_else(|e| panic!("stat {gradlew:?}: {e}")).permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&gradlew, p)
                .unwrap_or_else(|e| panic!("chmod +x {gradlew:?}: {e}"));
        }
        let props = std::path::Path::new(out).join("gradle/wrapper/gradle-wrapper.properties");
        if props.is_file() {
            let content = std::fs::read_to_string(&props)
                .unwrap_or_else(|e| panic!("reading {props:?}: {e}"));
            let bumped = content
                .replace("gradle-8.14.3", "gradle-9.1.0")
                .replace("-all.zip", "-bin.zip");
            std::fs::write(&props, bumped)
                .unwrap_or_else(|e| panic!("writing {props:?}: {e}"));
        }
    }
    if !status.success() {
        panic!("openapi-generator failed with {status}");
    }
}

/// Highest existing tag without the `v` prefix, or `1.0.0` if no tags exist.
/// Used to seed openapi-generator's `npmVersion` / `--artifact-version` so the
/// generated client's version matches what `buidl` will tag. Non-semver tags
/// are skipped silently — we just need ONE valid tag for the seed.
pub fn last_tag(repo: &git2::Repository) -> String {
    let last = repo.tag_names(None).expect("listing tags")
        .iter().flatten().flatten()
        .filter_map(|t| {
            let v = t.strip_prefix('v').unwrap_or(t);
            let mut p = v.split('.');
            Some((p.next()?.parse::<u32>().ok()?,
                  p.next()?.parse::<u32>().ok()?,
                  p.next()?.parse::<u32>().ok()?))
        })
        .max();
    match last {
        Some((maj, min, pat)) => format!("{maj}.{min}.{pat}"),
        // Fall back to DEFAULT_VERSION (without the `v`) so the openapi-gen
        // seed matches what `compute_new_tag` would bump to on a tagless
        // first run — otherwise package.json's version diverges from the
        // first tag and the next run sees a spurious diff.
        None => DEFAULT_VERSION.strip_prefix('v').unwrap_or(DEFAULT_VERSION).to_string(),
    }
}

/// For Js (typescript-fetch) and Kotlin (multiplatform) openapi entries,
/// openapi-generator bakes the seed version into output files: `package.json`'s
/// `"version"`, README.md's `# <name>@<ver>` heading + `- Package version:
/// \`<ver>\`` line, and `build.gradle.kts`'s top-level `version = "..."`.
///
/// Without this sync step, HEAD's version markers lag the new tag by one
/// bump every run — openapi-gen seeds with `last_tag` but we tag with
/// `last_tag + 1`, so the next run sees a stale version diff and bumps
/// again, forever, even when the spec hasn't changed.
///
/// Caller MUST re-stage afterwards so the patched files reach the commit.
pub fn sync_openapi_versions(repo_kind: RepoKind, path: &Path, new_tag: &str) {
    let version = new_tag.strip_prefix('v').unwrap_or(new_tag);
    match repo_kind {
        RepoKind::Js => sync_js_versions(path, version),
        RepoKind::Kotlin => sync_kotlin_version(path, version),
        // swift6 doesn't bake version into output; android/rust/generic
        // aren't openapi-generated by this orchestrator.
        _ => {}
    }
}

fn sync_js_versions(path: &Path, version: &str) {
    let pkg = path.join("package.json");
    if pkg.is_file() {
        let content = std::fs::read_to_string(&pkg)
            .unwrap_or_else(|e| panic!("reading {pkg:?}: {e}"));
        let mut json: serde_json::Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("parsing {pkg:?}: {e}"));
        if let Some(obj) = json.as_object_mut() {
            obj.insert("version".to_string(), serde_json::Value::String(version.to_string()));
        }
        let pretty = serde_json::to_string_pretty(&json)
            .unwrap_or_else(|e| panic!("serializing {pkg:?}: {e}"));
        std::fs::write(&pkg, format!("{pretty}\n"))
            .unwrap_or_else(|e| panic!("writing {pkg:?}: {e}"));
    }

    let readme = path.join("README.md");
    if readme.is_file() {
        let content = std::fs::read_to_string(&readme)
            .unwrap_or_else(|e| panic!("reading {readme:?}: {e}"));
        let mut new_lines: Vec<String> = Vec::with_capacity(content.lines().count() + 1);
        for line in content.lines() {
            // `# <name>@<old>` → `# <name>@<new>` — `<name>` is whatever the
            // generator's `npmName` was; we don't hard-code it.
            if let Some(rest) = line.strip_prefix("# ") {
                if let Some((name, _old)) = rest.split_once('@') {
                    new_lines.push(format!("# {name}@{version}"));
                    continue;
                }
            }
            // `- Package version: \`<old>\`` → `\`<new>\``. Distinct from
            // `- API version: \`...\`` which describes the SPEC, not the
            // package — leave that alone.
            if line.starts_with("- Package version: ") {
                new_lines.push(format!("- Package version: `{version}`"));
                continue;
            }
            new_lines.push(line.to_string());
        }
        let mut out = new_lines.join("\n");
        if content.ends_with('\n') { out.push('\n'); }
        std::fs::write(&readme, out)
            .unwrap_or_else(|e| panic!("writing {readme:?}: {e}"));
    }
}

fn sync_kotlin_version(path: &Path, version: &str) {
    let gradle = path.join("build.gradle.kts");
    if gradle.is_file() {
        let content = std::fs::read_to_string(&gradle)
            .unwrap_or_else(|e| panic!("reading {gradle:?}: {e}"));
        let mut new_lines: Vec<String> = Vec::with_capacity(content.lines().count() + 1);
        for line in content.lines() {
            if line.starts_with("version = ") {
                new_lines.push(format!("version = \"{version}\""));
                continue;
            }
            new_lines.push(line.to_string());
        }
        let mut out = new_lines.join("\n");
        if content.ends_with('\n') { out.push('\n'); }
        std::fs::write(&gradle, out)
            .unwrap_or_else(|e| panic!("writing {gradle:?}: {e}"));
    }
}

/// Expand each `glob` in `globs` to a list of files on disk. Absolute paths
/// are passed straight through to the `glob` crate; relative paths are
/// resolved against `repo_path` first. A pattern with no wildcards still
/// works — it just matches the literal file if it exists, otherwise yields
/// nothing. Extracted from `gh_release_create` so it's unit-testable
/// without spawning a process.
pub fn expand_release_assets(repo_path: &Path, globs: &[String]) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    for g in globs {
        let pat_path = Path::new(g);
        let full: String = if pat_path.is_absolute() {
            g.clone()
        } else {
            repo_path.join(g).to_string_lossy().into_owned()
        };
        let entries = glob::glob(&full)
            .unwrap_or_else(|e| panic!("invalid release glob {full:?}: {e}"));
        for entry in entries {
            let p = entry.unwrap_or_else(|e| panic!("globbing release asset under {repo_path:?}: {e}"));
            out.push(p);
        }
    }
    out
}

/// Build the argv for `gh release create`. Separated so the exact CLI
/// invocation is testable without an authenticated `gh`. Uses
/// `--generate-notes` to auto-populate the body from commits since the
/// previous tag, and `--title <tag>` so the release page header matches
/// the tag name.
pub fn build_gh_release_args(tag: &str, assets: &[std::path::PathBuf]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "release".to_string(),
        "create".to_string(),
        tag.to_string(),
        "--title".to_string(),
        tag.to_string(),
        "--generate-notes".to_string(),
    ];
    for a in assets {
        args.push(a.to_string_lossy().into_owned());
    }
    args
}

/// Run `gh release create <tag> [assets…] --title <tag> --generate-notes`
/// in `cwd`. Requires `gh` on PATH and an authenticated session. Asset
/// globs are expanded via `expand_release_assets`.
pub fn gh_release_create(cwd: &Path, tag: &str, asset_globs: &[String]) {
    let assets = expand_release_assets(cwd, asset_globs);
    println!("📦 Creating GitHub release {tag} with {} asset(s)", assets.len());
    let args = build_gh_release_args(tag, &assets);
    let status = Command::new("gh")
        .args(&args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("spawning `gh {}` in {cwd:?}: {e}", args.join(" ")));
    if !status.success() {
        panic!("`gh {}` in {cwd:?} failed with {status}", args.join(" "));
    }
}

/// `./gradlew publishAllPublicationsToGitHubPackagesRepository
/// -PlibraryVersion=<version>` in `cwd`. Used for android apps and kotlin
/// multiplatform libraries that publish to GitHub Packages.
pub fn gradle_publish_github_packages(cwd: &Path, new_tag: &str) {
    let version = new_tag.strip_prefix('v').unwrap_or(new_tag);
    println!("📦 Publishing v{version} to GitHub Packages...");
    let status = Command::new("./gradlew")
        .args([
            "publishAllPublicationsToGitHubPackagesRepository",
            &format!("-PlibraryVersion={version}"),
        ])
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("spawning gradlew in {cwd:?}: {e}"));
    if !status.success() {
        panic!("./gradlew publish in {cwd:?} failed with {status}");
    }
}

// ── Unit tests for pure functions ──────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_kind_feat_is_minor() {
        assert_eq!(bump_kind("feat: add login"), "minor");
        assert_eq!(bump_kind("feat(auth): add login"), "minor");
    }

    #[test]
    fn bump_kind_fix_is_patch() {
        assert_eq!(bump_kind("fix: handle nil"), "patch");
        assert_eq!(bump_kind("fix(parser): handle nil"), "patch");
    }

    #[test]
    fn bump_kind_chore_docs_style_are_patch() {
        assert_eq!(bump_kind("chore: update deps"), "patch");
        assert_eq!(bump_kind("docs: fix typo"), "patch");
        assert_eq!(bump_kind("style: rustfmt"), "patch");
        assert_eq!(bump_kind("refactor: split mod"), "patch");
    }

    #[test]
    fn bump_kind_bang_is_major() {
        assert_eq!(bump_kind("feat!: drop api v1"), "major");
        assert_eq!(bump_kind("fix!: rename field"), "major");
        assert_eq!(bump_kind("refactor(api)!: split module"), "major");
    }

    #[test]
    fn bump_kind_breaking_change_in_body_is_major() {
        let msg = "feat: add v2\n\nBREAKING CHANGE: drops v1 endpoint";
        assert_eq!(bump_kind(msg), "major");
    }

    #[test]
    fn strip_outer_fence_drops_plain_fence() {
        let input = "```\nhello\nworld\n```";
        assert_eq!(strip_outer_fence(input), "hello\nworld");
    }

    #[test]
    fn strip_outer_fence_drops_lang_fence() {
        let input = "```markdown\n# Title\nbody\n```";
        assert_eq!(strip_outer_fence(input), "# Title\nbody");
    }

    #[test]
    fn strip_outer_fence_preserves_unfenced_content() {
        let input = "# Title\nNo fence here.";
        assert_eq!(strip_outer_fence(input), input);
    }

    #[test]
    fn strip_outer_fence_preserves_inner_fences() {
        // Triple-backtick inside the body must survive — only the outermost
        // opening/closing fence pair is stripped.
        let input = "```\nbefore\n```inner```\nafter\n```";
        assert_eq!(strip_outer_fence(input), "before\n```inner```\nafter");
    }

    fn s(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn matches_glob_extension_matches_at_root() {
        assert!(matches_any_glob("foo.swift", &s(&["*.swift"])));
    }

    #[test]
    fn matches_glob_extension_matches_in_subdirs_when_unanchored() {
        // Patterns without `/` are anchored anywhere — `*.swift` matches at
        // any depth. Ergonomic intent for "any swift file anywhere".
        assert!(matches_any_glob("Sources/Foo.swift", &s(&["*.swift"])));
        assert!(matches_any_glob("a/b/c/Foo.swift", &s(&["*.swift"])));
    }

    #[test]
    fn matches_glob_extension_rejects_other_extensions() {
        assert!(!matches_any_glob("foo.kt", &s(&["*.swift"])));
        assert!(!matches_any_glob("README.md", &s(&["*.swift"])));
    }

    #[test]
    fn matches_glob_any_of_multiple_patterns() {
        assert!(matches_any_glob("foo.swift", &s(&["*.kt", "*.swift"])));
        assert!(matches_any_glob("Foo.kt", &s(&["*.kt", "*.swift"])));
        assert!(!matches_any_glob("README.md", &s(&["*.kt", "*.swift"])));
    }

    #[test]
    fn matches_glob_empty_list_never_matches() {
        assert!(!matches_any_glob("foo.swift", &[]));
    }

    #[test]
    fn matches_glob_anchored_pattern_only_matches_immediate_children() {
        // `Prod/*.swift` matches files directly in `Prod/`, NOT nested deeper.
        assert!(matches_any_glob("Prod/Foo.swift", &s(&["Prod/*.swift"])));
        assert!(!matches_any_glob("Prod/Sub/Foo.swift", &s(&["Prod/*.swift"])));
        // `Prod/**/*.swift` matches both.
        assert!(matches_any_glob("Prod/Foo.swift", &s(&["Prod/**/*.swift"])));
        assert!(matches_any_glob("Prod/Sub/Foo.swift", &s(&["Prod/**/*.swift"])));
    }

    #[test]
    fn matches_glob_bare_filename_matches_at_any_depth() {
        // `Package.swift` (no `/`) is anchored anywhere by design — matches
        // root AND `Sources/Package.swift`. To restrict to root only, write
        // `./Package.swift` or use an explicitly-anchored glob like
        // `Sources/Package.swift`.
        assert!(matches_any_glob("Package.swift", &s(&["Package.swift"])));
        assert!(matches_any_glob("Sources/Package.swift", &s(&["Package.swift"])));
    }

    // ── select_commit_paths ────────────────────────────────────────────────

    fn f(items: &[(&str, bool)]) -> Vec<(String, bool)> {
        items.iter().map(|(p, b)| (p.to_string(), *b)).collect()
    }

    #[test]
    fn select_commit_paths_drops_binary_files() {
        let files = f(&[("main.rs", false), ("icon.png", true), ("Cargo.toml", false)]);
        let kept = select_commit_paths(&files, None);
        assert_eq!(kept, vec!["main.rs", "Cargo.toml"]);
    }

    #[test]
    fn select_commit_paths_no_ignore_globs_keeps_all_text_files() {
        let files = f(&[("main.rs", false), ("Cargo.lock", false), ("Cargo.toml", false)]);
        let kept = select_commit_paths(&files, None);
        assert_eq!(kept, vec!["main.rs", "Cargo.lock", "Cargo.toml"]);
    }

    #[test]
    fn select_commit_paths_drops_files_matching_ignore_globs() {
        // The actual user case: `commitIgnore.glob = ["*.lock"]` should
        // drop Cargo.lock but keep main.rs and Cargo.toml.
        let files = f(&[
            ("Cargo.lock", false),
            ("Cargo.toml", false),
            ("src/main.rs", false),
            ("src/lib.rs", false),
        ]);
        let ignore = s(&["*.lock"]);
        let kept = select_commit_paths(&files, Some(&ignore));
        assert_eq!(kept, vec!["Cargo.toml", "src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn select_commit_paths_combines_binary_skip_and_glob_filter() {
        let files = f(&[
            ("main.rs", false),
            ("Cargo.lock", false),  // dropped by ignore_globs
            ("icon.png", true),     // dropped because binary
            ("README.md", false),
        ]);
        let ignore = s(&["*.lock"]);
        let kept = select_commit_paths(&files, Some(&ignore));
        assert_eq!(kept, vec!["main.rs", "README.md"]);
    }

    #[test]
    fn select_commit_paths_empty_ignore_globs_keeps_text_files() {
        // Edge case: ignore_globs = Some(empty list). Should keep everything
        // since nothing matches.
        let files = f(&[("main.rs", false), ("Cargo.lock", false)]);
        let kept = select_commit_paths(&files, Some(&[]));
        assert_eq!(kept, vec!["main.rs", "Cargo.lock"]);
    }

    // ── select_readme_paths ────────────────────────────────────────────────

    #[test]
    fn select_readme_paths_always_drops_readme_md() {
        // Even if README.md matches the globs, it's still dropped to avoid
        // the model anchoring to its own previous output.
        let index = s(&["README.md", "src/main.rs"]);
        let globs = s(&["*.md", "*.rs"]);
        let kept = select_readme_paths(&index, &globs);
        assert_eq!(kept, vec!["src/main.rs"]);
    }

    #[test]
    fn select_readme_paths_keeps_only_matching_paths() {
        // The actual user case: `readme.glob = ["*.rs", "*.toml"]` for the
        // buidl repo. Should keep .rs and .toml files, drop Cargo.lock.
        let index = s(&["Cargo.lock", "Cargo.toml", "src/main.rs", "src/lib.rs", "README.md"]);
        let globs = s(&["*.rs", "*.toml"]);
        let kept = select_readme_paths(&index, &globs);
        assert_eq!(kept, vec!["Cargo.toml", "src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn select_readme_paths_anchored_glob_only_matches_immediate_children() {
        // The other user case: `readme.glob = ["Prod/*.swift"]`. Should only
        // include direct children of `Prod/`, not nested.
        let index = s(&[
            "Prod/Main.swift",
            "Prod/Sub/Helper.swift",
            "Test/Main.swift",
            "README.md",
        ]);
        let globs = s(&["Prod/*.swift"]);
        let kept = select_readme_paths(&index, &globs);
        assert_eq!(kept, vec!["Prod/Main.swift"]);
    }

    #[test]
    fn select_readme_paths_empty_globs_yields_empty() {
        let index = s(&["main.rs", "Cargo.toml"]);
        let kept = select_readme_paths(&index, &[]);
        assert!(kept.is_empty());
    }

    #[test]
    fn select_readme_paths_multiple_globs_union() {
        let index = s(&["a.rs", "b.swift", "c.kt", "d.md"]);
        let globs = s(&["*.rs", "*.kt"]);
        let kept = select_readme_paths(&index, &globs);
        assert_eq!(kept, vec!["a.rs", "c.kt"]);
    }

    // ── sync_openapi_versions ──────────────────────────────────────────────
    //
    // Each test writes a sample of what openapi-generator's output looks
    // like into a tempdir, runs the sync, then re-reads the file and
    // asserts the version was rewritten. These cover the exact rewrite
    // rules; the cross-run convergence is tested end-to-end in
    // integration.rs.

    use tempfile::TempDir;

    #[test]
    fn sync_js_rewrites_package_json_version() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"jsapi","version":"4.5.3","main":"./dist/index.js"}"#,
        ).unwrap();

        sync_openapi_versions(RepoKind::Js, tmp.path(), "v4.5.4");

        let after = std::fs::read_to_string(tmp.path().join("package.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(parsed["version"], "4.5.4");
        // Other fields preserved.
        assert_eq!(parsed["name"], "jsapi");
        assert_eq!(parsed["main"], "./dist/index.js");
    }

    #[test]
    fn sync_js_rewrites_readme_heading_and_package_version_line() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("README.md"),
            "# jsapi@4.5.3\n\nA TypeScript SDK.\n\n- API version: `1.0.0`\n- Package version: `4.5.3`\n",
        ).unwrap();

        sync_openapi_versions(RepoKind::Js, tmp.path(), "v4.5.4");

        let after = std::fs::read_to_string(tmp.path().join("README.md")).unwrap();
        assert!(after.contains("# jsapi@4.5.4"));
        assert!(after.contains("- Package version: `4.5.4`"));
        // API version is the SPEC version, not the package — must NOT change.
        assert!(after.contains("- API version: `1.0.0`"));
        assert!(after.ends_with('\n'), "trailing newline preserved");
    }

    #[test]
    fn sync_js_accepts_tag_without_v_prefix() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"jsapi","version":"4.5.3"}"#,
        ).unwrap();

        sync_openapi_versions(RepoKind::Js, tmp.path(), "4.5.4");

        let parsed: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("package.json")).unwrap()
        ).unwrap();
        assert_eq!(parsed["version"], "4.5.4");
    }

    #[test]
    fn sync_kotlin_rewrites_top_level_version_line() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("build.gradle.kts"),
            "plugins { kotlin(\"multiplatform\") }\n\n\
             group = \"io.github.femimarket\"\n\
             version = \"4.5.3\"\n\n\
             kotlin { jvm() }\n",
        ).unwrap();

        sync_openapi_versions(RepoKind::Kotlin, tmp.path(), "v4.5.4");

        let after = std::fs::read_to_string(tmp.path().join("build.gradle.kts")).unwrap();
        assert!(after.contains("version = \"4.5.4\""));
        assert!(!after.contains("version = \"4.5.3\""));
        // Other lines preserved.
        assert!(after.contains("group = \"io.github.femimarket\""));
        assert!(after.contains("kotlin { jvm() }"));
    }

    #[test]
    fn sync_swift_is_a_noop() {
        // swift6 doesn't bake version into output — sync should leave the
        // dir alone. Use a sentinel file to confirm nothing was touched.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Package.swift"), "// original\n").unwrap();

        sync_openapi_versions(RepoKind::Swift, tmp.path(), "v9.9.9");

        let after = std::fs::read_to_string(tmp.path().join("Package.swift")).unwrap();
        assert_eq!(after, "// original\n");
    }

    #[test]
    fn sync_for_non_openapi_kind_is_a_noop() {
        // Rust / Android / Generic aren't openapi-generated by this
        // orchestrator. Make sure passing them through doesn't accidentally
        // rewrite Cargo.toml or anything else.
        let tmp = TempDir::new().unwrap();
        let cargo = "[package]\nname = \"x\"\nversion = \"4.5.3\"\n";
        std::fs::write(tmp.path().join("Cargo.toml"), cargo).unwrap();

        sync_openapi_versions(RepoKind::Rust, tmp.path(), "v9.9.9");
        sync_openapi_versions(RepoKind::Android, tmp.path(), "v9.9.9");
        sync_openapi_versions(RepoKind::Generic, tmp.path(), "v9.9.9");

        let after = std::fs::read_to_string(tmp.path().join("Cargo.toml")).unwrap();
        assert_eq!(after, cargo);
    }

    // ── expand_release_assets ──────────────────────────────────────────────

    #[test]
    fn expand_release_assets_resolves_literal_relative_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("artifact.bin"), b"data").unwrap();
        let globs = vec!["artifact.bin".to_string()];

        let assets = expand_release_assets(tmp.path(), &globs);

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].file_name().unwrap(), "artifact.bin");
        assert!(assets[0].is_absolute(), "globbed path is absolute on disk");
    }

    #[test]
    fn expand_release_assets_expands_wildcard_glob() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("foo.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("bar.bin"), b"b").unwrap();
        std::fs::write(tmp.path().join("baz.txt"), b"c").unwrap();
        let globs = vec!["*.bin".to_string()];

        let mut names: Vec<String> = expand_release_assets(tmp.path(), &globs)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();

        assert_eq!(names, vec!["bar.bin".to_string(), "foo.bin".to_string()]);
    }

    #[test]
    fn expand_release_assets_handles_absolute_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("artifact.bin"), b"data").unwrap();
        // Absolute pattern — repo_path is irrelevant.
        let abs = tmp.path().join("artifact.bin").to_string_lossy().into_owned();
        let globs = vec![abs.clone()];

        let assets = expand_release_assets(Path::new("/var"), &globs);

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0], std::path::PathBuf::from(abs));
    }

    #[test]
    fn expand_release_assets_yields_empty_when_nothing_matches() {
        let tmp = TempDir::new().unwrap();
        let globs = vec!["nonexistent-*.bin".to_string()];

        let assets = expand_release_assets(tmp.path(), &globs);

        assert!(assets.is_empty());
    }

    #[test]
    fn expand_release_assets_multiple_globs_combined() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"a").unwrap();
        std::fs::write(tmp.path().join("b.txt"), b"b").unwrap();
        let globs = vec!["*.bin".to_string(), "*.txt".to_string()];

        let mut names: Vec<String> = expand_release_assets(tmp.path(), &globs)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();

        assert_eq!(names, vec!["a.bin".to_string(), "b.txt".to_string()]);
    }

    #[test]
    fn expand_release_assets_recurses_with_double_star() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("dist/sub")).unwrap();
        std::fs::write(tmp.path().join("dist/sub/inner.bin"), b"x").unwrap();
        std::fs::write(tmp.path().join("dist/top.bin"), b"y").unwrap();
        let globs = vec!["dist/**/*.bin".to_string()];

        let mut paths: Vec<String> = expand_release_assets(tmp.path(), &globs)
            .into_iter()
            .map(|p| {
                // Just compare relative-to-tmp for stability.
                p.strip_prefix(tmp.path()).unwrap().to_string_lossy().into_owned()
            })
            .collect();
        paths.sort();

        assert_eq!(
            paths,
            vec!["dist/sub/inner.bin".to_string(), "dist/top.bin".to_string()],
        );
    }

    // ── build_gh_release_args ──────────────────────────────────────────────

    #[test]
    fn build_gh_release_args_no_assets() {
        let args = build_gh_release_args("v1.0.0", &[]);
        // Must start with `release create <tag>` — that's the gh CLI shape.
        assert_eq!(args[0], "release");
        assert_eq!(args[1], "create");
        assert_eq!(args[2], "v1.0.0");
        // Title and generate-notes are required for non-interactive use.
        assert!(args.contains(&"--title".to_string()));
        assert!(args.contains(&"v1.0.0".to_string()));  // both as tag and as title
        assert!(args.contains(&"--generate-notes".to_string()));
    }

    #[test]
    fn build_gh_release_args_with_assets_appends_paths() {
        let assets = vec![
            std::path::PathBuf::from("/tmp/a.bin"),
            std::path::PathBuf::from("/tmp/b.bin"),
        ];
        let args = build_gh_release_args("v1.2.3", &assets);
        assert_eq!(args[0], "release");
        assert_eq!(args[1], "create");
        assert_eq!(args[2], "v1.2.3");
        assert!(args.iter().any(|a| a == "/tmp/a.bin"));
        assert!(args.iter().any(|a| a == "/tmp/b.bin"));
    }
}
