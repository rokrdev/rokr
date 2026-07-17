# rokr

open-source agentic coding CLI — bring your own models, plan your own agents, blazingly fast and token-efficient through aggressive prompt caching.

## Status

Pre-alpha. Phase 1 in progress.

## Architecture

Workspace crates, one line each:

- `rokr` — binary entry point and CLI argument parsing.
- `rokr-tui` — ratatui frontend: render loop, layout, input handling.
- `rokr-core` — the agent loop, message and content-block model, context compaction.
- `rokr-provider` — the `Provider` trait and provider implementations (OpenAI-compatible first, Anthropic later).
- `rokr-tools` — the `Tool` trait and the core tool implementations (read, write, edit, bash, glob, grep, ls).
- `rokr-config` — JSON configuration loading, schema versioning, and migrations.
- `rokr-session` — session persistence as append-only JSONL, a metadata index, and resume/search support.

## Roadmap

See [docs/PLAN.md](docs/PLAN.md).

## ADRs

Architecture decisions are recorded in [docs/adr/](docs/adr/).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
