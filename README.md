# buidl

`buidl` is a JSON-driven build orchestrator for Rust, Swift, Kotlin, Android, JavaScript, and generic repositories. It automates the release pipeline: detecting language, generating Conventional Commits messages via a local LLM, bumping semver tags, regenerating READMEs, pushing to GitHub, and optionally publishing to GitHub Packages or creating GitHub Releases.

It is designed for a "local is prod" workflow where `buidl` manages multiple local git checkouts defined in a single `build.json` configuration file.

## Features

- **Language Auto-Detection**: Automatically identifies Swift, Kotlin, Android, JS, Rust, or Generic repos based on build files (`Cargo.toml`, `Package.swift`, `build.gradle.kts`, `package.json`).
- **LLM-Driven Commits**: Uses a local LM Studio instance to generate Conventional Commits messages from staged diffs.
- **LLM-Driven READMEs**: Optionally regenerates `README.md` from source code using a local LLM.
- **OpenAPI Code Generation**: Supports `kind: "openapi"` entries to wipe a repo, fetch an OpenAPI spec, run `openapi-generator`, and sync version markers.
- **Automated Semver Bumping**: Computes new tags based on commit types (`feat` → minor, `fix` → patch, `!`/BREAKING → major).
- **GitHub Integration**: Pushes code, tags, and optionally creates GitHub Releases with assets.
- **Gradle Publishing**: Automatically publishes Android/Kotlin libraries to GitHub Packages.

## Prerequisites

- **Rust Toolchain**: To build `buidl`.
- **Git**: Installed and configured.
- **LM Studio**: Running locally on `http://localhost:1234` with a compatible model (e.g., `qwen/qwen3.6-35b-a3b`).
- **OpenAPI Generator**: Installed on PATH (`openapi-generator`) if using `kind: "openapi"`.
- **GitHub CLI (`gh`)**: Installed and authenticated if using `release` assets.
- **Gradle**: Installed if using `kind: "openapi"` with Kotlin templates or `RepoKind::Android`/`Kotlin` publishing.

## Installation

```bash
git clone <repository-url>
cd buidl
cargo build --release
```

## Configuration

`buidl` is driven by a `build.json` file (default: `./build.json`). It accepts a list of repository entries.

### Structure

```json
[
  {
    "path": "/absolute/path/to/repo",
    "remote": "https://github.com/user/repo",
    "kind": "openapi",
    "data": {
      "templates": "/path/to/templates",
      "deleteAllExceptGit": true
    },
    "readme": {
      "glob": ["*.swift", "Sources/**/*.kt"]
    },
    "commitIgnore": {
      "glob": ["*.lock", "dist/**"]
    },
    "release": {
      "glob": ["dist/*.bin", "artifacts/*.tar.gz"]
    }
  }
]
```

### Fields

- **`path`** (required): Absolute path to the local git repository.
- **`remote`** (optional): Informational only. The actual push URL is derived from the repo's git config.
- **`kind`** (optional):
  - `"openapi"`: Triggers a pre-wipe, spec fetch, and `openapi-generator` pass before the standard commit flow. Requires `data` field.
  - Absent or other: Standard commit/README/push/tag flow.
- **`data`** (required if `kind: "openapi"`):
  - `templates`: Path to the openapi-generator templates directory.
  - `deleteAllExceptGit`: If `true`, wipes the repo directory (except `.git/`) before generation.
- **`readme`** (optional):
  - `glob`: List of glob patterns. Only files matching these patterns are fed to the LLM for README regeneration. If absent, README regeneration is skipped.
- **`commitIgnore`** (optional):
  - `glob`: List of glob patterns. Files matching these are excluded from the commit-message LLM prompt (e.g., to exclude lockfiles).
- **`release`** (optional):
  - `glob`: List of glob patterns for assets to attach to the GitHub Release.

### Glob Semantics

- Patterns without `/` (e.g., `*.swift`) match anywhere in the repo.
- Patterns with `/` (e.g., `Sources/*.swift`) match exactly as specified.
- `**` matches recursively.

## Usage

### Running

```bash
# Use default build.json
./target/release/buidl

# Use custom config
./target/release/buidl --config /path/to/build.json
```

### Workflow

For each entry in `build.json`:

1. **OpenAPI Setup** (if `kind: "openapi"`):
   - Fetches the OpenAPI spec from `https://localhost:443/api-docs/openapi.json` (hardcoded in `src/lib.rs`).
   - Wipes the repo (if `deleteAllExceptGit: true`).
   - Runs `openapi-generator` with the specified templates.
   - Syncs version markers in generated files (`package.json`, `build.gradle.kts`, `README.md`) to match the new tag.

2. **Stage & Diff**:
   - Stages all changes.
   - Computes the diff against HEAD.
   - If no changes, skips the entry.

3. **Commit Message**:
   - Filters out binary files and `commitIgnore` globs.
   - Sends the diff to LM Studio to generate a Conventional Commits message.
   - Validates the message format.

4. **Version Bump**:
   - Computes the new semver tag based on the commit type (`feat` → minor, `fix` → patch, `!` → major).

5. **README Regeneration** (if `readme` field present):
   - Filters tracked files by `readme.glob`.
   - Sends source code to LM Studio to generate a fresh `README.md`.
   - Stages the new README.

6. **Commit & Push**:
   - Creates a commit with the generated message.
   - Pushes to `origin main`.
   - Creates and pushes the new tag.

7. **Publishing**:
   - **Android/Kotlin**: Runs `./gradlew publishAllPublicationsToGitHubPackagesRepository`.
   - **Swift/Rust/JS/Generic**: No extra step; the tag is the artifact.

8. **GitHub Release** (if `release` field present):
   - Creates a GitHub Release with the new tag.
   - Uploads assets matching `release.glob`.

## Architecture

### Key Files

- **`src/main.rs`**: CLI entry point. Parses `build.json`, iterates over entries, and orchestrates the pipeline.
- **`src/lib.rs`**: Core logic.
  - `detect_kind`: Auto-detects language from build files.
  - `commit_msg_for_diff`: Interfaces with LM Studio for commit messages.
  - `regenerate_readme`: Interfaces with LM Studio for README generation.
  - `compute_new_tag`: Semver bumping logic.
  - `run_openapi_generator_for_templates`: Spawns `openapi-generator`.
  - `sync_openapi_versions`: Patches version strings in generated files.
  - `gh_release_create`: Spawns `gh release create`.
  - `gradle_publish_github_packages`: Spawns `gradlew`.

### LLM Integration

`buidl` communicates with LM Studio via HTTP POST to `http://localhost:1234/v1/chat/completions`.

- **Model**: `qwen/qwen3.6-35b-a3b` (hardcoded).
- **Timeout**: 30 minutes (1800 seconds) to accommodate large diffs.
- **Temperature**: 0.2 for deterministic output.
- **Prompt Engineering**:
  - Commit messages: Prompt includes the unified diff. Excludes binary files and `commitIgnore` globs.
  - READMEs: Prompt includes file contents of files matching `readme.glob`. Excludes the existing `README.md` to prevent anchoring.

### OpenAPI Generator

- **Spec Source**: `https://localhost:443/api-docs/openapi.json` (hardcoded in `src/lib.rs::SPEC`).
- **Templates**: Supported templates are inferred from the `templates` path:
  - `swift6`: Swift 6 client.
  - `typescript-fetch`: TypeScript/JS client.
  - `multiplatform` (under `libraries/`): Kotlin Multiplatform client.
- **Version Sync**: To prevent infinite version bumps, `buidl` syncs the new tag version into generated files (`package.json`, `build.gradle.kts`, `README.md`) after generation.

## Testing

```bash
cargo test
```

Tests cover:
- Pure functions (`bump_kind`, `matches_any_glob`, `select_commit_paths`, etc.).
- JSON deserialization.
- Filesystem operations (`detect_kind`, `wipe_except_git`).
- Git operations (`last_tag`, `compute_new_tag`) using temp repositories.
- LLM guardrails (ensuring empty diffs/globs don't trigger LLM calls).

## Non-Obvious Conventions

- **Binary Files**: Always excluded from LLM prompts.
- **Commit Message Validation**: If the LLM returns a non-Conventional Commits message, `buidl` panics.
- **Empty Diff**: If all staged files are binary or ignored, `buidl` uses a hardcoded `chore: update` commit message.
- **README Regeneration**: If no files match `readme.glob`, the existing README is left untouched.
- **OpenAPI Version Sync**: Critical for preventing drift between the generated code's version and the git tag.
- **Gradle Wrapper Patching**: For Kotlin multiplatform, `buidl` patches the generated `gradlew` permissions and `gradle-wrapper.properties` to ensure compatibility with newer JDKs.