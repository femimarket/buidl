# buidl

`buidl` is a JSON-driven build orchestrator that automates the commit → push → tag → release pipeline across multiple repositories. It integrates with a local LLM (via LM Studio) to generate Conventional Commits messages and regenerate `README.md` files, while supporting OpenAPI client generation, automatic semver bumping, and GitHub Packages publishing.

## Features

- **Multi-repo orchestration**: Single `build.json` drives parallel or sequential pipelines across any number of local git checkouts.
- **Auto-detection**: Language is inferred from disk signatures (Swift, Kotlin, Android, JS, Rust, Generic).
- **LLM-powered commits & docs**: Generates Conventional Commits messages and regenerates `README.md` using a local LM Studio instance.
- **Automatic semver bumping**: `feat` → minor, `fix`/`chore`/etc. → patch, `!` or `BREAKING CHANGE` → major.
- **OpenAPI code generation**: Pre-wipe, generate, and version-sync for Swift, TypeScript, and Kotlin clients.
- **GitHub integration**: Pushes to `origin/main`, creates lightweight tags, and optionally publishes GitHub releases with glob-expanded assets.
- **Gradle publishing**: Automatically publishes Android and Kotlin multiplatform artifacts to GitHub Packages.
- **Swift safety**: Enforces `.gitignore` rules to prevent SPM build artifacts and Xcode user data from leaking into commits.

## Prerequisites

- Rust toolchain (edition `2024`)
- `git` CLI
- `gh` CLI (authenticated, for GitHub releases)
- `openapi-generator` CLI (for `kind: "openapi"` entries)
- `gradlew` (for Android/Kotlin publishing)
- LM Studio running locally on `http://localhost:1234` with a compatible model (default: `qwen/qwen3.6-35b-a3b`)

## Installation & Build

```bash
git clone <repository-url>
cd buidl
cargo build --release
```

The binary will be available at `./target/release/buidl`.

## Configuration

`buidl` is driven entirely by a `build.json` file (default: `build.json`). The schema is defined in `src/lib.rs` as `BuildConfig` (a flat array of `Entry` objects).

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
    "remote": "https://github.com/femimarket/jsapi",
    "kind": "openapi",
    "data": {
      "templates": "/tpl/typescript-fetch",
      "deleteAllExceptGit": true
    }
  }
]
```

### Entry Fields

| Field | Type | Description |
|-------|------|-------------|
| `path` | `string` | Absolute path to the local git checkout. |
| `remote` | `string?` | Informational. Triggers `git remote add origin <url>` if missing. Git config is the source of truth for push URLs. |
| `kind` | `string?` | Set to `"openapi"` to trigger a pre-wipe + `openapi-generator` pass before the commit flow. |
| `data` | `object?` | Required when `kind: "openapi"`. Contains `templates` (path to generator templates) and `deleteAllExceptGit` (bool). |
| `readme` | `object?` | Opt-in README regeneration. `glob` array filters which tracked files are fed to the LLM. `README.md` itself is always excluded. |
| `commitIgnore` | `object?` | Opt-in diff filtering. `glob` array excludes files from the commit-message LLM prompt (e.g., `["*.lock"]`). |
| `release` | `object?` | Opt-in GitHub release. `glob` array expands to asset paths uploaded via `gh release create`. |

### Glob Semantics

Glob patterns follow `glob::MatchOptions` with `require_literal_separator: true` (see `src/lib.rs::matches_any_glob`):
- Patterns **without** a `/` (e.g., `*.swift`) are anchored anywhere in the tree.
- Patterns **with** a `/` (e.g., `Prod/*.swift`) match exactly what they describe. Use `**` for recursion.
- Empty glob lists match nothing and trigger safe no-ops.

## Usage

```bash
./target/release/buidl --config build.json
```

### Pipeline Flow (per entry)

1. **OpenAPI Pre-step** (if `kind: "openapi"`): Reads last tag, optionally wipes the directory (preserving `.git/`), and runs `openapi-generator`.
2. **Language Detection**: Auto-detects repo kind from build-tool signatures (`src/lib.rs::detect_kind`).
3. **Remote & Gitignore**: Sets `origin` if missing; enforces Swift `.gitignore` rules if applicable.
4. **Stage & Diff**: Stages all changes, diffs against HEAD, and filters binary/ignored files.
5. **Commit Message**: Sends filtered diff to LM Studio → validates Conventional Commits format → computes semver bump.
6. **README Regen** (if `readme` present): Feeds glob-matched files to LM Studio → strips markdown fences → re-stages `README.md`.
7. **Commit & Push**: Creates commit, pushes to `origin/main`.
8. **Tag**: Creates lightweight tag, pushes tag.
9. **Publish** (Android/Kotlin only): Runs `./gradlew publishAllPublicationsToGitHubPackagesRepository`.
10. **Release** (if `release` present): Runs `gh release create` with auto-generated notes and expanded assets.

If no changes are staged, the entry exits early with `ℹ️ No changes to commit. Nothing to release.`

## Architecture & Key Files

| File | Purpose |
|------|---------|
| `Cargo.toml` | Package metadata, dependencies (`clap`, `git2`, `glob`, `reqwest`, `serde`, `serde_json`), edition `2024`. |
| `src/main.rs` | CLI entry point. Parses `--config`, iterates `build.json` entries, orchestrates the pipeline. |
| `src/lib.rs` | Core logic: config types, repo detection, LLM interaction, git operations, versioning, OpenAPI generation, release asset expansion, and unit tests. |
| `tests/integration.rs` | Integration tests for config parsing, kind detection, git-backed versioning, and LLM guard behavior. |

## Non-Obvious Conventions & Gotchas

- **LLM Prompt Filtering**: 
  - `commitIgnore` globs exclude files from the commit-message prompt. If all files are filtered or binary, `buidl` falls back to `chore: update` instead of invoking the LLM.
  - `readme` globs filter README regeneration sources. `README.md` is always dropped from the prompt to prevent the model from anchoring to its own previous output.
- **Binary Handling**: Binary files are silently skipped in LLM prompts. They remain staged and committed but never reach the model.
- **OpenAPI Version Sync**: Generated clients bake the seed version into `package.json`, `README.md`, and `build.gradle.kts`. `buidl` rewrites these to match the new tag (`src/lib.rs::sync_openapi_versions`) to prevent infinite bump loops on subsequent runs.
- **Swift `.gitignore` Enforcement**: `src/lib.rs::ensure_swift_gitignore` appends missing SPM/Xcode ignore rules and actively untracks previously committed files that match them.
- **Strict Conventional Commits**: The model's output is validated against `feat|fix|docs|style|refactor|perf|test|build|ci|chore`. Malformed output panics immediately rather than corrupting the tag.
- **LM Studio Timeout**: Large prompts (README regen, multi-file diffs) use a 30-minute HTTP timeout (`1800s`) to accommodate local model inference.
- **`gh` CLI Dependency**: GitHub releases require `gh` on `PATH` with an active authentication session. Asset globs are resolved relative to the repo directory.
- **Gradle Wrapper Patching**: For Kotlin multiplatform OpenAPI entries, `buidl` automatically patches `gradlew` permissions and upgrades `gradle-wrapper.properties` from `8.14.3` to `9.1.0` to avoid JDK 25 compiler crashes.

## Testing

```bash
cargo test
```

- **Unit tests**: Embedded in `src/lib.rs` (pure functions: glob matching, version bumping, fence stripping, path filtering).
- **Integration tests**: `tests/integration.rs` (filesystem setup, git-backed tag resolution, config deserialization, LLM guard validation).
- **LLM Guards**: Tests deliberately avoid mocking LM Studio. If filter guards break, the test suite will fail loudly with a connection error, ensuring safe defaults are preserved.