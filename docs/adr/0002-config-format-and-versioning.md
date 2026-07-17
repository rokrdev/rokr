# 0002 - Config format and versioning

## Status

accepted

## Context

rokr needs a persistent, user-editable configuration file for provider
credentials, model selection, and future settings. Because this is an
open-source tool that users and scripts will edit and depend on directly, the
on-disk format is effectively a public API contract, not an internal
implementation detail.

## Decision

Store configuration as JSON at `~/.config/rokr/rokr.json`. The file includes
an explicit `"version": 1` field from the very first release, and every
future schema change ships with a defined migration path keyed off that
field.

## Considered Options

### JSON

- Pro: ubiquitous, zero ambiguity in parsing, serde support is first-class
  and matches the `serde_json` dependency already used across the workspace;
  owner preference.
- Con: no comments, less human-friendly to hand-edit than TOML/YAML.

### TOML

- Pro: comment support, popular in the Rust ecosystem (e.g. `Cargo.toml`),
  friendlier for hand-editing.
- Con: less natural fit for deeply nested or list-heavy structures than JSON.

### YAML

- Pro: comment support, compact for simple structures.
- Con: notoriously easy to misconfigure (whitespace sensitivity, implicit
  typing surprises); weaker ecosystem fit than JSON given `serde_json` is
  already a workspace dependency.

## Consequences

`rokr-config` owns loading, validating, and migrating this file. Because the
schema is versioned from day one, adding fields or restructuring config in
later phases (e.g. Phase 4's session-scoped model switching, Phase 6's
MCP/hooks config) is a matter of writing a migration for the version bump
rather than a breaking change for existing users.
