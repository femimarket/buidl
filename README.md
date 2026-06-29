# buidl

A JSON-driven build orchestrator that automates the full release pipeline for multi-language repositories. It stages changes, generates Conventional Commits messages via a local LLM, auto-regenerates READMEs, bumps semantic versions, pushes to Git, and optionally publishes to GitHub Packages or creates GitHub releases. Language detection is automatic based on build-tool signatures on disk.

## Overview

`buidl` reads a flat `build.json` configuration and iterates over each repository entry. For every repo it:
1. Stages all tracked changes.
2. Filters the diff (excluding binaries and `commitIgnore` globs).
3. Sends the filtered diff to a local LLM to generate a Conventional Commits message.
4. Computes the next semantic version tag based on the commit type.
5. Optionally regenerates `README.md` using only files matching `readme.glob`.
6. Commits, pushes to `origin/main`, and creates a lightweight tag.
7. Runs language-specific publish steps (Gradle for Android/Kotlin).
8. Optionally creates a GitHub release with asset globs.

If no changes are staged, the entry is skipped with `ℹ️ No changes to commit. Nothing to release.`

## Prerequisites

- **Rust toolchain** (edition 2024)
- **`git` CLI** (for push/tag operations)
- **`gh` CLI** (authenticated, for GitHub releases)
- **`openapi-generator` CLI** (required when `kind: "openapi"`)
- **`./gradlew`** (required for Android/Kotlin publishing)
- **LM Studio** running locally on `localhost:1234` (serves the LLM API)

## Installation & Build

```bash
git clone <repo-url>
cd buidl
cargo build --release
```

The binary will be available at `target/release/buidl`.

## Configuration

Configuration is driven by a single `build.json` file (default path, overridable via `--config`). It expects a JSON array of `Entry` objects. Each entry maps to a local Git repository checkout.

### Entry Schema

| Field | Type | Description |
|-------|------|-------------|
| `path` | `string` | Absolute path to the local repository checkout. |
| `remote` | `string?` | Optional `origin` remote URL. If absent, existing git config is used. |
| `kind` | `string?` | Set to `"openapi"` to trigger a pre-wipe + OpenAPI generator pass before the standard commit flow. |
| `data` | `object?` | Required when `kind: "openapi"`. Contains `templates` (path to generator templates) and `deleteAllExceptGit` (boolean). |
| `readme` | `object?` | Opt-in README regeneration. Contains `glob` (array of glob patterns). `README.md` is always excluded from the prompt to prevent anchoring. |
| `commitIgnore` | `object?` | Opt-in diff filtering. Contains `glob` (array of glob patterns). Files matching these patterns are excluded from the commit-message LLM prompt. |
| `release` | `object?` | Opt-in GitHub release creation. Contains `glob` (array of asset paths/globs). Requires `gh` CLI. |

### Example `build.json`

```json
[
  {
    "path": "/Users/u/rustapps/buidl",
    "remote": "https://github.com/femimarket/buidl",
    "readme": { "glob": ["*.rs", "*.toml"] },
    "commitIgnore": { "glob": ["*.lock"] }
  },
  {
    "path": "/Users/u/swiftapps/LyricEditor",
    "remote": "https://github.com/femimarket/SwiftLyricEditor",
    "readme": { "glob": ["*.swift"] },
    "release": { "glob": ["qwen3-aligner-0.6b"] }
  },
  {
    "path": "/Users/u/openapi/jsapi",
    "kind": "openapi",
    "data": {
      "templates": "/tpl/typescript-fetch",
      "deleteAllExceptGit": true
    }
  }
]
```

## Usage & Workflow

Run the orchestrator from any directory:

```bash
./target/release/buidl
# or with a custom config:
./target/release/buidl --config /path/to/pipeline.json
```

### Per-Repository Flow

1. **OpenAPI Pre-processing** (if `kind: "openapi"`): Reads the last existing tag, optionally wipes the working directory (keeping `.git/`), and runs `openapi-generator generate`. The generator's output version is seeded with the last tag to prevent drift.
2. **Language Detection**: Auto-detects `RepoKind` by scanning for `Cargo.toml`, `Package.swift`, `.xcodeproj`, `build.gradle.kts` (Android vs Kotlin multiplatform), or `package.json`. Falls back to `Generic`.
3. **Staging & Diffing**: Stages all files respecting `.gitignore`. Computes the diff against HEAD.
4. **Commit Message Generation**: Filters out binary files and `commitIgnore` globs. Sends the unified patch to LM Studio. Validates the response against Conventional Commits types. Falls back to `chore: update` if the filtered diff is empty.
5. **Version Bumping**: Parses the commit header. `feat` → minor, `!` or `BREAKING CHANGE` → major, everything else → patch. Computes the next tag (defaults to `v0.1.0` if no tags exist).
6. **OpenAPI Version Sync** (if applicable): Rewrites version markers in `package.json`, `README.md`, and `build.gradle.kts` to match the new tag, preventing infinite bump loops on subsequent runs.
7. **README Regeneration** (if `readme` is present): Feeds matching source files to LM Studio. Strips outer markdown fences. Writes and re-stages `README.md`.
8. **Commit & Push**: Creates a commit, pushes to `origin/main`, and pushes the new tag.
9. **Publishing**: Runs `./gradlew publishAllPublicationsToGitHubPackagesRepository` for Android/Kotlin. Swift, Rust, JS, and Generic repos treat the Git tag as the artifact.
10. **GitHub Release** (if `release` is present): Expands asset globs and runs `gh release create <tag> --title <tag> --generate-notes [assets...]`.

## Architecture & Key Files

- `src/main.rs`: CLI entry point. Parses arguments, loads `build.json`, and drives the per-repo orchestration loop.
- `src/lib.rs`: Core library. Contains LLM communication (`lm`), git operations (`stage_all`, `diff_files`, `commit_with_msg`, `tag_and_push`), versioning logic (`bump_kind`, `compute_new_tag`), glob matching (`matches_any_glob`), OpenAPI sync (`sync_openapi_versions`), and release creation (`gh_release_create`).
- `tests/integration.rs`: Filesystem and Git-backed integration tests. Validates `detect_kind`, `wipe_except_git`, `last_tag`, `compute_new_tag`, and guards against empty-diff LLM invocations.
- `Cargo.toml`: Project metadata and dependencies (`clap`, `git2`, `glob`, `reqwest`, `serde`, `serde_json`).

## Non-Obvious Conventions & Gotchas

- **LLM Endpoint & Model**: The orchestrator hardcodes `http://localhost:1234/v1/chat/completions` and the model `qwen/qwen3.6-35b-a3b` in `src/lib.rs`. LM Studio must be running and serving this model.
- **Timeouts**: LLM requests have a 30-minute timeout (`Duration::from_secs(1800)`) to accommodate large diff regenerations.
- **Hardcoded Branch & Remote**: Push operations always target `origin/main`. If your default branch differs, you must modify `git_push_main` in `src/lib.rs`.
- **`commitIgnore` Semantics**: Globs without `/` are anchored anywhere in the repo tree (e.g., `*.lock` matches `Cargo.lock` and `node_modules/.package-lock.json`). Use anchored globs like `dist/**` for precise scoping.
- **README Anchoring Prevention**: `README.md` is explicitly excluded from the `readme.glob` filter before being sent to the LLM. This forces the model to generate fresh documentation rather than editing the existing file.
- **OpenAPI Version Sync**: JS and Kotlin OpenAPI generators bake the seed version into output files. `sync_openapi_versions` patches these to the new tag immediately after generation. The caller must re-stage the index afterward (handled automatically in `main.rs`).
- **Gradle Wrapper Patching**: The Kotlin multiplatform generator ships with a `gradlew` that is `chmod 644` and pins Gradle 8.14.3 (which crashes on JDK 25). `run_openapi_generator_for_templates` automatically patches permissions and bumps the wrapper to Gradle 9.1.0.
- **Empty Diff Guard**: If `commitIgnore` or binary filtering removes all files from the prompt, `commit_msg_for_diff` returns `chore: update` instead of invoking the LLM. This prevents libgit2's "no pathspecs = include everything" behavior from accidentally sending the full diff.
- **Tag Parsing**: `compute_new_tag` and `last_tag` accept both `vX.Y.Z` and bare `X.Y.Z` formats. Non-semver tags are silently ignored during version resolution.

## Testing

Run the full test suite (unit + integration):

```bash
cargo test
```

- **Unit tests** in `src/lib.rs` cover pure functions: `bump_kind`, `strip_outer_fence`, `matches_any_glob`, `select_commit_paths`, `select_readme_paths`, `sync_openapi_versions`, `expand_release_assets`, and `build_gh_release_args`.
- **Integration tests** in `tests/integration.rs` use `tempfile::TempDir` to validate filesystem operations, Git tag resolution, and early-return guards for empty diffs. No external services are required to run tests.