# R2 Agent

> **Small droid, big jobs.**

A ~5MB Rust binary containing a complete Agent runtime: loop engine, L1/L2/L3 context cache, 4 core tools (`read` / `write` / `edit` / `bash`), sandbox isolation. Zero external dependencies — drop it on any Linux and run.

> Named after R2-D2 — the smallest droid in the room, no lightsaber, no speeches, just tools and execution. Saving the day for decades.

## Quick Start

```bash
# Build
cargo build --release

# Interactive chat
r2

# One-shot
r2 --once "read config.toml and explain it"

# Resume a session
r2 --session abc-123
```

## Design

Read the [design doc](docs/design.md). Every line of code maps to one of the 6 original concepts / 5 axioms. Code that cannot be traced back to an axiom does not exist.

```
r2-agent/
├── src/
│   ├── main.rs             # CLI entry + config
│   ├── agent.rs            # The loop engine
│   ├── context.rs          # L1/L2/L3 context manager
│   ├── session.rs          # JSONL persistence, crash recovery
│   ├── sandbox.rs          # chroot + rlimits + seccomp
│   ├── model/              # ModelProvider trait + providers
│   │   ├── openai_compat.rs
│   │   └── anthropic.rs
│   └── tools/              # read / write / edit / bash
└── tests/
```

## Roadmap

| Phase | Deliverable | Status |
|-------|-------------|--------|
| P0 | Core loop + OpenAI-compatible provider | 🚧 |
| P0.5 | Anthropic provider | ⏳ |
| P1 | 4 core tools + ToolRegistry | ⏳ |
| P2 | Session JSONL + crash recovery (MVP) | ⏳ |
| P3 | L2 context compression | ⏳ |
| P4 | Sandbox levels | ⏳ |
| P5 | L3 cross-session index | ⏳ |
| P6 | CLI + config polish | ⏳ |
| P7 | Hardening + tests | ⏳ |

## License

MIT
