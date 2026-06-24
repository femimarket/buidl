use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

pub fn run() -> Result<()> {
    let repo = git2::Repository::open(".").context("opening git repo")?;

    // Stage everything but skip IGNOREFILES patterns at the staging step so
    // build output (`.build/`, `DerivedData/`), Xcode user state
    // (`xcuserdata/`, `.xcuserstate`), and lockfiles never enter the index —
    // otherwise they'd both show up in the diff (forcing spurious `chore:
    // update` commits) AND get bundled into the commit body.
    let mut index = repo.index().context("getting index")?;
    index.add_all(
        ["*"],
        git2::IndexAddOption::DEFAULT,
        Some(&mut |p: &Path, _spec: &[u8]| -> i32 {
            if buidl::is_ignored(&p.to_string_lossy()) { 1 } else { 0 }
        }),
    ).context("staging all")?;
    index.write().context("writing index")?;

    // Unborn branch (initial commit run) → no HEAD ref; diff against empty tree.
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let diff = repo.diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
        .context("diffing HEAD to index")?;
    if diff.deltas().count() == 0 {
        println!("ℹ️ No changes to commit. Nothing to release.");
        return Ok(());
    }

    let files: Vec<(String, bool)> = diff.deltas().filter_map(|d| {
        let path = d.new_file().path().or_else(|| d.old_file().path())?
            .to_string_lossy().into_owned();
        let is_binary = d.new_file().is_binary() || d.old_file().is_binary();
        Some((path, is_binary))
    }).collect();

    let is_md = |path: &str| path.to_lowercase().ends_with(".md");

    let has_md = files.iter().any(|(p, _)| is_md(p));
    let has_real = files.iter().any(|(p, b)| !is_md(p) && !buidl::is_ignored(p) && !b);

    eprintln!("[buidl] {} staged file(s):", files.len());
    for (p, b) in &files {
        let cat = if *b { "binary" }
            else if is_md(p) { "md" }
            else { "REAL" };
        eprintln!("[buidl]   {cat:>7}  {p}");
    }

    let commit_msg = if !has_real {
        if has_md { "docs: update documentation".to_string() }
        else { "chore: update".to_string() }
    } else {
        let mut opts = git2::DiffOptions::new();
        for (path, bin) in &files {
            if !is_md(path) && !buidl::is_ignored(path) && !bin {
                opts.pathspec(path);
            }
        }
        let filtered = repo.diff_tree_to_index(head_tree.as_ref(), Some(&index), Some(&mut opts))
            .context("computing filtered diff")?;
        let mut diff_text = String::new();
        filtered.print(git2::DiffFormat::Patch, |_d, _h, line| {
            let content = std::str::from_utf8(line.content()).unwrap_or("");
            match line.origin() {
                '+' | '-' | ' ' => {
                    diff_text.push(line.origin());
                    diff_text.push_str(content);
                }
                _ => diff_text.push_str(content),
            }
            true
        }).context("rendering filtered diff")?;

        let prompt = format!(
            "Write a Conventional Commits message (type(scope): description). \
             Types: feat|fix|docs|style|refactor|perf|test|build|ci|chore. \
             Output a single line, no quotes, no markdown, no explanation.\n\nDIFF:\n{diff_text}"
        );
        let message = buidl::lm(&prompt)?;

        let first_line = message.lines().next().unwrap_or("");
        let Some(colon) = first_line.find(':') else {
            return Err(anyhow!("model did not return Conventional Commits format: {message:?}"));
        };
        let prefix = first_line[..colon].trim_end_matches('!');
        let type_part = prefix.split_once('(').map(|(t, _)| t).unwrap_or(prefix);
        let types = ["feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore"];
        if !types.contains(&type_part) {
            return Err(anyhow!("model returned non-Conventional type {type_part:?} in: {message:?}"));
        }
        message
    };

    let first_line = commit_msg.lines().next().unwrap_or("");
    let head = first_line.split(':').next().unwrap_or("");
    let bump_kind: &str = if head.ends_with('!') || commit_msg.contains("BREAKING CHANGE") {
        "major"
    } else {
        let type_part = head.split_once('(').map(|(t, _)| t).unwrap_or(head);
        if type_part == "feat" { "minor" } else { "patch" }
    };

    let last = repo.tag_names(None).context("listing tags")?
        .iter().flatten().flatten()
        .filter_map(|t| {
            let v = t.strip_prefix('v').unwrap_or(t);
            let mut p = v.split('.');
            Some((p.next()?.parse::<u32>().ok()?,
                  p.next()?.parse::<u32>().ok()?,
                  p.next()?.parse::<u32>().ok()?))
        })
        .max();
    let new_tag = match (last, bump_kind) {
        (None, _) => buidl::DEFAULT_VERSION.to_string(),
        (Some((major, _, _)), "major") => format!("v{}.0.0", major + 1),
        (Some((major, minor, _)), "minor") => format!("v{major}.{}.0", minor + 1),
        (Some((major, minor, patch)), _) => format!("v{major}.{minor}.{}", patch + 1),
    };

    // Regenerate README.md from the full codebase so docs stay in sync with
    // code. Gated on `has_real` so a README-only or chore-only commit doesn't
    // trigger another regen → another commit → infinite loop.
    if has_real {
        buidl::regenerate_readme(&mut index).context("regenerating README.md")?;
    }

    println!("🚀 Publishing {new_tag} to GitHub...");
    println!("{commit_msg}");

    let tree_oid = index.write_tree().context("writing tree from index")?;
    let tree = repo.find_tree(tree_oid).context("finding tree")?;
    let sig = repo.signature().context("getting signature from git config")?;
    // Initial commit on an unborn branch has no parent.
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, &commit_msg, &tree, &parents)
        .context("creating commit")?;

    println!("⏳ Pushing code...");
    if !Command::new("git").args(["push", "origin", "main"]).status().context("git push")?.success() {
        return Err(anyhow!("git push failed"));
    }

    println!("🏷️ Creating tag {new_tag}...");
    let head_commit = repo.head().context("getting HEAD")?
        .peel_to_commit().context("peeling HEAD to commit")?;
    repo.tag_lightweight(&new_tag, head_commit.as_object(), false)
        .context("creating tag")?;

    println!("⏳ Pushing tag...");
    if !Command::new("git").args(["push", "origin", &new_tag]).status().context("git push tag")?.success() {
        return Err(anyhow!("git push tag failed"));
    }

    println!("✅ Successfully pushed {new_tag} to GitHub!");
    Ok(())
}
