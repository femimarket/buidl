
use anyhow::{anyhow, Context, Result};
use std::io::{self, Write};
use std::time::Duration;

/// Glob patterns matched against staged-file paths. Anything matching is
/// filtered out of LLM diff prompts — pure churn with zero signal for
/// commit-message generation. `.md` is intentionally NOT here so we can
/// distinguish docs-only commits (→ `docs:`) from noise-only commits (→ `chore:`).
pub const IGNOREFILES: &[&str] = &[
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
        .timeout(Duration::from_secs(60))
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
