# buidl

A JSON-driven build orchestrator that automates the release pipeline for multiple repositories. It stages changes, generates Conventional Commits messages and READMEs via a local LLM, bumps semantic versions, pushes to GitHub, and optionally publishes artifacts or creates GitHub releases. Language detection is automatic, and OpenAPI client generation is supported.

## Prerequisites

- **Rust toolchain** (edition 2024)
- **`git`** CLI (for credential helpers, SSH agent, and push/tag operations)
- **`gh`** CLI (required for GitHub release creation)
- **`openapi-generator`** CLI (required for entries with `kind: "openapi"`)
- **`gradle`** / `./gradlew` (required for Android/Kotlin publishing)
- **LM Studio** running locally on `http://localhost:1234` (serving a compatible model; default: `qwen/qwen3.6-35b-a3b`)

## Installation & Build

```bash
cargo build --release
```

The binary is produced at `target/release/buidl`.

## Configuration

`buidl` is driven entirely by a `build.json` file (default path). It expects a flat JSON array of repository entries.

### Entry Schema

| Field | Type | Description |
|-------|------|-------------|
| `path` | `string` | Absolute or relative path to the local repo checkout. |
| `remote` | `string` | Informational only. The actual push URL is read from the repo's git config. |
| `kind` | `string` | Optional. Use `"openapi"` to trigger a pre-wipe + OpenAPI generator pass. |
| `data` | `object` | Required if `kind: "openapi"`. Contains `templates` (path to generator templates) and `deleteAllExceptGit` (boolean). |
| `readme` | `object` | Optional. Contains `glob: string[]`. Opts into LLM-generated README rewrites. Only files matching at least one glob are fed to the model. |
| `commitIgnore` | `object` | Optional. Contains `glob: string[]`. Files matching these patterns are excluded from the commit-message LLM prompt. |
| `release` | `object` | Optional. Contains `glob: string[]`. Asset paths to attach to a GitHub release. |

### Example `build.json`

```json
[
  {
    "path": "/Users/u/rustapps/buidl",
    "readme": { "glob": ["*.rs", "*.toml"] },
    "commitIgnore": { "glob": ["*.lock"] }
  },
  {
    "path": "/Users/u/swiftapps/LyricEditor",
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

## Usage

```bash
./buidl --config build.json
```

If `--config` is omitted, `buidl` defaults to `build.json` in the current working directory.

## Architecture & Key Files

- **`src/main.rs`** – CLI entry point. Parses arguments, loads `build.json`, and orchestrates the per-repo pipeline.
- **`src/lib.rs`** – Core logic. Contains git operations, LLM communication, semantic versioning, glob filtering, OpenAPI generation, release creation, and comprehensive unit tests.
- **`tests/integration.rs`** – Filesystem and git-backed integration tests covering tag computation, kind detection, wipe behavior, and empty-filter guards.
- **`build.json`** – Declarative pipeline configuration.

## How It Works

For each entry in `build.json`, `buidl` executes the following pipeline:

1. **OpenAPI Pre-processing** (if `kind: "openapi"`): Reads the highest existing tag, optionally wipes the working directory (preserving `.git/`), and runs `openapi-generator generate`. Version markers baked into JS/Kotlin output are patched to match the new tag to prevent infinite bump loops.
2. **Language Detection**: Auto-detects `RepoKind` (Swift, Kotlin, Android, JS, Rust, Generic) by inspecting build-tool signatures (`Cargo.toml`, `Package.swift`, `build.gradle.kts`, `package.json`, etc.).
3. **Stage & Diff**: Stages all tracked files. If the diff against HEAD is empty, the entry is skipped (`Nothing to release`).
4. **Commit Message Generation**: Filters staged files by dropping binaries and applying `commitIgnore` globs. Sends the unified diff to LM Studio and validates the response against Conventional Commits format.
5. **Version Bumping**: Computes the new tag based on the commit type (`feat` → minor, `!`/`BREAKING CHANGE` → major, others → patch). Falls back to `v0.1.0` if no tags exist.
6. **README Regeneration** (if `readme.glob` is present): Feeds matched source files to LM Studio, strips outer markdown fences, and re-stages the new `README.md`.
7. **Commit & Push**: Creates the commit, pushes `origin main`, and pushes the new tag.
8. **Artifact Publishing**: For Android/Kotlin repos, runs `./gradlew publishAllPublicationsToGitHubPackagesRepository -PlibraryVersion=<tag>`.
9. **GitHub Release** (if `release.glob` is present): Runs `gh release create <tag> --generate-notes` with the expanded asset paths.

## Non-Obvious Conventions & Gotchas

- **Glob Semantics**: 
  - Patterns **without** `/` are anchored anywhere (e.g., `*.swift` matches `foo.swift` and `Sources/Foo.swift`).
  - Patterns **with** `/` use standard literal-separator matching (e.g., `Prod/*.swift` matches only direct children; `Prod/**/*.swift` recurses).
- **LLM Timeout**: README regeneration and large refactor diffs can take minutes. The HTTP client is configured with a 30-minute timeout (`Duration::from_secs(1800)`).
- **Binary File Handling**: Binary files are silently dropped from LLM prompts to prevent corruption. They remain committed but are excluded from `commitIgnore` and `readme` filtering logic.
- **Empty Filter Guard**: If `commitIgnore` or binary filtering leaves no files for the LLM, `buidl` falls back to `chore: update` instead of sending an empty diff. This prevents libgit2 from silently inverting the filter to "include all paths".
- **OpenAPI Version Sync**: JS (`typescript-fetch`) and Kotlin (`multiplatform`) generators bake the seed version into `package.json`, `README.md`, and `build.gradle.kts`. `buidl` patches these to match the newly computed tag before committing, ensuring subsequent runs don't see a stale version diff and trigger infinite bumps.
- **JSON Field Naming**: `commitIgnore` uses camelCase in JSON but maps to `commit_ignore` in Rust via `#[serde(rename = "commitIgnore")]`.
- **Remote URLs**: The `remote` field in `build.json` is purely informational. Push URLs are always resolved from the repository's existing git configuration.

## Testing

Run the full test suite (unit + integration):

```bash
cargo test
```

Tests cover pure functions (glob matching, version bumping, fence stripping, path filtering), filesystem operations (`wipe_except_git`, `detect_kind`), and git-backed workflows (`last_tag`, `compute_new_tag`, empty-filter guards). No external LLM or network calls are required for tests.