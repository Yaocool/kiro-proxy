# Changelog

This file records user-visible changes. The current source still declares
`0.2.4`, but includes changes after the `v0.2.4` tag. Items below are unreleased;
they must receive a new version rather than replacing that tag or its image.

## Unreleased — since v0.2.4

### Breaking changes and migration

- CLI groups `logs`, `models`, `tasks`, and `diagnose` now show help when used
  without an action. Use `logs show`, `models list`, `tasks list`, and `diagnose all`
  for data. Bare groups with `--json` fail with exit code 2.
- Removed parent-level logs/models options, hidden `account add-sso-batch`, alert
  `--kind`/`--url` and platform `wechat`, and topic-style `help balance`.
  See the [English migration](docs/startup-and-debugging.md#migration-from-the-old-cli) or
  [中文迁移表](docs/startup-and-debugging.zh-CN.md#旧命令迁移).
- Protocol-specific User-Agent checks are enabled by default: Claude Code for
  Claude routes, Codex for OpenAI generation routes, both for models. Other clients
  need a service/key exemption or a global configuration change; authentication
  and service key allowlists continue to apply.

### Added

- OpenAI Responses JSON/SSE endpoints and aliases, bounded service/key-scoped
  `previous_response_id` state, function/custom/namespace tools, Responses Lite
  `additional_tools`, tool choice, reasoning summaries and Codex automation input
  normalization. State defaults to enabled and disappears on restart.
- Explicit Kiro headless API-key import, regional runtime/management routing,
  Claude gateway model discovery aliases, effort metadata and client `envState`.
- Module-scoped configuration management, serialized cross-process configuration
  transactions, API-key restoration with a supplied secret, and per-service/key
  User-Agent exemptions.
- Nested CLI help, complete command-tree discovery, guides, shell completion,
  and Docker help/navigation from stopped containers using the matching image.

### Fixed

- Align Claude counting, optional tool results and nullable citations, and OpenAI
  nullable stream/strict controls, disabled reasoning and refusal replay with the
  public protocols. Normalize missing tool descriptions for Kiro tool replay.
- Execute Chat stop sequences and multiple choices in JSON/SSE, map
  `max_completion_tokens`, and adapt legacy function requests and responses.
  Wait for each streamed candidate's settlement before the next admission,
  retain body memory reservations, and record failures after SSE starts.
- Translate OpenAI inline/URL files and Claude search-result content to Kiro,
  accept empty web-search domain filters, and raise the image count limit to 100.
- Support Responses `truncation: auto` against the account's resolved model
  window, including smaller fallback models and continuations, while preserving
  instructions and the current tool chain.
- Validate cache controls against Claude's `ephemeral` and TTL schema across
  Messages, counting aliases, and OpenAI Chat cache extensions; accept absent/null
  controls and reject malformed or unknown types. Check Claude TTL ordering,
  automatic/explicit breakpoint conflicts and limits, uncacheable blocks, and
  nested search-result controls without forwarding client cache fields to Kiro.
- Preserve tool-only/empty assistant turns and validate upstream tool JSON across
  buffered/unbuffered, JSON/SSE paths; recover hollow Responses tool continuations
  once and surface repeated failure.
- Accept additive output-format and reasoning hints without claiming structured
  output enforcement; omit unsupported thinking controls when model metadata is
  missing, and preserve reasoning/tool markup across stream fragments.
- Resolve the account's actual model before compaction, summarize complete source
  input in bounded chunks, preserve checkpoints across client turns, and handle
  oversized Claude Code `/compact` source/system/tool material.
- Correct summary timeout, failure and usage accounting; diagnose fixed-context
  overflow without logging prompt or schema contents.
- Keep alerts when resetting general configuration; improve wrapper deployment
  preflight/atomic replacement and health-check rollback handling.
- Harden model identifiers and normalized upstream error redaction; tolerate
  transient browser navigation-context errors during SSO.

### Documentation

- Reorganize bilingual quick starts, startup/configuration references, CLI
  migration and protocol limits; replace historical design prose with current
  compaction behavior and explicitly labelled remaining work.
- Add contributor/release guidance and a dated [1.0.0 readiness assessment](docs/release-readiness-1.0.0.zh-CN.md).
- Consolidate operations, CLI migration, compatibility and release guidance into six maintained references; remove duplicate indexes and design documents.

## Earlier releases

Historical source changes remain available through Git tags and commit history.
`v0.2.4` points to `b06ec16` (2026-08-31). This changelog does not reconstruct
unverified release notes or assert registry availability for older images.

```bash
git log --oneline v0.2.4..HEAD
git tag --list --sort=-v:refname
```
