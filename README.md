# buidl

**buidl** is a JSON-driven build orchestrator for automated release management. It automates the entire lifecycle of a repository release: detecting the language, generating Conventional Commits messages via a local LLM, regenerating READMEs, computing semantic version bumps, committing, pushing, and tagging.

It is designed for polyglot monorepos or distributed repositories where the release process needs to be consistent, automated, and driven by a single configuration file (`build.json`).

## Features

- **Language Auto-Detection**: Automatically detects Swift, Kotlin, Android, JavaScript, Rust, or Generic repos based on file signatures (e.g., `Cargo.toml`, `Package.swift`, `build.gradle.kts`).
- **LLM-Driven Commit Messages**: Uses a local LM Studio instance to generate Conventional Commits messages from the staged diff.
- **LLM-Driven README Generation**: Optionally regenerates `README.md` from scratch using the codebase as context, preventing anchoring to previous versions.
- **Semantic Versioning**: Automatically bumps major/minor/patch versions based on the generated commit message type (`feat`, `fix`, `BREAKING CHANGE`, etc.).
- **OpenAPI Code Generation**: Special support for `kind: "openapi"` entries to wipe directories, run `openapi-generator`, and sync versions.
- **Gradle Publishing**: Automatically publishes Android and Kotlin Multiplatform artifacts to GitHub Packages.
- **Smart Filtering**: Excludes binary files and lockfiles from LLM prompts to save tokens and reduce noise.

## Prerequisites

1.  **Rust Toolchain**: `buidl` is written in Rust. Install via [rustup](https://rustup.rs/).
2.  **Git**: Standard Git installation.
3.  **LM Studio**: A local instance of LM Studio running on `http://localhost:1234`.
    -   The tool expects a model compatible with the OpenAI chat completion API.
    -   Default model in code: `qwen/qwen3.6-35b-a3b`.
    -   *Note*: The HTTP client timeout is set to 30 minutes to accommodate large context windows.
4.  **OpenAPI Generator** (Optional): Required only if you have entries with `kind: "openapi"` in your config.
5.  **Gradle** (Optional): Required for Android/Kotlin repos to publish to GitHub Packages.

## Installation

Build from source:

```bash
git clone <repository-url>
cd buidl
cargo build --release
```

The binary will be available at `target/release/buidl`.

## Configuration

The entire pipeline is driven by a single JSON file, `build.json` by default. This file contains an array of repository entries.

### Entry Structure

Each entry in `build.json` supports the following fields:

| Field | Type | Required | Description |
| :--- | :--- | :--- | :--- |
| `path` | `string` | Yes | Local filesystem path to the git repository. |
| `remote` | `string` | No | Informational only. The actual push URL is derived from the repo's git config. |
| `kind` | `string` | No | If `"openapi"`, triggers a pre-wipe and OpenAPI generator pass before the standard release flow. |
| `data` | `object` | Conditional | Required if `kind` is `"openapi"`. See [OpenAPI Entries](#openapi-entries). |
| `readme` | `object` | No | Opt-in for README regeneration. See [README Regeneration](#readme-regeneration). |
| `commitIgnore` | `object` | No | Opt-out for commit message generation. See [Commit Filtering](#commit-filtering). |

### OpenAPI Entries

If `kind: "openapi"`, the `data` object must contain:

-   `templates` (`string`): Path to the openapi-generator templates directory. The tool derives the language (Swift, TypeScript, Kotlin) from the template folder name.
-   `deleteAllExceptGit` (`boolean`): If `true`, wipes the repository directory (except `.git/`) before generating code.

### README Regeneration

To enable automatic README regeneration, include a `readme` object:

```json
{
  "readme": {
    "glob": ["*.rs", "*.toml"]
  }
}
```

-   **`glob`**: An array of glob patterns. Only files matching these patterns are fed to the LLM.
-   **Behavior**: The existing `README.md` is explicitly excluded from the prompt to prevent the LLM from anchoring to its previous output. If no files match the globs, the README is left untouched.

### Commit Filtering

To exclude noisy files (like lockfiles) from the commit message generation prompt:

```json
{
  "commitIgnore": {
    "glob": ["*.lock", "dist/**"]
  }
}
```

-   **`glob`**: Files matching these patterns are excluded from the diff sent to the LLM.
-   **Binary Files**: Binary files are *always* excluded from LLM prompts regardless of this setting.

### Example `build.json`

```json
[
  {
    "path": "/Users/dev/projects/my-rust-app",
    "readme": { "glob": ["*.rs", "*.toml"] },
    "commitIgnore": { "glob": ["Cargo.lock"] }
  },
  {
    "path": "/Users/dev/projects/my-swift-api",
    "kind": "openapi",
    "data": {
      "templates": "/Users/dev/templates/swift6",
      "deleteAllExceptGit": true
    }
  },
  {
    "path": "/Users/dev/projects/my-kotlin-lib",
    "readme": { "glob": ["src/**/*.kt"] }
  }
]
```

## Usage

Run the tool from the directory containing your `build.json`:

```bash
# Default config file
./target/release/buidl

# Custom config file
./target/release/buidl --config my-releases.json
```

### Workflow

For each entry in the configuration, `buidl` performs the following steps:

1.  **Pre-processing**: If `kind` is `"openapi"`, it runs the OpenAPI generator.
2.  **Detection**: Auto-detects the repository language (Swift, Kotlin, Android, JS, Rust, Generic).
3.  **Staging**: Stages all changes in the working directory.
4.  **Analysis**:
    -   Filters out binary files and `commitIgnore` globs.
    -   If no files remain for the LLM, it defaults to `chore: update`.
5.  **Commit Message**: Sends the filtered diff to LM Studio to generate a Conventional Commits message.
6.  **Version Bump**: Calculates the new semantic version based on the commit type (`feat` → minor, `fix` → patch, `!` or `BREAKING CHANGE` → major).
7.  **README Regen**: If configured, regenerates `README.md` using LM Studio and stages it.
8.  **Commit**: Creates the commit with the generated message.
9.  **Push**: Pushes to `origin/main`.
10. **Tag**: Creates a lightweight tag and pushes it.
11. **Publish**: For Android/Kotlin repos, runs `gradlew publishAllPublicationsToGitHubPackagesRepository`.

## Architecture

### Key Files

-   `src/main.rs`: CLI entry point. Parses `build.json` and orchestrates the loop over entries.
-   `src/lib.rs`: Core logic. Contains:
    -   `detect_kind`: Language detection heuristics.
    -   `lm`: HTTP client for LM Studio.
    -   `commit_msg_for_diff`: LLM prompt construction and validation.
    -   `regenerate_readme`: Context gathering and README generation.
    -   `compute_new_tag`: Semver bumping logic.
    -   `run_openapi_generator_for_templates`: Shell-out to `openapi-generator` with specific flags per language.
-   `tests/integration.rs`: End-to-end tests using temporary directories and real git repositories.

### Non-Obvious Conventions

1.  **Glob Semantics**:
    -   Patterns without a `/` (e.g., `*.swift`) are anchored **anywhere** in the directory tree.
    -   Patterns with a `/` (e.g., `Sources/*.swift`) match exactly as written. Use `**` for recursion (e.g., `Sources/**/*.swift`).
2.  **OpenAPI Version Sync**:
    -   The tool reads the highest existing git tag to seed the OpenAPI generator's version (`npmVersion`, `artifact-version`). This prevents infinite version-bump loops where the generator defaults to `1.0.0` while `buidl` tags `v0.1.0`.
3.  **Gradle Patching**:
    -   When generating Kotlin multiplatform clients, `buidl` automatically patches the generated `gradlew` permissions and updates the Gradle wrapper version from `8.14.3` to `9.1.0` to ensure compatibility with JDK 25.
4.  **Empty Diff Handling**:
    -   If `commitIgnore` or binary filtering results in an empty set of files for the LLM, `buidl` safely falls back to `chore: update` rather than sending an empty prompt or including the entire repo diff.

## Dependencies

-   `clap`: CLI argument parsing.
-   `git2`: Git repository manipulation (index, diff, commit, tag).
-   `reqwest`: HTTP client for LM Studio communication.
-   `serde` / `serde_json`: JSON configuration parsing.
-   `glob`: Pattern matching for file filtering.