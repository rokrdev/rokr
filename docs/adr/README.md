# Architecture Decision Records

ADRs are immutable once accepted; to change a decision, write a new ADR that
supersedes the old one (never rewrite in place). ADRs are only written for
decisions that are irreversible or contested.

| ID | Title | Status |
|----|-------|--------|
| [0001](0001-language-and-tui-stack.md) | Language and TUI stack | accepted |
| [0002](0002-config-format-and-versioning.md) | Config format and versioning | accepted |
| [0003](0003-provider-abstraction.md) | Provider abstraction | superseded by 0009 |
| [0004](0004-agent-tool-loop.md) | Agent tool loop | accepted |
| [0005](0005-permission-model.md) | Permission model | accepted |
| [0006](0006-message-and-content-block-model.md) | Message and content-block model | accepted |
| [0007](0007-async-runtime-and-concurrency.md) | Async runtime and concurrency | accepted |
| [0008](0008-render-loop-architecture.md) | Render loop architecture | accepted |
| [0009](0009-provider-trait-location.md) | Provider trait location | accepted |
| [0010](0010-config-additive-fields-vs-version-bump.md) | Config additive-fields policy (amends 0002) | accepted |
| [0011](0011-rokr-mcp-crate-boundary.md) | rokr-mcp crate boundary | accepted |
| [0012](0012-hooks-execution-trust-model.md) | Hooks execution and trust model | accepted |
| [0013](0013-headless-output-schema.md) | Headless output schema | accepted |
| [0014](0014-custom-command-trust-boundary.md) | Custom command project-scope discovery and trust boundary | accepted |
| [0015](0015-sandbox-trait-and-seatbelt-backend.md) | Sandbox trait and macOS Seatbelt backend | accepted |
| [0016](0016-permission-mode-policy-layer.md) | Permission mode policy layer | accepted |
| [0017](0017-concurrent-subagent-execution.md) | Concurrent subagent execution | accepted |
| [0018](0018-executable-skill-trust-model.md) | Executable skill trust model | accepted |
