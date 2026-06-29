# buidl

A JSON-driven build orchestrator that automates the commit → push → tag → release pipeline across multiple repositories. It integrates with a local LLM (via LM Studio) to generate conventional commit messages and optionally regenerate `README.md` files. Supports OpenAPI client generation, automatic semver bumping, and platform-specific publishing.

## Features

- **Multi-repo orchestration**: Single `build.json` drives the entire pipeline
- **Auto-detection**: Identifies Rust, Swift, Kotlin, Android, JS, or Generic repos from disk signatures
- **LLM-powered commits & docs**: Generates Conventional Commits headers and regenerates READMEs via a local LM Studio instance
- **OpenAPI client generation**: Wipes repos, runs `openapi-generator`, and syncs version markers to prevent drift
- **Semver bumping**: `feat` → minor, `fix`/others → patch, `!`/BREAKING → major
- **GitHub releases**: Attaches glob-expanded assets to newly pushed tags
- **Gradle publishing**: Automatically publishes Android/Kotlin artifacts to GitHub Packages
- **Smart filtering**: Excludes lockfiles, generated code, or binaries from LLM prompts using glob patterns

## Prerequisites

- Rust toolchain (Edition 2024)
- `git` CLI (required for push/tag operations to leverage system credential helpers/SSH agents)
- `openapi-generator` CLI (only if using `kind: "openapi"`)
- `gh` CLI (only if using the `release` field)
- `./gradlew` (only for Android/Kotlin publishing)
- LM Studio running locally on `http://localhost:1234` with the `qwen/qwen3.6-35b-a3b` model loaded

## Installation & Build

```bash
git clone <repository-url>
cd buidl
cargo build --release
```

The binary will be available at `./target/release/buidl`.

## Configuration

Pipeline behavior is driven by a flat JSON array in `build.json` (default) or a custom path via `--config`. Each entry maps to `src/lib.rs::Entry`:

```json
[
  {
    "path": "/absolute/path/to/repo",
    "remote": "https://github.com/user/repo",
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
    },
    "release": {
      "glob": ["build/artifact.bin", "docs/spec.pdf"]
    }
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `path` | `string` | Local checkout directory. Required. |
| `remote` | `string` | Informational only. `git` config is the source of truth for push URLs. |
| `kind` | `string` | `"openapi"` triggers a pre-wipe + `openapi-generator` pass before the commit flow. |
| `data` | `object` | OpenAPI knobs: `templates` (path to generator templates), `deleteAllExceptGit` (wipe repo before generation). |
| `readme` | `object` | `{ "glob": [...] }` opts into LLM README regeneration. Empty list or absent → skip. |
| `commitIgnore` | `object` | `{ "glob": [...] }` excludes matching files from the commit-message LLM prompt. |
| `release` | `object` | `{ "glob": [...] }` expands to files attached to the GitHub release. Requires `gh` CLI. |

## Usage

Run the orchestrator from any directory:

```bash
# Uses default build.json in CWD
./target/release/buidl

# Custom config path
./target/release/buidl --config /path/to/pipeline.json
```

**Pipeline flow per entry:**
1. If `kind: "openapi"`: reads last tag, optionally wipes repo, runs `openapi-generator`
2. Auto-detects repo language
3. Stages all tracked files
4. If no changes → skips entry (`Nothing to release`)
5. Filters staged files via `commitIgnore` + binary detection, sends diff to LLM for Conventional Commits header
6. Computes semver bump and new tag
7. If `openapi`: syncs version markers in `package.json`, `README.md`, `build.gradle.kts`
8. If `readme` configured: regenerates `README.md` via LLM
9. Commits, pushes `origin main`, creates & pushes tag
10. If Android/Kotlin: runs `./gradlew publishAllPublicationsToGitHubPackagesRepository`
11. If `release` configured: runs `gh release create` with expanded assets

## Architecture & Key Files

| File | Purpose |
|------|---------|
| `src/main.rs` | CLI entry point (`clap`). Parses `build.json`, iterates entries, orchestrates the commit/push/tag/release sequence. |
| `src/lib.rs` | Core logic: config structs, repo detection, glob matching, LLM interaction (`lm`), git operations, version bumping, OpenAPI generation, release asset expansion, Gradle publishing, and extensive unit tests. |
| `tests/integration.rs` | Filesystem and `git2`-backed integration tests for config parsing, repo detection, wipe logic, tag computation, and empty-filter guards. |
| `Cargo.toml` | Package metadata, dependencies (`git2`, `reqwest`, `serde`, `clap`, `glob`), and dev dependencies. |

## Non-Obvious Conventions & Gotchas

- **LLM Endpoint & Timeout**: All LLM calls hit `http://localhost:1234/v1/chat/completions`. The HTTP client timeout is hardcoded to 30 minutes (`1800s`) to accommodate large README regenerations and multi-minute completions on the local Qwen3 model.
- **Hardcoded OpenAPI Spec**: The upstream spec URL is fixed at `https://localhost:443/api-docs/openapi.json` (`src/lib.rs::SPEC`). Move to `build.json` if multiple specs are needed later.
- **Glob Semantics**: `readme.glob` and `commitIgnore.glob` use `glob::MatchOptions` with `require_literal_separator: true`. Patterns without `/` (e.g., `*.swift`) match at any depth. Patterns with `/` (e.g., `Prod/*.swift`) are anchored to that directory structure.
- **README Regeneration Guard**: `README.md` is always excluded from the source file list fed to the LLM to prevent anchoring to its own previous output. If `readme.glob` matches zero files, the step is skipped entirely to avoid hallucinated content.
- **Commit Message Empty-Filter Guard**: If `commitIgnore` or binary detection filters out all files, `buidl` safely falls back to `chore: update` instead of sending an empty diff to the LLM or triggering libgit2's "include all paths" fallback.
- **OpenAPI Version Sync**: OpenAPI generators bake the seed version into output files. `buidl` rewrites `package.json`, `README.md`, and `build.gradle.kts` to match the *new* tag immediately after generation. Without this, every run would see a stale version diff and trigger infinite bumping.
- **Gradle Wrapper Patching**: When generating Kotlin multiplatform clients, the tool automatically patches `gradlew` permissions (`chmod +x`) and upgrades the bundled Gradle version from `8.14.3` to `9.1.0` to avoid JDK 25 compiler crashes.
- **Git CLI vs libgit2**: Push and tag operations spawn the `git` CLI rather than using `libgit2`'s `Remote::push` to correctly leverage user credential helpers, SSH agents, and GitHub keychain integrations.
- **Repo Detection Priority**: `Cargo.toml` > `Package.swift`/`.xcodeproj` > Gradle plugins (`com.android.*` → Android, `kotlin("multiplatform")` → Kotlin) > `package.json` > Generic. Polyglot repos default to Rust if `Cargo.toml` is present.

## Testing

Run the full test suite (unit + integration):

```bash
cargo test
```

- Unit tests in `src/lib.rs` cover pure functions: bump logic, glob matching, path filtering, version sync, and release asset expansion.
- Integration tests in `tests/integration.rs` validate config deserialization, repo detection, filesystem wiping, git tag computation, and empty-filter guards against LLM invocation.