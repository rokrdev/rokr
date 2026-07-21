# 0014 - Custom command project-scope discovery and trust boundary

## Status

accepted

## Context

Ticket 63 (custom-command-discovery-and-registry) landed `CommandRegistry`,
discovering markdown-templated `/command` files from a single, fixed
location: `config_dir/commands/` -- user-scope only, keyed by filename stem,
expanded via `$ARGUMENTS`/`$1`../`$n` substitution with no other semantics.
Ticket 64 extends discovery to a second location, `<project_root>/.rokr/commands/`,
so a repository can ship its own `/deploy`, `/release-notes`, etc. commands
alongside whatever the user has defined for themselves globally. Doing so
raises the same question ADR 0012 already answered for hooks and MCP
servers -- can this new discoverable, repo-controlled surface run as
whoever's config file cloned it? -- but the two features differ enough in
shape that ADR 0012's answer ("user-scope-only, full stop") doesn't
transfer unmodified, and needs its own explicit decision.

## Decision

**1. Project-scope discovery is allowed, unlike ADR 0012's hooks/MCP
boundary.** `CommandRegistry::discover_project_scope` scans
`<project_root>/.rokr/commands/*.md` using the exact same parsing and
expansion path as user-scope's `discover_user_scope` -- no special-cased
project-scope-only behavior. This is deliberately looser than ADR 0012's
"config, and therefore hooks/MCP, load user-scope only" rule, because the
risk profile is different in kind, not just degree: a hook or an MCP server
is an external PROCESS a cloned repo could get the user's own rokr binary
to spawn with the user's own permissions; a custom command's YAML+markdown
file can only ever expand into a prompt STRING that then goes through the
ordinary submit path -- identical exposure to a repo's `AGENTS.md`, which
rokr already loads and feeds to the model unconditionally on every session
in that directory. Since `AGENTS.md` is not treated as a special trust
boundary today, a text-only `.rokr/commands/*.md` file isn't one either.

**2. `!`-prefixed (or any other) inline shell-execution expansion is
explicitly NOT implemented, v1 or otherwise, in `CommandRegistry`.**
`expand_template` performs exactly two substitutions -- `$ARGUMENTS` and
positional `$1..$n` -- and nothing else; a `!`-prefixed line, a backtick
subshell, or any other shell-metacharacter-looking sequence in a command
body is inert literal text, copied through to the expanded prompt
unchanged. This absence is what keeps decision 1 sound: the moment a
custom command body could cause a subprocess to run, `git clone && rokr`
would let an attacker's `.rokr/commands/*.md` execute arbitrary code the
instant the victim typed the matching `/name` -- a strictly worse version
of the "hook command as an arbitrary-command-execution primitive" hazard
ADR 0012 already closed off for hooks, except reachable from project scope
with zero user-scope config required. If `!`-execution semantics are ever
added (comparable tools have a similar convention), they MUST come with
their own ADR revisiting this boundary, not be smuggled in as a
template-syntax extension.

**3. Collision precedence: built-in > project-scope > user-scope,
most-specific-scope-wins.** Two separate mechanisms enforce this, at two
different layers:

- Built-in names always win, but not because `CommandRegistry` knows what a
  built-in is -- it doesn't, and ticket 64 deliberately keeps it that way
  rather than teaching it a reserved-name list that would need to be kept
  in sync with `main.rs`'s `command` closure by hand. The guarantee instead
  comes from call order, unchanged since ticket 63: `rokr_tui::run`'s
  `resolve_custom_command` closure is only ever consulted from the
  built-in dispatcher's OWN "unknown command" fallthrough arm, after the
  built-in match has already run to completion. A discovered command
  literally named `cost` sits in the registry same as any other, inert
  until nothing else claims the name first.
- Project-scope wins over user-scope on a same-named collision between two
  DISCOVERED commands -- `CommandRegistry::merge_overriding` folds a
  project-scope registry over a user-scope one, so a project's own
  `/deploy` shadows a same-named personal one for anyone working in that
  directory. ASSUMPTION (ticket 64's Context note, not stated in the PRD):
  most-specific-scope-wins, matching the general precedent that a more
  local override should win over a more global default (shell `PATH`
  lookup order, `.gitconfig` local-over-global, etc.) -- flagged here for
  review rather than treated as self-evidently correct, since a project
  could just as easily surprise a user who didn't expect their own
  personal `/deploy` to be shadowed by walking into an unfamiliar repo. If
  review disagrees, this is a one-line swap of which registry calls
  `merge_overriding` on which.

## Considered Options

### Project-scope commands allowed as text-only templates (chosen)

- Pro: matches `AGENTS.md`'s existing, already-accepted trust level for a
  cloned repo's ambient instructions to the model; no new trust primitive
  introduced, just a new place the same trust level is exercised.
- Pro: the feature (repo-shipped `/command`s) is meaningfully useful --
  teams can standardize on a `/deploy`, `/release-notes`, etc. without
  every teammate hand-copying files into their own user-scope `commands/`
  directory.
- Con: a cloned repo can shadow a user's expectation of what `/name` does
  (see decision 3's project-over-user precedence) -- mitigated, not
  eliminated, by the fact that the expansion is still just text the user
  SEES before it's treated as a prompt (the TUI shows the expanded text
  going out, same as any other submission).

### Project-scope commands excluded entirely (ADR-0012-equivalent boundary)

- Pro: simplest possible trust story -- identical to hooks/MCP, zero new
  surface area from an untrusted clone.
- Con: rejected. Loses the actual feature ticket 64 exists to add, and the
  risk argument in decision 1 shows the ADR-0012 boundary was calibrated
  for PROCESS-spawning surfaces, not text-only ones -- applying it here
  isn't "consistent," it's applying the wrong precedent to a materially
  different risk shape.

### `!`-prefixed lines invoke a real subprocess, output substituted into the prompt

- Pro: strictly more powerful -- lets a command template embed live command
  output (e.g. `!git status` inlined into a `/commit` template), a real and
  useful pattern in comparable tools.
- Con: rejected for v1, exactly per decision 2 -- this is the one design
  that would turn a project-scope `.rokr/commands/*.md` file into a
  `git clone && rokr` arbitrary-code-execution primitive, the specific
  hazard project-scope discovery has to stay clear of to keep decision 1's
  "no riskier than AGENTS.md" argument true. Revisit only behind its own
  ADR, with its own explicit trust story (likely something closer to ADR
  0012's user-scope-only boundary, or an explicit per-project opt-in
  confirmation prompt).

## Consequences

- `CommandRegistry` gains `discover_project_scope` (identical
  parsing/expansion path to `discover_user_scope`, different root
  directory: `<project_dir>/.rokr/commands/` vs `config_dir/commands/`) and
  `merge_overriding` (a plain `HashMap::extend`-based fold, project scope
  calling this on top of a user-scope base). Neither method gains any
  built-in-name awareness -- that stays entirely a caller-side, call-order
  concern per decision 3.
- `crates/rokr/src/main.rs` builds the registry as
  `user_scope.merge_overriding(project_scope)` once per session start,
  using the same `cwd: Option<PathBuf>` already resolved for `/compact`'s
  repo-map regeneration and `/memory`'s path resolution -- when `cwd` can't
  be resolved, project-scope discovery is simply skipped (an absent
  `.rokr/commands/` directory is already a no-op per
  `discover_project_scope`'s own contract), not an error.
- A future project-scope commands directory could theoretically be made
  git-ignorable / requiring an explicit opt-in flag if this trust story
  ever needs tightening; not needed now given decision 1's argument, but
  worth remembering as the escape hatch if `AGENTS.md`'s own trust
  treatment ever changes.
