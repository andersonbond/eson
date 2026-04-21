# Eson

Eson is a local-first personal AI desktop shell built with a Tauri + Svelte UI, a Rust agent service, and a Rust memory sidecar.

It is designed around a strict workspace boundary (`workspace/` by default), live tool/event streaming over Socket.IO, and always-on memory backed by SQLite.

It can also analyze structured files in your workspace (including CSV and Excel), produce charts, generate descriptive summaries, retain useful memory, and improve behavior over time through learnings.

## Current status

- Active development, currently optimized for macOS.
- Local-first by default, with optional cloud model providers.
- Open-source preparation in progress.

## Features

- Desktop app (`Tauri + Svelte`) with chat UX and Markdown rendering.
- Agent runtime (`eson-agent`) with tool orchestration, workspace guards, and HTTP + Socket.IO APIs.
- Memory sidecar (`eson-memory`) for durable memory and image metadata via SQLite.
- Structured data analysis from CSV/Excel files, including chart generation and descriptive insights.
- Self-learning loop that captures useful learnings to improve future responses and behavior.
- Persona and skills system (`persona/`, `skills/`) for behavior shaping and automation runbooks.
- Optional vision workflows and image ingestion.

## Repository layout

| Path | Role |
|------|------|
| `apps/desktop/` | Tauri + Vite + Svelte desktop/web UI |
| `services/agent/` | Main agent service (tools, policies, Socket.IO, HTTP) |
| `services/memory/` | Memory service (SQLite-backed API) |
| `workspace/` | Default sandbox root for agent file access |
| `persona/` | Persona definitions (`IDENTITY.md`, `SOUL.md`, `Eson.md`) |
| `skills/` | Skill runbooks (`SKILL.md`) for scheduled/automated workflows |
| `docs/` | Lifecycle, roadmap, release, and debugging docs |
| `scripts/` | Development and packaging helpers |

## Prerequisites

- Rust toolchain (stable) and Cargo
- Node.js + pnpm (`pnpm@9` recommended)
- macOS (required for full desktop workflow)
- Optional: local Ollama or cloud API keys for model providers

## Quick start (development)

1. Create local env file:

   ```bash
   cp .env.example .env
   ```

2. Set required values in `.env` (minimum):
   - `ESON_WORKSPACE_ROOT=./workspace`
   - `LIVEKIT_URL`, `LIVEKIT_API_KEY`, `LIVEKIT_API_SECRET`
   - at least one model provider key/config (for example `ANTHROPIC_API_KEY`)

3. Start backend services (agent + memory):

   ```bash
   just dev
   ```

   This runs:
   - `eson-memory` on `ESON_MEMORY_PORT` (default `8888`)
   - `eson-agent` on `ESON_AGENT_HTTP_PORT` (default `8787`)

4. In another terminal, start desktop UI:

   ```bash
   cd apps/desktop
   pnpm install
   pnpm tauri dev
   ```

5. Optional web-only UI (without Tauri shell):

   ```bash
   cd apps/desktop
   pnpm dev
   ```

6. Optional: trigger image ingestion scan:

   ```bash
   curl -X POST http://127.0.0.1:8787/ingestion/scan-images
   ```

## Environment highlights

See [.env.example](.env.example) for the full list. Common settings:

- `ESON_WORKSPACE_ROOT` - sandbox root for file tools
- `ESON_WORKSPACE_ONLY_PATHS` / `ESON_ALLOW_ABSOLUTE_PATHS` - workspace safety controls
- `ESON_AGENT_HTTP_PORT`, `ESON_SOCKETIO_PORT`, `ESON_MEMORY_PORT` - local service ports
- `ESON_MEMORY_URL` - memory sidecar URL used by the agent
- `ANTHROPIC_*`, `OPENAI_*`, `OLLAMA_*` - provider selection and model configuration
- `ESON_MAX_CONCURRENT_TOOLS`, `ESON_MAX_TOOL_QUEUE_DEPTH` - orchestration limits

## Testing

Run core backend tests:

```bash
just test
```

Run full workspace tests (includes desktop build artifact requirement):

```bash
just test-workspace
```

Equivalent manual flow:

```bash
cd apps/desktop && pnpm build
cd ../.. && cargo test --workspace
```

## Build (desktop installer)

- `just build-desktop` - builds macOS desktop app via Tauri
- `just installer` - one-click DMG build workflow

For packaging/signing details, see:

- [build.md](build.md)
- [docs/MACOS_RELEASE.md](docs/MACOS_RELEASE.md)

## Additional docs

- [development.md](development.md)
- [debugging.md](debugging.md)
- [docs/INTERACTION_ROADMAP.md](docs/INTERACTION_ROADMAP.md)
- [docs/DATA_LIFECYCLE.md](docs/DATA_LIFECYCLE.md)

## Contributing

Contributions are welcome.

- [CONTRIBUTING.md](CONTRIBUTING.md)
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)

## License

This project is licensed under the [MIT License](LICENSE).