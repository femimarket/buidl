# buidl

**buidl** is a JSON-driven build orchestrator for automated release management. It automates the "stage → diff → commit → tag → push" lifecycle for multiple repositories defined in a single configuration file.

Key features:
- **Auto-detection**: Automatically detects repository languages (Rust, Swift, Kotlin, Android, JS, Generic) based on file signatures.
- **LLM-Powered**: Uses a local LM Studio instance to generate Conventional Commits messages and regenerate `README.md` files from source code.
- **OpenAPI Integration**: Supports `kind: "openapi"` entries to wipe directories, run `openapi-generator`, and sync version markers across generated clients.
- **Semantic Versioning**: Automatically calculates semver bumps (major/minor/patch) based on commit message types (`feat`, `fix`, `BREAKING CHANGE`, etc.).
- **Gradle Publishing**: Automatically publishes Android and Kotlin Multiplatform artifacts to GitHub Packages.

## Prerequisites

- **Rust Toolchain**: `cargo` (Edition 2024).
- **Git**: Installed and configured with user identity (`user.name`, `user.email`).
- **LM Studio**: Running locally on `http://localhost:1234`.
  - The tool expects a model named `qwen/qwen3.6-35b-a3b` (or compatible).
  - Ensure the LM Studio server is accessible at the default API endpoint.
- **OpenAPI Generator** (Optional): Required only if `build.json` contains entries with `kind: "openapi"`.
- **Gradle** (Optional): Required for `Android` and `Kotlin` repos to publish to GitHub Packages.

## Configuration

The pipeline is driven by a single JSON file, defaulting to `build.json`.

### Structure

`build.json` is an array of `Entry` objects. Each object represents a repository to process.

```json
[
  {
    "path": "/path/to/local/repo",
    "remote": "https://github.com/user/repo.git",
    "kind": "openapi",
    "data": {
      "templates": "/path/to/templates/swift6",
      "deleteAllExceptGit": true
    },
    "readme": {
      "glob": ["*.swift", "Sources/**/*.swift"]
    },
    "commitIgnore": {
      "glob": ["*.lock", "dist/**"]
    }
  },
  {
    "path": "/path/to/another/repo",
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
|-------|------|----------|-------------|
| `path` | `string` | Yes | Local filesystem path to the git repository. |
| `remote` | `string` | No | Informational only. The actual push URL is derived from the repo's existing git config. |
| `kind` | `string` | No | If `"openapi"`, triggers a pre-wipe and `openapi-generator` pass before the standard commit flow. |
| `data` | `object` | Conditional | Required if `kind` is `"openapi"`. Contains `templates` (path to generator templates) and `deleteAllExceptGit` (boolean). |
| `readme` | `object` | No | Opt-in for LLM-generated README regeneration. Contains `glob` (array of glob patterns). If absent, README is not touched. |
| `commitIgnore` | `object` | No | Excludes files from the commit-message LLM prompt. Contains `glob` (array of glob patterns). Useful for excluding lockfiles or generated noise. |

### Glob Semantics

Glob patterns use `glob` crate semantics with `require_literal_separator: true`.
- **Unanchored** (no `/`): Matches anywhere in the path (e.g., `*.swift` matches `foo.swift` and `Sources/foo.swift`).
- **Anchored** (contains `/`): Matches exactly (e.g., `Sources/*.swift` matches only direct children of `Sources/`).

## Usage

### Running the Pipeline

```bash
# Use default build.json
cargo run --release

# Use a custom config file
cargo run --release -- --config my-build.json
```

### Workflow per Entry

For each entry in `build.json`, `buidl` performs the following steps:

1. **OpenAPI Pre-processing** (if `kind: "openapi"`):
   - Reads the last existing tag.
   - Wipes the directory (except `.git/`) if `deleteAllExceptGit` is true.
   - Runs `openapi-generator` using the specified templates.
   - Syncs version markers in generated files (`package.json`, `build.gradle.kts`, `README.md`) to match the new tag.

2. **Auto-Detection**:
   - Detects `RepoKind` (Rust, Swift, Kotlin, Android, JS, Generic) based on file signatures (`Cargo.toml`, `Package.swift`, `build.gradle.kts`, `package.json`, etc.).

3. **Staging & Diffing**:
   - Stages all changes in the working directory.
   - Computes the diff against HEAD.
   - If no changes exist, skips the entry.

4. **Commit Message Generation**:
   - Filters staged files: excludes binary files and those matching `commitIgnore.glob`.
   - Sends the filtered diff to LM Studio to generate a Conventional Commits message.
   - If no files reach the LLM (e.g., all ignored/binary), defaults to `chore: update`.

5. **Version Calculation**:
   - Parses the commit message to determine bump type (`major`, `minor`, `patch`).
   - Calculates the new tag (e.g., `v1.2.3` → `v1.2.4` for patch).

6. **README Regeneration** (if `readme` field is present):
   - Filters tracked files by `readme.glob`.
   - Excludes `README.md` itself to prevent anchoring.
   - Sends source content to LM Studio to generate a fresh README.
   - Re-stages the new `README.md`.

7. **Commit & Push**:
   - Creates a commit with the generated message.
   - Pushes to `origin/main`.
   - Creates and pushes the new lightweight tag.

8. **Artifact Publishing** (if applicable):
   - For `Android` and `Kotlin` repos: Runs `./gradlew publishAllPublicationsToGitHubPackagesRepository`.
   - For others: The git tag is considered the artifact.

## Architecture

### Key Files

- **`src/main.rs`**: CLI entry point. Parses `build.json`, iterates over entries, and orchestrates the pipeline steps.
- **`src/lib.rs`**: Core logic.
  - **`detect_kind`**: Inspects filesystem to determine language.
  - **`commit_msg_for_diff`**: Interfaces with LM Studio for commit messages.
  - **`regenerate_readme`**: Interfaces with LM Studio for README generation.
  - **`compute_new_tag` / `bump_kind`**: Semver logic.
  - **`run_openapi_generator_for_templates`**: Spawns the `openapi-generator` CLI.
  - **`sync_openapi_versions`**: Post-processing to fix version strings in generated code.
- **`tests/integration.rs`**: End-to-end tests using temporary directories and real git repositories.

### Non-Obvious Conventions

1. **LM Studio Dependency**: The tool assumes a local LM Studio instance. If it's not running, the tool will panic with a connection error.
2. **Binary File Handling**: Binary files are always excluded from LLM prompts. They are staged and committed but not analyzed.
3. **Commit Message Validation**: The tool validates that the LLM returns a valid Conventional Commits header. If the format is invalid, the tool panics rather than committing garbage.
4. **OpenAPI Version Sync**: For `openapi` entries, the tool explicitly rewrites version strings in generated files (`package.json`, `build.gradle.kts`) to match the new tag. This prevents infinite version-bump loops caused by the generator seeding with the *previous* tag.
5. **Gradle Wrapper Patching**: For Kotlin multiplatform generators, the tool automatically patches `gradlew` permissions and updates `gradle-wrapper.properties` to use a compatible Gradle version (9.1.0) to avoid JDK 25 compatibility issues.
6. **Default Version**: If no tags exist, the tool uses `v0.1.0` as the starting version.

### Error Handling

- **Git Errors**: Panics if git operations fail (e.g., push failure, tag creation failure).
- **LLM Errors**: Panics if the LLM returns an empty response or invalid format.
- **OpenAPI Errors**: Panics if `openapi-generator` fails or if templates are missing.

## Development

### Running Tests

```bash
cargo test
```

### Building

```bash
cargo build --release
```

### Adding New Language Support

To add support for a new language:
1. Update `RepoKind` enum in `src/lib.rs`.
2. Update `detect_kind` to check for new file signatures.
3. Update the publish logic in `src/main.rs` if the new language requires a specific publish step (like Gradle).