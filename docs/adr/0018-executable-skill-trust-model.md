# 0018 - Executable skill trust model

## Status

accepted (human approved 2026-08-04)

## Context

Ticket 65 (skills-instruction-bundle-loading) shipped skills as inert
instruction-bundle markdown only: `@skill:<name>` mentions
(`CommandRegistry::resolve_skill_mentions`, `crates/rokr-app/src/commands.rs`)
inline a discovered `skills/*.md` file's full contents in place of the
mention, sourced from a user- or project-scope `skills/` directory sibling
to `commands/`, project winning on name collision (ADR 0014, decision 3).
No execution semantics existed; a skill file was, and remains absent this
ADR, exactly as trusted as `AGENTS.md` -- text the model reads, nothing
more.

Ticket 75 (executable-skill-invocation) set out to add a `run:` frontmatter
field: a skill declares a shell command, and invoking `@skill:<name>`
executes it through the sandboxed `SeatbeltSandbox`/`BashTool` path (ticket
69) instead of, or alongside, inlining text. It was blocked before Gate 0
on 2026-08-02 for two compounding reasons:

1. **ADR 0014 decision 2 conflict.** That decision holds the line that
   `CommandRegistry::expand_template` performs exactly two substitutions
   (`$ARGUMENTS`, positional `$1..$n`) and that a `!`-prefixed or
   shell-metacharacter-looking line in a command body is inert literal
   text -- explicitly because the moment a project-scope, repo-shipped
   markdown file can cause a subprocess to run, `git clone && rokr` turns
   an attacker's `.rokr/commands/*.md` (and, by the same shape, a
   `.rokr/skills/*.md`) into arbitrary code execution the instant a victim
   types the matching mention. `run:` as ticket 75 specced it is precisely
   that hazard, on the sibling skills surface decision 2 didn't originally
   enumerate but is unambiguously covered by the same reasoning.

2. **Sandboxing (ADR 0015) is containment, not consent.** `SeatbeltSandbox`
   confines a subprocess's filesystem writes to the workspace and can deny
   network -- it does not ask permission before running, and it explicitly
   permits in-workspace writes and unrestricted reads (ADR 0015, decision
   2's profile: `(deny default)` layered with `(allow file-write* (subpath
   workspace_root))`, no read restriction at all). A sandboxed `git clone
   && rokr` can still exfiltrate every file in the workspace on first
   mention-resolution, still overwrite any in-workspace file, before a
   human ever sees what ran. Sandboxing alone does not clear the trust bar
   ADR 0014 decision 2 exists to hold.

Compounding both: mention resolution (`CommandRegistry::resolve_skills`,
called from both `crates/rokr/src/main.rs`'s TUI `submit` closure and
`crates/rokr-app/src/headless.rs`'s prompt-resolution path) is a
synchronous, pre-submission, deterministic text substitution -- it runs
*before* the prompt reaches the model and *before* `rokr_core::run_tool_loop`
exists for that turn at all. It is therefore outside every gate ADR 0016
built: `PermissionPolicy::resolve` and `SessionGrants` answer "should this
tool call the model chose to make run," a question with no analog here,
since there is no tool call, no model turn, and no `rokr_core` involved
yet. Threading a permission round-trip through the existing gate isn't an
option; a new seam is needed at the layer mention resolution already lives
in.

Per the human ruling that deferred ticket 75, this ADR is the prerequisite:
an explicit trust model for `run:`, an architectural consult (Merlin), and
human-approved rulings on the open design questions it raised. The
decisions below implement that consult's recommendation verbatim; ticket 75
is rescoped to build against them.

## Decision

**1. Consent is mandatory and TOFU (trust-on-first-use) hash-pinned.**
Executing a `run:` command requires a trust-store entry matching the exact
pair `(absolute_skill_path, sha256(skill_file_contents))`. The hash is
computed over the in-memory file contents about to be acted on -- not a
hash re-read from disk after the trust decision -- which closes the TOCTOU
gap where a file could be swapped between "user approved this content" and
"this content is what actually runs." On a miss (the skill has never been
trusted, or its contents changed since it was), the resolver shows the
user the exact literal command plus the skill's path and scope, and asks
for a trust decision. Nothing ever auto-executes on first sight -- this is
what closes the `git clone && rokr` supply-chain hazard ADR 0014 decision 2
was protecting against and ticket 75 as originally specced reopened. On
approval, the pair is recorded and the command executes; an approval may
alternatively be one-shot ([y] run once), executing without recording the pair
-- only an explicit trust decision ([r] trust this skill version) records it.
On decline, the mention resolves to a short "skill not executed" notice,
never the skill's body (ruling 3 -- the body may assume the command already
ran, so inlining it after a decline would hand the model instructions premised
on a false state).

**2. Containment (sandboxing) remains mandatory and orthogonal to
consent.** A `run:` command that clears the consent check in decision 1
still executes through the existing `SeatbeltSandbox`/`BashTool` path
(ticket 69) -- no new execution mechanism, no bypass of workspace-write
confinement or network denial. Decisions 1 and 2 are both required and
neither substitutes for the other: consent answers "should this ever run
at all," containment answers "what can it do once it's running." An
out-of-workspace write attempted by a *consented* skill's command is still
blocked by the sandbox, exactly as it would be for `bash`.

**3. Gate placement: a new pre-submission `ConsentResolver` seam,
abstracted over interactive and non-interactive execution.** Mirroring
`PermissionRequester`'s shape (trait abstracting TUI vs. headless), a
`ConsentResolver` is consulted by `resolve_skills` before a `run:` command
executes -- not folded into `rokr_core::run_tool_loop`'s
`request_permission` callback. Mention resolution is deterministic,
pre-send substitution of user-visible text, unconditionally applied to
every submitted prompt (per ticket 65's F-007 fix, both TUI and headless
call `resolve_skills` before the model ever sees the prompt); making that
resolution model-elective -- i.e., routing it through the tool loop, where
the model decides whether and when to invoke it -- would change the
feature's semantics from "a mention the user typed expands deterministically"
to "the model chooses to run a skill," a materially different capability
this ADR does not authorize. In the TUI, the `submit` closure in
`crates/rokr/src/main.rs` already holds a fresh `PermissionHandle` per
Enter-press (threaded from `rokr_tui::run`); that handle backs the consent
prompt, reusing `rokr_tui::PermissionDetail::Text` to show the literal
command (decision 7) rather than introducing a new prompt variant.
Non-interactive (headless, `--print`) is decision 4's job. `resolve_skills`
(`crates/rokr-app/src/commands.rs`) becomes `async` and fallible, gaining
`workspace_root`, a `Sandbox` implementation, and a `ConsentResolver`
parameter; both of `resolve_skills`'s call sites (`main.rs`'s `submit`
closure, `headless.rs`'s prompt-resolution path) update accordingly.

**4. Headless behavior reuses `PermissionMode`, per ruling 4.** This is the
same concept ADR 0016 already established for gated tool calls, not a
parallel notion invented for skills:

- Default mode, untrusted skill: inert (mention replaced by the "not
  executed" notice per decision 1) plus a one-line stderr notice; the run
  continues rather than aborting.
- Default mode, already-trusted hash: executes, no prompt (TOFU means the
  prompt only happens once per `(path, hash)`).
- `Bypass` (`--dangerously-skip-permissions`): executes without ever
  consulting or writing a trust-store entry -- `Bypass` already means "skip
  every permission gate," and a `run:` command is not exempted from that
  meaning, but bypassing does not fabricate consent history either.
- `--allow-skill`, a CI-friendly explicit allowlist flag, is explicitly
  **deferred** -- noted here as future work, not designed or implemented by
  this ADR. Implemented in a later change: a repeatable `--allow-skill
  <name>` or `--allow-skill <name>@<sha256-hex>` flag, honored in both the
  TUI and headless paths, checked on a trust-store miss before the
  prompt/inert-fallback. A matching entry executes with no prompt and, like
  the interactive "[y] run once" path above, writes no trust-store entry --
  the approval is ephemeral. A hash-pinned entry whose pin does not match
  the skill file's current content hash is treated as not allowed (falling
  through to the normal flow, plus a one-line stderr notice naming the
  mismatch) rather than silently approved.

**5. Scope and store, per rulings 1, 2, and 5.** Both user- and
project-scope skills may declare `run:` -- ADR 0014's discovery symmetry
(decision 1: project-scope discovery uses the identical path to user-scope)
is unchanged; what differs is trust, not discoverability:

- **User-scope skills are auto-trusted.** The user authored (or
  deliberately placed) them in their own `config_dir/skills/`; this is
  consistent with ADR 0012's user-scope-only trust boundary for hooks/MCP
  -- content only the user themselves controls needs no additional consent
  ritual layered on top of "it's in my own config directory."
- **Project-scope skills are TOFU hash-pinned**, per decision 1 -- a
  project-scope `.rokr/skills/*.md` file arrived via `git clone`, same
  provenance question ADR 0014 decision 1 already drew the AGENTS.md-level
  trust line around for *text*; `run:` is a categorically higher-trust
  surface than text, so it gets its own gate rather than inheriting that
  line.
- **The trust store is user-scope only and is never consulted for a
  project-scope trust file, because no such file exists.** There is no
  mechanism, in this ADR or any future one implied by it, for a
  project-scope skill to pre-declare or ship its own trust for itself --
  that would be a repo self-certifying its own executable content, exactly
  the anti-self-certification hazard ADR 0012's user-scope-only boundary
  was drawn to prevent for hooks/MCP. The store lives under rokr's user
  config dir, alongside the existing `rokr-config` persistence helpers
  (`default_config_dir`, `load_or_init`) and `rokr-provider`'s
  `FileTokenStore`/`TokenStore` pattern (`crates/rokr-provider/src/auth.rs`)
  -- a small `load`/`save` trait over a JSON file under `config_dir`, not a
  new persistence mechanism.
- **Shadowing is allowed but never inherits trust (ruling 5).** A
  project-scope skill may shadow a same-named, already-trusted user-scope
  executable skill -- ADR 0014 decision 3's project-over-user precedence is
  untouched by this ADR. But the trust-store key is `(absolute_skill_path,
  hash)`; a project-scope file at a different path with (at best)
  different bytes never matches the user-scope entry's key, so the
  shadowing skill re-triggers consent from a clean slate. This is a direct
  consequence of decision 1's key shape, not a special case bolted on for
  shadowing.

**6. `run:` is a literal command; no argument interpolation in v1, per
ruling 6.** The consent prompt shows, and the sandboxed execution runs,
the frontmatter's `run:` value exactly as written -- no `$ARGUMENTS`,
`$1..$n`, or any other substitution is ever applied to it. This is the
same injection guard ADR 0012 decision 1 established for hooks (payload
data never touches the command line the shell parses) and ADR 0014
decision 2 established for command templates (no live interpolation into
anything that could become a subprocess argument): the value the user
consented to seeing is byte-identical to the value that executes, with no
substitution step between consent and execution that could smuggle in
different content than what was shown.

**7. The consent prompt shows the command only, per ruling 7.** No
dry-run, no output preview -- `PermissionDetail::Text` carries the literal
`run:` string, the skill's path, and its scope (user/project); nothing is
executed speculatively to preview its effect before the user decides.

## Amendments

This ADR amends and refines ADR 0014 as follows. ADR 0014's own decisions
1 and 3 are otherwise left intact; nothing here supersedes ADR 0014 as a
document (per `docs/adr/README.md`'s "ADRs are immutable... write a new ADR
that supersedes" rule for full supersession -- this is a narrower,
in-place amendment of one sub-decision, in the same spirit as ADR 0016's
own amendment section for ticket 72).

- **Amends ADR 0014 decision 2.** Decision 2 forbade any execution
  semantics on the discoverable command/skill surface, v1 or otherwise.
  This ADR permits subprocess execution via a skill's `run:` frontmatter
  field, *and only* behind this ADR's Decisions 1 (consent) and 2
  (containment) both holding. Explicitly unchanged and still forbidden:
  `CommandRegistry`'s built-in-name-blind, call-order-only precedence
  invariant (ADR 0014 decision 3's first bullet) is untouched;
  `expand_template` still performs exactly `$ARGUMENTS`/`$1..$n`
  substitution and nothing else (decision 6, above); `!`-prefixed or any
  other inline-shell-looking line inside a command or skill *body* stays
  inert literal text -- `run:` is a new, opt-in, separately-gated
  *frontmatter field* a skill author sets deliberately, not an extension
  of what template-expansion syntax means or does to arbitrary body text.
- **Refines ADR 0014 decision 1.** Decision 1's argument -- that a
  text-only `.rokr/commands|skills/*.md` file is no riskier than
  `AGENTS.md`, which rokr already loads and feeds to the model
  unconditionally -- stays true for text. An executable `run:` skill is
  explicitly carved out as a *distinct, higher-trust surface*: it is never
  ambient (never runs without a fresh or previously-recorded consent
  decision, per Decision 1), unlike `AGENTS.md` and unlike a non-`run:`
  skill's inlined text, both of which remain exactly as ambient as before.
- **Leaves ADR 0014 decision 3 intact; notes the shadowing interaction.**
  Project-over-user, most-specific-scope-wins precedence for a same-named
  collision is unchanged. This ADR's decision 5 records the specific
  consequence for executable skills: shadowing is allowed the same as for
  any other command/skill, but a shadowing project-scope skill never
  inherits a shadowed user-scope skill's trust-store entry, because the
  two never share a `(path, hash)` key.

## Considered Options

### TOFU hash-pinned consent, pre-submission `ConsentResolver` seam (chosen)

- Pro: closes the `git clone && rokr` hazard completely -- nothing
  executes without a human having seen the exact command at least once per
  `(path, hash)`.
- Pro: reuses existing shapes end to end -- `PermissionRequester`/
  `PermissionHandle` for the seam abstraction, `PermissionDetail::Text` for
  the prompt payload, `PermissionMode` for headless dispatch, the
  `TokenStore` persistence pattern for the trust store -- no new
  first-principles design needed for any of the surrounding machinery.
- Con: a first-use prompt per project-skill *version* is real, visible
  friction -- mitigated by TOFU meaning it's the *first* use only, and by
  decision 7 keeping the prompt itself minimal (no dry-run round-trip to
  sit through).

### Prompt on every invocation, no persistence

- Pro: simplest possible mental model -- nothing to store, nothing that
  can go stale.
- Con: rejected. Prompt fatigue on a skill invoked routinely (e.g. a
  `/deploy`-style skill run every session) trains users to click through
  without reading, which is worse for the actual security property than a
  hash-pinned "you've seen this exact content before" grant that only
  re-prompts on a real change.

### User-scope-only executable skills (no project-scope `run:` at all)

- Pro: simplest possible trust story -- identical to ADR 0012's
  hooks/MCP boundary, zero new project-scope executable surface.
- Con: rejected. Discards the real use case ADR 0014 decision 1 explicitly
  exists to enable -- a team shipping a repo-standardized `/deploy` or
  `/release-notes` *skill*, not just a *command*, that every teammate gets
  without hand-copying files into their own user-scope directory. The
  consult's job was to find a trust model that keeps that use case, not to
  discard it because it's the harder case to get right.

### Static config allowlist (a config-file list of pre-approved skill paths/hashes)

- Pro: no interactive prompt at all, works unattended.
- Con: rejected as the *only* mechanism -- functionally a strictly worse
  version of the chosen design (TOFU consent *plus* persistence *is* an
  allowlist, just one populated by an interactive decision instead of
  hand-edited JSON) with more upfront friction (a user has to go edit a
  config file before a skill they just cloned can ever run) and no path
  for someone without config-file access to consent to anything. The
  CI-friendly slice of this idea survives as the explicitly deferred
  `--allow-skill` flag (decision 4), layered on top of, not instead of,
  the interactive TOFU flow.

### Reject execution entirely, close ticket 75 as won't-do

- Pro: zero new trust surface, zero new code.
- Con: rejected. This is a re-deferral dressed as a decision, not a
  resolution -- it was already the state of the world before this ADR and
  is exactly what the human ruling asked this ADR to move past by
  producing an actual trust model.

### Route consent through the tool loop / make it model-elective

- Pro: would reuse `rokr_core::run_tool_loop`'s existing gate machinery
  (`request_permission`, `PermissionPolicy::resolve`) without a new seam.
- Con: rejected, per decision 3's reasoning -- mention resolution is
  deterministic, pre-submission text substitution today; routing it
  through the tool loop would make execution something the *model*
  chooses to trigger rather than something that deterministically follows
  from a mention the *user* typed, changing the feature's semantics rather
  than just its gating mechanism.

## Consequences

- A new module, `crates/rokr-app/src/skill_trust.rs`, holds the trust-store
  type (a `(PathBuf, hash)`-keyed store, `load`/`save` over a JSON file
  under `rokr_config::default_config_dir()`, shaped after
  `rokr-provider`'s `TokenStore`/`FileTokenStore` pattern) and the
  `ConsentResolver` trait plus its interactive/headless implementations.
- `CommandRegistry::resolve_skills` (`crates/rokr-app/src/commands.rs`)
  becomes `async` and fallible, and its signature grows `workspace_root`,
  a sandbox, and a `ConsentResolver` parameter -- rippling into both call
  sites: `crates/rokr/src/main.rs`'s `submit` closure (already holds a
  `PermissionHandle` per submission, per decision 3) and
  `crates/rokr-app/src/headless.rs`'s prompt-resolution path (dispatches
  on `PermissionMode` per decision 4).
- Every project-scope executable skill costs its user exactly one
  first-use consent prompt per `(path, hash)` -- editing your own
  project's skill file changes its hash and re-triggers consent on next
  invocation, by design (decision 1's TOCTOU closure), not a bug to work
  around.
- `--allow-skill` (a CI-friendly non-interactive pre-approval flag) is
  explicitly out of scope here; a future ticket can add it without
  revisiting this ADR's core model, since it composes with (doesn't
  replace) the trust-store shape decision 5 establishes.
- Ticket 75 is rescoped against these decisions rather than its original,
  deferred spec; see ticket 75 (`executable-skill-invocation`) on the kanban
  board -- referenced by id/slug rather than a column path, since
  `.workflow/` board state (which column a ticket sits in) is ephemeral.
