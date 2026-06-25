# buidl

`buidl` is a JSON-driven build orchestrator for Rust. It automates the release pipeline for multiple repositories defined in a single `build.json` configuration file.

It handles the full lifecycle of a release:
1.  **Auto-detection**: Identifies the language/type of each repository (Rust, Swift, Kotlin, Android, JS, or Generic) based on files on disk.
2.  **LLM-Assisted Commit Messages**: Generates Conventional Commits messages by analyzing the staged diff via a local LM Studio instance.
3.  **LLM-Assisted README Generation**: Optionally regenerates `README.md` files from scratch using the current codebase as context.
4.  **Semantic Versioning**: Automatically calculates the next version tag based on the commit type (`feat` → minor, `fix` → patch, `BREAKING` → major).
5.  **Publishing**: Pushes code, tags releases, and triggers language-specific publishing steps (e.g., Gradle for Android/Kotlin).
6.  **OpenAPI Generation**: Special support for regenerating client code from OpenAPI specs before committing.

## Installation

### Prerequisites

*   **Rust Toolchain**: `buidl` requires Rust edition 2024.
*   **Git**: Standard Git CLI is required for credential handling and pushing.
*   **LM Studio**: A local instance of LM Studio running on `http://localhost:1234` is required for commit message and README generation.
*   **OpenAPI Generator**: Required if any entry in `build.json` has `kind: "openapi"`.
*   **Gradle**: Required if any entry is detected as Android or Kotlin.

### Building

```bash
cargo build --release
```

## Configuration

The entire pipeline is driven by a single JSON file, typically named `build.json`.

### Structure

`build.json` is a flat array of `Entry` objects. Each object describes one repository to release.

```json
[
  {
    "path": "/path/to/local/repo",
    "remote": "https://github.com/user/repo.git",
    "kind": "openapi",
    "data": {
      "templates": "/path/to/templates",
      "deleteAllExceptGit": true
    },
    "readme": {
      "glob": ["*.rs", "*.toml"]
    },
    "commitIgnore": {
      "glob": ["*.lock"]
    }
  }
]
```

### Entry Fields

| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` | Yes | Local filesystem path to the git repository. |
| `remote` | `string` | No | Informational remote URL. The actual push URL is derived from the repo's git config. |
| `kind` | `string` | No | If `"openapi"`, triggers a pre-commit OpenAPI generation pass. Any other value or absence means standard commit flow. |
| `data` | `object` | Conditional | Required if `kind` is `"openapi"`. Contains `templates` (path to generator templates) and `deleteAllExceptGit` (boolean, wipes repo contents before generation). |
| `readme` | `object` | No | If present, triggers LLM-based README regeneration. Contains `glob` (array of glob patterns) to select files for context. |
| `commitIgnore` | `object` | No | Contains `glob` (array of glob patterns). Files matching these patterns are excluded from the diff sent to the LLM for commit message generation. |

### Glob Semantics

Glob patterns use the `glob` crate semantics with `require_literal_separator: true`.
*   **Unanchored** (no `/`): Matches anywhere in the path. `*.swift` matches `foo.swift` and `Sources/foo.swift`.
*   **Anchored** (contains `/`): Matches exactly. `Sources/*.swift` matches only direct children of `Sources/`. `Sources/**/*.swift` matches recursively.

## Usage

Run `buidl` from the directory containing your `build.json`.

```bash
# Use default build.json
./target/release/buidl

# Use a custom config file
./target/release/buidl --config my-releases.json
```

### Workflow

For each entry in the configuration, `buidl` performs the following steps:

1.  **Pre-processing (if `kind: "openapi"`)**:
    *   Reads the last semantic tag.
    *   Wipes the repository contents (keeping `.git/`) if `deleteAllExceptGit` is true.
    *   Runs `openapi-generator` using the specified templates.
2.  **Detection**:
    *   Scans the repository for build files (`Cargo.toml`, `Package.swift`, `build.gradle.kts`, `package.json`, etc.) to determine `RepoKind` (Rust, Swift, Kotlin, Android, JS, Generic).
3.  **Staging & Diffing**:
    *   Stages all changes.
    *   Computes the diff against HEAD.
    *   Filters out binary files and files matching `commitIgnore.glob`.
4.  **Commit Message Generation**:
    *   Sends the filtered diff to LM Studio.
    *   Validates the response against Conventional Commits format.
5.  **Version Calculation**:
    *   Parses the commit message to determine bump type (`major`, `minor`, `patch`).
    *   Computes the new tag (e.g., `v1.2.3` → `v1.3.0` for `feat`).
6.  **README Regeneration (if `readme` is present)**:
    *   Selects files matching `readme.glob`.
    *   Sends file contents to LM Studio to generate a fresh `README.md`.
    *   Stages the new `README.md`.
7.  **Commit & Push**:
    *   Creates a commit with the generated message.
    *   Pushes to `origin/main`.
    *   Creates and pushes the new tag.
8.  **Publishing**:
    *   If `RepoKind` is `Android` or `Kotlin`, runs `./gradlew publishAllPublicationsToGitHubPackagesRepository`.
    *   Other kinds are considered "tag-only" artifacts.

## Architecture

### Key Files

*   `src/main.rs`: CLI entry point. Parses `build.json` and orchestrates the loop over entries.
*   `src/lib.rs`: Core logic. Contains:
    *   **Data Structures**: `Entry`, `BuildConfig`, `RepoKind`.
    *   **Detection**: `detect_kind()` inspects filesystem for language signatures.
    *   **LLM Integration**: `lm()` handles HTTP POST to LM Studio; `commit_msg_for_diff()` and `regenerate_readme()` construct prompts.
    *   **Git Operations**: `stage_all()`, `diff_files()`, `commit_with_msg()`, `tag_and_push()`.
    *   **Utilities**: `select_commit_paths()`, `select_readme_paths()`, `matches_any_glob()`, `bump_kind()`, `compute_new_tag()`.
    *   **OpenAPI/Gradle**: `run_openapi_generator_for_templates()`, `gradle_publish_github_packages()`, `wipe_except_git()`.

### Non-Obvious Conventions

*   **LLM Prompting**:
    *   **README Regeneration**: The existing `README.md` is explicitly excluded from the prompt to prevent the model from anchoring to its previous output.
    *   **Commit Messages**: Binary files are always dropped. `commitIgnore.glob` is used to exclude noisy files (like lockfiles) that don't inform the commit intent.
*   **Versioning**:
    *   Tags are expected to be in `vMAJOR.MINOR.PATCH` format.
    *   If no tags exist, the default version `v0.1.0` is used.
    *   `feat` bumps minor; `fix` and others bump patch; `!` or `BREAKING CHANGE` bumps major.
*   **OpenAPI Generation**:
    *   The generator name and flags are derived from the template path's filename (e.g., `swift6`, `typescript-fetch`, `multiplatform`).
    *   For Kotlin/JS, the generator version is seeded with the last git tag to prevent infinite version-bump churn.
    *   Post-generation, `gradlew` permissions are fixed, and the Gradle wrapper version is patched to ensure compatibility with modern JDKs.
*   **Publishing**:
    *   For Swift, Rust, JS, and Generic repos, the Git tag *is* the artifact.
    *   For Android and Kotlin, `gradlew` is invoked to publish to GitHub Packages.

## Testing

Run the test suite:

```bash
cargo test
```

Tests cover:
*   Pure functions (glob matching, version bumping, fence stripping).
*   Filesystem operations (directory wiping, language detection).
*   Git operations (tag parsing, commit creation) using temporary directories.