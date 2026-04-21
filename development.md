# Eson development (macOS)

Notes for running the **Eson** stack locally on macOS: memory sidecar, agent gateway (HTTP + Socket.IO), and the desktop or web UI.

## Prerequisites

- **Rust** (stable), **Cargo** — [rustup](https://rustup.rs/)
- **Node.js** and **npm** (v18+ recommended) — for `apps/desktop`
- **Xcode Command Line Tools** — required for some native crates and Tauri on macOS (`xcode-select --install`)

Optional:

- **[just](https://github.com/casey/just)** — task runner; recipes live in the repo `Justfile`
- **Ollama** — optional for chat provider **ollama**; required for **background** automation and for **analyze_visual** / **pdf_to_table** (local multimodal)
- **Poppler** (`brew install poppler`) — provides **`pdftoppm`** for PDF rasterization used by vision tools

## Repository root

All commands below assume your current directory is the **`eson/`** folder (the Cargo workspace root), unless noted otherwise.

```bash
cd /path/to/agentic/eson
```

## Environment variables

**`eson-agent`** loads env files automatically from the current working directory (run it from `eson/`):

1. `.env` (if present)
2. `.env.local` (if present; **overrides** `.env`)

So you can keep secrets in `.env.local` (gitignored) and still start the agent with plain `cargo run -p eson-agent`.

Other binaries (e.g. `eson-memory`) still rely on the shell environment unless you `source` a file yourself.

Copy the example when setting up:

```bash
cp .env.example .env
# add ANTHROPIC_API_KEY to .env.local (recommended)
```

### Chat providers

Desktop Settings can choose provider per **new** chat session (`anthropic`, `openai`, `ollama`). Configure one or more in `eson/.env` or `eson/.env.local`:

| Variable | Role |
|----------|------|
| `ANTHROPIC_API_KEY` | Required for `/session/message` |
| `ANTHROPIC_MODEL` | e.g. `claude-haiku-4-5-20251001` |
| `ANTHROPIC_MAX_TOKENS` | Optional (default `8192`) |
| `OPENAI_API_KEY` | Enables OpenAI provider |
| `OPENAI_MODEL` | e.g. `gpt-4o-mini` |
| `OPENAI_BASE_URL` | Optional OpenAI-compatible endpoint (`https://api.openai.com/v1` by default) |
| `OLLAMA_BASE_URL` | Ollama base URL (`http://127.0.0.1:11434`, auto-suffixed with `/v1`) |
| `OLLAMA_MODEL` | e.g. `gemma4:26b` (default in `.env.example`; must support tools for chat) |

Restart **`eson-agent`** after changing these.

Minimum for a normal dev loop:

| Variable | Role |
|----------|------|
| `ESON_WORKSPACE_ROOT` | Absolute or relative path to the sandboxed data directory (use `$(pwd)/workspace` from `eson/`) |
| `ESON_MEMORY_URL` | Base URL of the memory sidecar (default `http://127.0.0.1:8888`) |
| `ESON_MEMORY_PORT` | Port for `eson-memory` (default `8888`) |
| `ESON_AGENT_HTTP_PORT` | Agent HTTP + Socket.IO (default `8787`) |
| `ESON_PERSONA_DIR` | Optional override for folder containing `IDENTITY.md`, `SOUL.md`, `Eson.md` (defaults to `eson/persona/` when you run the agent from `eson/`) |

Persona files live in **`persona/`** at the Eson repo root. They are prepended to the Claude **system** prompt together with workspace + memory context. Edit them and restart **`eson-agent`**.

### Skills, background loop, and local vision

| Path / variable | Role |
|-----------------|------|
| [`skills/`](skills/) |  `SKILL.md` files (`cron/`, `inbox/`, `user/`, `auto/`). Override root with `ESON_SKILLS_DIR`. |
| `ESON_BACKGROUND_LOOP_ENABLED=1` | Enables heartbeat (cron skills) + inbox watcher under `workspace/inbox/`. |
| `ESON_BACKGROUND_PROVIDER` | Provider for background turns: `ollama` (default), `anthropic`, or `openai`. The Settings panel can override at runtime and persist to `<workspace>/db/background_settings.json`. Auto-falls back to Anthropic (then OpenAI) if the chosen provider isn't configured. |
| `ESON_HEARTBEAT_SEC` | Cron tick interval (default `60`). |
| `ESON_INBOX_AUTO_PROCESS` | `1` (default) watches `inbox/`; set `0` to disable. |
| `ESON_VISION_MODEL` | Defaults to `gemma4:e4b` (multimodal). The text-only `gemma4:26b`/`31b` will not work for `analyze_visual` / `pdf_to_table`. |
| `ESON_VISION_OLLAMA_URL` | Ollama host without `/v1` (defaults to `OLLAMA_BASE_URL`). |
| `ESON_VISION_PDF_MAX_PAGES` | Cap pages per PDF call (default `5`). |

Pull the default chat + vision models once (chat is text-only MoE; vision is the multimodal E4B):

```bash
ollama pull gemma4:26b   # chat / tools
ollama pull gemma4:e4b   # analyze_visual / pdf_to_table
```

Desktop (Vite) reads **`apps/desktop/.env`** or **`.env.local`**. Copy the example there:

```bash
cp apps/desktop/.env.example apps/desktop/.env.local
```

`VITE_ESON_AGENT_URL` should match where the agent listens (default `http://127.0.0.1:8787`).

**Tauri dev + agent:** The UI is served from **port 1420** but the agent is on **8787**. WKWebView often **blocks or stalls** cross-origin `fetch`/`Socket.IO` to `:8787`. In `tauri dev`, the app therefore calls **`/eson-api/...`** on the Vite server, which **proxies** to `http://127.0.0.1:8787` (including WebSockets). Ensure **`eson-agent` is running** before using the desktop shell.

**Workspace browser (agent HTTP):** `GET /workspace/info` returns the resolved workspace root; `GET /workspace/browse?path=` lists entries under that root (read-only JSON: `root`, `path`, `entries` with `name` and `is_dir`). Used by the desktop **Workspace** panel and works in plain browser dev when `VITE_ESON_AGENT_URL` points at the agent.

**Provider introspection:** `GET /session/providers` returns which providers are configured on the current agent process.

If the UI shows **404** on `/workspace/browse` (or JSON parse errors on an empty body), the process on `8787` is almost certainly an **older `eson-agent` binary** without that route. From `eson/`, run **`cargo build -p eson-agent`** and restart **`cargo run -p eson-agent`** (or stop any other server bound to that port).

## Run the stack (three terminals)

This is the most reliable layout while everything is still separate processes.

### Terminal 1 — memory sidecar

```bash
cd /path/to/agentic/eson
export ESON_WORKSPACE_ROOT="$(pwd)/workspace"
# optional: export ESON_MEMORY_PORT=8888
cargo run -p eson-memory
```

SQLite is created under `workspace/db/` (see `.env.example`).

### Terminal 2 — agent gateway

```bash
cd /path/to/agentic/eson
export ESON_WORKSPACE_ROOT="$(pwd)/workspace"
export ESON_MEMORY_URL=http://127.0.0.1:8888
# optional: export ESON_AGENT_HTTP_PORT=8787
cargo run -p eson-agent
```

You should see logs that the agent is listening on **8787** (HTTP and Socket.IO on the **same** port).

### Terminal 3 — desktop UI

First time only:

```bash
cd apps/desktop
npm install
```

**Tauri window** (full desktop shell):

```bash
cd apps/desktop
npm run tauri dev
```

**Browser only** (faster iteration, same API):

```bash
cd apps/desktop
npm run dev
```

Vite’s dev server is typically `http://localhost:5173` unless configured otherwise; the UI points at `VITE_ESON_AGENT_URL` for HTTP and Socket.IO.

## One script for backend only

To start **memory + agent** in one terminal (agent runs in the foreground; memory in the background until you exit):

```bash
./scripts/dev.sh
```

From another terminal, still run `apps/desktop` with `npm run tauri dev` or `npm run dev`.

With **just** (if installed):

```bash
just dev
```

Same idea: memory and agent only; start the UI separately.

## Quick health checks

With memory and agent running:

```bash
curl -s http://127.0.0.1:8888/status
curl -s http://127.0.0.1:8787/health
curl -s http://127.0.0.1:8787/ready
```

Start a session:

```bash
curl -s -X POST http://127.0.0.1:8787/session/start \
  -H 'Content-Type: application/json' \
  -d '{}'
```

Optional image index pass over `workspace/`:

```bash
curl -s -X POST http://127.0.0.1:8787/ingestion/scan-images
```

## macOS-specific tips

- **Workspace path**: Prefer an absolute `ESON_WORKSPACE_ROOT` when debugging path issues; APFS is often case-insensitive—keep paths normalized in tools and tests.
- **Ports in use**: If `8787` or `8888` is taken, set `ESON_AGENT_HTTP_PORT` / `ESON_MEMORY_PORT` and align `ESON_MEMORY_URL` + `VITE_ESON_AGENT_URL`.
- **Microphone / voice**: When you enable real audio, Tauri will need the usual macOS usage strings and user consent; see `docs/MACOS_RELEASE.md` for release-time notes.
- **Firewall**: Local bind to `127.0.0.1` keeps traffic on loopback; adjust only if you intentionally expose services.

## `tauri dev` “stuck” on macOS

**Expected:** After Vite prints `ready` and Rust builds, the terminal **stays open with little or no new output** until you press **Ctrl+C**. That is normal — the dev process is running the desktop window.

**Harmless log noise:** Lines such as `IMKClient`, `IMKInputSession`, `_TIPropertyValueIsValid`, or `Text input context does not respond` come from Apple’s Input Method / WebKit when the webview focuses a field. They are **not** a crash and usually can be ignored.

**If you do not see the Eson window:** Check the Dock, Mission Control, or other desktops; the window may be behind other apps.

**White screen in `tauri dev`:** Vite **HMR** (the WebSocket) often fails inside **WKWebView**, which can leave the window white forever. When the Tauri CLI starts Vite (`beforeDevCommand`), it sets `TAURI_ENV_PLATFORM` and our Vite config turns **`server.hmr` off** so the UI loads (use a full window reload after edits). **Web Inspector** opens automatically in **debug** builds so you can see console errors.

While `tauri dev` is running, **[http://127.0.0.1:1420](http://127.0.0.1:1420)** should load in a normal browser. For **hot reload in the browser**, run `npm run dev` directly in `apps/desktop` (no `TAURI_ENV_*` → HMR stays on).

**IMK / Text input log lines** in the terminal are harmless macOS WebKit noise.

## Tests and full workspace builds

- **Rust only** (no frontend build):

  ```bash
  just test
  ```

- **Including Tauri**: the desktop crate embeds the Vite `build/` output. Run:

  ```bash
  just test-workspace
  ```

  or `npm run build` inside `apps/desktop`, then `cargo test --workspace`.

## Related docs

- [README.md](README.md) — overview and layout
- [docs/MACOS_RELEASE.md](docs/MACOS_RELEASE.md) — signing and distribution
- [docs/INTERACTION_ROADMAP.md](docs/INTERACTION_ROADMAP.md) — chat, voice, transport
- [docs/DEBUG_BUNDLE.md](docs/DEBUG_BUNDLE.md) — local debugging artifacts
