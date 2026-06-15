use anyhow::{anyhow, Context, Result};
use std::io::{self, Write};
use std::process::Command;
use std::time::Duration;

/// Ask LM Studio for a Conventional Commits message describing `git diff --cached`.
/// Validates the first line against the type whitelist before returning.
pub fn lm() -> Result<String> {
    let diff_out = Command::new("git").args(["diff", "--cached"]).output()
        .context("running git diff --cached")?;
    if !diff_out.status.success() {
        return Err(anyhow!("git diff --cached failed: {}",
            String::from_utf8_lossy(&diff_out.stderr)));
    }
    let diff = String::from_utf8(diff_out.stdout).context("git diff output not utf8")?;
    if diff.trim().is_empty() {
        return Err(anyhow!("nothing staged"));
    }

    print!("thinking... ");
    io::stdout().flush().ok();

    let body = serde_json::json!({
        "model": "qwen/qwen3.6-35b-a3b",
        "messages": [{
            "role": "user",
            "content": format!(
                "Write a Conventional Commits message (type(scope): description). \
                 Types: feat|fix|docs|style|refactor|perf|test|build|ci|chore. \
                 Output a single line, no quotes, no markdown, no explanation.\n\nDIFF:\n{diff}"
            ),
        }],
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
    let message = resp.pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("LM Studio response missing choices[0].message.content: {resp}"))?
        .trim()
        .to_string();
    println!();

    if message.is_empty() {
        return Err(anyhow!("LM Studio returned empty message"));
    }

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

    Ok(message)
}
