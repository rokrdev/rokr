# 0010 - Config additive-fields policy (amends 0002)

## Status

accepted

## Context

ADR 0002 committed to a versioned config contract: `"version": 1` from day
one, with "every future schema change ships with a defined migration path
keyed off that field." Read literally, that means adding so much as one
optional field forces a version bump and a migration.

Phase 3 needed two new fields — `context_window_size` and
`auto_compact_threshold` — to drive auto-compaction. Bumping to version 2
and migrating on load would have made rokr rewrite a user's existing,
possibly hand-edited `rokr.json` the first time it ever ran under this
phase, a behavior the config loader has never done before (`load_or_init`
today never touches an existing file). That's a real product decision
(backup? warning? silent rewrite?) with no PRD guidance, not a mechanical
consequence of adding two fields with sane defaults.

## Decision

Draw a line ADR 0002 didn't: not every field addition is a schema change
that warrants a version bump.

- **Additive-optional fields** (a new field with a sane default, where a
  file missing it is still fully valid) are added as `#[serde(default)]`
  fields. `version` stays unchanged. The file is never rewritten to add
  them — an existing file without the field simply gets the runtime
  default; existing "never rewrite" behavior and its test stay in force.
- **Breaking changes** (a field becomes required, a field's meaning or
  shape changes, a field is removed) still bump `version` and still require
  an explicit migration with write-back, exactly as ADR 0002 describes. The
  migration UX for that case (backup, warning, or silent rewrite) is
  decided when such a change is actually proposed, not pre-committed here.

Phase 3's `context_window_size` and `auto_compact_threshold` are additive-
optional under this policy: no version bump, no migration, no write-back.

## Considered Options

### Option 1: Bump to version 2, migrate with write-back (ADR 0002 as originally read)

- Pro: consistent with ADR 0002's literal text; every schema change looks
  the same going forward.
- Con: forces a migration-UX decision (backup/warn/silent-rewrite) for a
  purely additive, backward-compatible change with sane defaults — a
  disproportionate cost, and the first time this codebase would ever
  rewrite a user's existing file.

### Option 2: Bump to version 2, no write-back (defaults only in memory)

- Pro: avoids the rewrite risk.
- Con: `version` becomes disconnected from the file's actual shape — a
  "version 2" file and a "version 1" file could be byte-identical, which
  defeats the point of a version field as a breaking-change detector.

### Option 3: Additive fields via serde defaults, version unchanged (chosen)

- Pro: zero risk to existing files, zero new migration-UX surface for a
  change that doesn't need one, keeps `version` meaningful (it now means
  "breaking-change generation," not "any change generation").
- Con: `version` alone can no longer answer "does this file have field X?"
  — a reader has to check field presence directly. Judged acceptable: that
  question is exactly what `#[serde(default)]` already answers for free.

## Consequences

`rokr-config`'s `Config` struct gains new fields as `#[serde(default = ...)]`
whenever they're additive-optional; `load_or_init`'s existing
never-rewrite guarantee is unaffected and its test stays green. `version`
remains reserved for the next actual breaking change, at which point the
migration-UX question ADR 0002 raises gets answered for that specific
change rather than pre-decided in the abstract.
