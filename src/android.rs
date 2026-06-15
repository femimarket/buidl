use anyhow::{anyhow, Context, Result};
use std::process::Command;

pub fn run() -> Result<()> {
    // Deterministic next tag: patch bump off `git describe --tags --abbrev=0`.
    // No tags yet → v1.0.0. (Patch bump per android script intent — no minor/major
    // decision here, unlike the spec-driven flow in build.sh.)
    let last_out = Command::new("git").args(["describe", "--tags", "--abbrev=0"]).output()
        .context("running git describe")?;
    let last_tag = if last_out.status.success() {
        String::from_utf8(last_out.stdout).context("describe output not utf8")?.trim().to_string()
    } else {
        String::new()
    };
    let new_tag = if last_tag.is_empty() {
        "v1.0.0".to_string()
    } else {
        let ver = last_tag.strip_prefix('v').unwrap_or(&last_tag);
        let mut parts = ver.split('.');
        let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        format!("v{major}.{minor}.{}", patch + 1)
    };

    println!("🚀 Publishing {new_tag} to GitHub...");

    if !Command::new("git").args(["add", "."]).status().context("git add")?.success() {
        return Err(anyhow!("git add failed"));
    }

    // `git diff-index --quiet HEAD` exits 0 if no changes, 1 if changes.
    // Only commit when something's actually staged-vs-HEAD.
    let has_changes = !Command::new("git").args(["diff-index", "--quiet", "HEAD"])
        .status().context("git diff-index")?.success();
    if has_changes {
        let commit_msg = buidl::lm()?;
        println!("{commit_msg}");
        if !Command::new("git").args(["commit", "-m", &commit_msg]).status().context("git commit")?.success() {
            return Err(anyhow!("git commit failed"));
        }
    } else {
        println!("ℹ️ No new code changes detected. Just generating a new release tag.");
    }

    println!("⏳ Pushing code...");
    if !Command::new("git").args(["push", "origin", "main"]).status().context("git push")?.success() {
        return Err(anyhow!("git push failed"));
    }

    // Maven coordinate version (no v prefix) gets passed to gradle via -P so
    // mavenPublishing's `version` is whatever we just decided to tag.
    let maven_version = new_tag.strip_prefix('v').unwrap_or(&new_tag);
    println!("📦 Building, signing, and publishing v{maven_version} to Maven Central...");
    if !Command::new("./gradlew")
        .args([
            "publishAndReleaseToMavenCentral",
            &format!("-PlibraryVersion={maven_version}"),
            "--no-configuration-cache",
        ])
        .status().context("./gradlew publishAndReleaseToMavenCentral")?
        .success()
    {
        return Err(anyhow!("./gradlew publishAndReleaseToMavenCentral failed"));
    }

    println!("🏷️ Creating tag {new_tag}...");
    if !Command::new("git").args(["tag", &new_tag]).status().context("git tag")?.success() {
        return Err(anyhow!("git tag failed"));
    }

    println!("⏳ Pushing tag...");
    if !Command::new("git").args(["push", "origin", &new_tag]).status().context("git push tag")?.success() {
        return Err(anyhow!("git push tag failed"));
    }

    println!("✅ Successfully published v{maven_version} to Maven Central and pushed {new_tag} to GitHub!");
    Ok(())
}
