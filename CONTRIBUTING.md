# Contributing to rokr

## The crate map is the contribution map

Each crate in `crates/` owns one responsibility. If you know which part of the
product you want to change, you know which crate to open. See the root
[README.md](README.md) for the one-line-per-crate summary.

## Extension points

- **Tools** — implement the `Tool` trait in `rokr-tools`. Each tool is a
  single file (e.g. `read.rs`, `bash.rs`, `grep.rs`).
- **Providers** — implement the `Provider` trait in `rokr-provider`. Each
  provider is its own module (e.g. `openai.rs`, `anthropic.rs`).
- **Agents** — agents are defined as markdown prompts, not Rust code. Drop a
  new prompt file into the agents directory to define an agent's behavior.

## ADR process

We use the [MADR](https://adr.github.io/madr/) minimal format for
Architecture Decision Records, kept in `docs/adr/`. See
`docs/adr/0000-template.md` for the template and `docs/adr/README.md` for the
index and process rules.

ADRs are only written for decisions that are irreversible or contested. To
propose one, open a PR adding a new numbered file under `docs/adr/`.

## Commit style

This project uses [Conventional Commits](https://www.conventionalcommits.org/)
(e.g. `feat: ...`, `fix: ...`, `chore: ...`, `docs: ...`).
