# HTTP session API (v1)

Base: `http://127.0.0.1:8787` (override with `ESON_AGENT_HTTP_PORT`).

| Method | Path | Body | Notes |
|--------|------|------|-------|
| GET | `/health` | — | liveness |
| GET | `/ready` | — | memory reachable |
| POST | `/session/start` | `{}` | returns `{ "session_id" }` |
| POST | `/session/message` | `{ "session_id", "message", "provider"?, "settings"? }` | returns `{ "answer", "session_id" }`. `provider` is the user's current primary (`anthropic`/`openai`/`ollama`); when set it overrides the session's pinned provider for this and subsequent turns. |
| POST | `/session/branch` | `{ "session_id", "label"? }` | returns `{ "branch_id" }` |
| POST | `/session/merge` | `{ "session_id", "branch_id" }` | 204 |
| POST | `/session/terminate` | `{ "session_id" }` | 204 |
| GET | `/system/health` | — | OS metrics (macOS best-effort) |
| GET | `/system/processes` | — | top processes by memory (macOS) |
| GET | `/workspace/info` | — | workspace root path |
| POST | `/ingestion/scan-images` | — | index image files under workspace; emits Socket.IO `image_*` events |
