use anyhow::{anyhow, Context, Result};

pub fn run() -> Result<()> {
    let repo = git2::Repository::open(".").context("opening git repo")?;
    // Unborn branch (no commits yet) → no HEAD ref; diff against the empty tree.
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let index = repo.index().context("getting index")?;
    let diff = repo.diff_tree_to_index(head_tree.as_ref(), Some(&index), None)
        .context("diffing HEAD to index")?;
    if diff.deltas().count() == 0 {
        return Err(anyhow!("nothing staged"));
    }

    let files: Vec<(String, bool)> = diff.deltas().filter_map(|d| {
        let path = d.new_file().path().or_else(|| d.old_file().path())?
            .to_string_lossy().into_owned();
        let is_binary = d.new_file().is_binary() || d.old_file().is_binary();
        Some((path, is_binary))
    }).collect();

    let is_ignored = |path: &str| {
        buidl::IGNOREFILES.iter().any(|pat| {
            glob::Pattern::new(pat).ok().is_some_and(|p| p.matches(path))
        })
    };
    let is_md = |path: &str| path.to_lowercase().ends_with(".md");

    let has_md = files.iter().any(|(p, _)| is_md(p));
    let has_real = files.iter().any(|(p, b)| !is_md(p) && !is_ignored(p) && !b);

    // Diagnostic: print every staged file with its category so we can SEE why
    // the prompt is sized the way it is (binaries slipping through? ignored
    // pattern not matching?). To stderr so commit's stdout (the message)
    // stays scripting-friendly.
    eprintln!("[buidl] {} staged file(s):", files.len());
    for (p, b) in &files {
        let cat = if *b { "binary" }
            else if is_ignored(p) { "ignored" }
            else if is_md(p) { "md" }
            else { "REAL" };
        eprintln!("[buidl]   {cat:>7}  {p}");
    }

    if !has_real {
        let msg = if has_md { "docs: update documentation" } else { "chore: update" };
        println!("{msg}");
        return Ok(());
    }

    let mut opts = git2::DiffOptions::new();
    for (path, bin) in &files {
        if !is_md(path) && !is_ignored(path) && !bin {
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

    println!("{message}");
    Ok(())
}
