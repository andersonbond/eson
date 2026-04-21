# Socket.IO events (v1)

Client connects to the **same origin** as the agent (e.g. `http://127.0.0.1:8787`).

## Server → client

| Event | Payload (JSON) | When |
|-------|----------------|------|
| `tool_started` | `{ "tool", "args"? }` | tool invocation begins |
| `tool_completed` | `{ "tool", "result"? }` | tool finishes |

## Client → server

- v0.1: none required; chat uses HTTP `/session/message`.

## Image ingestion (Milestone 1 slice)

| Event | Payload |
|-------|---------|
| `image_scan_started` | `{ "scope": "workspace" }` |
| `image_file_processed` | `{ "path", "memory": bool }` |
| `image_scan_completed` | `{ "indexed": number, "memory": bool }` |

## Planned

- `memory_ingest`, `memory_consolidate`, `runtime_state`, `context_usage`, `ui_chart`, `system_health` — emit as features land.
