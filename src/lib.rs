
use anyhow::{anyhow, Context, Result};
use std::io::{self, Write};
use std::time::Duration;

/// Glob patterns matched against staged-file paths. Anything matching is
/// filtered out of LLM diff prompts — pure churn with zero signal for
/// commit-message generation. `.md` is intentionally NOT here so we can
/// distinguish docs-only commits (→ `docs:`) from noise-only commits (→ `chore:`).
pub const IGNOREFILES: &[&str] = &[
    // OS metadata that finder/explorer drops into every directory it touches.
    // Should never reach git but does — strip at the staging step.
    "**/.DS_Store", "**/._*", "**/.AppleDouble", "**/.LSOverride",
    "**/Thumbs.db", "**/Desktop.ini",
    "**/.fseventsd/**", "**/.Spotlight-V100/**", "**/.Trashes/**",
    "**/.DocumentRevisions-V100/**", "**/.TemporaryItems/**",
    // Lockfiles
    "**/Cargo.lock", "**/package-lock.json", "**/pnpm-lock.yaml",
    "**/yarn.lock", "**/bun.lock", "**/bun.lockb", "**/go.sum",
    "**/Gemfile.lock", "**/Package.resolved",
    // Xcode project plumbing — committed, huge, semantically opaque.
    "**/*.pbxproj", "**/*.xcscheme", "**/*.xcworkspacedata",
    "**/*.xcuserstate", "**/*.storyboard", "**/*.xib", "**/*.strings",
    "**/*.xcassets/**",
    // Xcode user data — per-user UI state, breakpoints, schemes. Xcode
    // rewrites these constantly so they show up as changes every run even
    // when nothing real changed.
    "**/xcuserdata/**", "**/*.xcuserdatad/**",
    // Build output dirs — local-only, must never reach the repo. `.build/` is
    // SwiftPM's intermediates (tens of thousands of files); `DerivedData` is
    // Xcode's; `target/` is cargo's; `node_modules/` is bun/npm's; `build/` is
    // gradle's.
    "**/.build/**", "**/DerivedData/**", "**/target/**",
    "**/node_modules/**", "**/build/**",
    // Build / codegen output we commit for consumption but the LLM gets nothing
    // useful from. `dist/` is the typescript-fetch compiled tree; the openapi
    // dirs are the generator's own bookkeeping.
    "**/dist/**",
    "**/.openapi-generator/**",
    "**/.openapi-generator-ignore",
    // Known-binary extensions. `is_binary()` from libgit2 is unreliable on the
    // `diff_tree_to_index` path (proven with a .wasm test) so we don't trust
    // it — hardcode every common binary format instead.
    // Images
    "**/*.png", "**/*.jpg", "**/*.jpeg", "**/*.gif", "**/*.webp",
    "**/*.heic", "**/*.heif", "**/*.ico", "**/*.icns", "**/*.bmp", "**/*.tiff",
    // Video
    "**/*.mp4", "**/*.mov", "**/*.m4v", "**/*.webm", "**/*.avi", "**/*.mkv",
    // Audio
    "**/*.mp3", "**/*.wav", "**/*.m4a", "**/*.aac", "**/*.ogg", "**/*.flac",
    // Documents
    "**/*.pdf", "**/*.doc", "**/*.docx", "**/*.xls", "**/*.xlsx",
    "**/*.ppt", "**/*.pptx",
    // Archives
    "**/*.zip", "**/*.tar", "**/*.gz", "**/*.tgz", "**/*.bz2", "**/*.7z", "**/*.rar",
    // Fonts
    "**/*.ttf", "**/*.otf", "**/*.woff", "**/*.woff2", "**/*.eot",
    // Compiled / executables / native libs
    "**/*.wasm", "**/*.exe", "**/*.dll", "**/*.so", "**/*.dylib",
    "**/*.a", "**/*.o", "**/*.class",
    // JVM / Android distributables
    "**/*.jar", "**/*.aar", "**/*.apk", "**/*.aab",
    // Disk images
    "**/*.dmg", "**/*.iso",
];

/// Second-layer ignore list applied ONLY when feeding the repo to the README
/// regen LLM. These files belong in git but bloat README-generation prompts:
/// test fixtures, raw API specs, vendored deps, license/changelog text, and
/// the README itself (feedback loop). IGNOREFILES still applies first.
pub const README_IGNOREFILES: &[&str] = &[
    "**/fixtures/**", "**/__fixtures__/**",
    "**/__snapshots__/**", "**/snapshots/**",
    "**/testdata/**", "**/test-data/**",
    "**/openapi.json", "**/swagger.json", "**/api-docs.json",
    "**/vendor/**", "**/third_party/**",
    "**/CHANGELOG.md", "**/CHANGELOG",
    "**/LICENSE", "**/LICENSE.md", "**/LICENSE.txt",
    // The README itself — feeding the current README back to the model
    // anchors it to the existing structure and defeats the point of regen.
    "**/README.md", "**/README",
];

/// First tag a release subcommand cuts when no tag exists yet. Pre-1.0 signals
/// the API isn't stable — bump major to 1.0.0 when you'd feel bad breaking it.
pub const DEFAULT_VERSION: &str = "v0.1.0";

/// True if the given repo-relative path matches any IGNOREFILES glob pattern.
/// Used both at staging time (filter before files enter the index) and at
/// diff classification time (decide REAL vs ignored for commit-message
/// generation).
pub fn is_ignored(path: &str) -> bool {
    IGNOREFILES.iter().any(|pat| {
        glob::Pattern::new(pat).ok().is_some_and(|p| p.matches(path))
    })
}

/// True if a path should be excluded from the README regen prompt. Combines
/// IGNOREFILES (universal noise) with README_IGNOREFILES (README-specific
/// noise). Anything not excluded by this is fed to the LLM verbatim.
pub fn is_readme_excluded(path: &str) -> bool {
    if is_ignored(path) { return true; }
    README_IGNOREFILES.iter().any(|pat| {
        glob::Pattern::new(pat).ok().is_some_and(|p| p.matches(path))
    })
}

/// Per-file size cap when collecting repo contents for the README prompt.
/// Anything larger gets skipped — a single huge generated file would blow the
/// model's context budget and yield no useful README content anyway.
const README_FILE_MAX_BYTES: usize = 100_000;

/// Read everything in the current index, filter through `is_readme_excluded`
/// and the size cap, ask LM Studio to regenerate README.md from the whole
/// codebase, write it to disk, and re-stage. Caller decides whether to invoke
/// (typically gated on `has_real` so README-only commits don't loop).
pub fn regenerate_readme(index: &mut git2::Index) -> Result<()> {
    let paths: Vec<String> = index.iter()
        .filter_map(|e| std::str::from_utf8(&e.path).ok().map(str::to_string))
        .filter(|p| !is_readme_excluded(p))
        .collect();

    let mut sections: Vec<String> = Vec::with_capacity(paths.len());
    let mut included: Vec<(String, usize)> = Vec::with_capacity(paths.len());
    for path in &paths {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.len() > README_FILE_MAX_BYTES { continue; }
        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s.to_string(),
            Err(_) => continue,
        };
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
         full repository source is provided (test fixtures, license, vendored \
         code, and the existing README excluded — so write fresh, don't \
         anchor to anything pre-existing). \n\n\
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
    let new_readme = lm(&prompt)?;
    let cleaned = strip_outer_fence(&new_readme);
    std::fs::write("README.md", cleaned).context("writing README.md")?;
    index.add_path(std::path::Path::new("README.md")).context("re-staging README.md")?;
    index.write().context("writing index after README regen")?;
    Ok(())
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
pub fn lm(prompt: &str) -> Result<String> {
    // chars/4 is the standard rough estimate for English/code tokens.
    eprintln!("[buidl] prompt: {} chars (~{} tokens)", prompt.len(), prompt.len() / 4);
    print!("thinking... ");
    io::stdout().flush().ok();

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
        .context("building HTTP client")?
        .post("http://localhost:1234/v1/chat/completions")
        .json(&body)
        .send().context("POST to LM Studio (is it running on localhost:1234?)")?
        .error_for_status().context("LM Studio HTTP error")?
        .json().context("decoding LM Studio response")?;
    println!();

    let message = resp.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("LM Studio response missing choices[0].message.content: {resp}"))?
        .trim()
        .to_string();
    if message.is_empty() {
        return Err(anyhow!("LM Studio returned empty message"));
    }
    Ok(message)
}
