# Observability and debug bundle

## Logging

- **Agent / memory**: set `RUST_LOG=eson_agent=debug,eson_memory=info` (crate names as used in `tracing`).
- Prefer **structured** fields on spans (session_id, correlation_id) as the codebase grows.

## Correlation IDs (next step)

- Propagate `X-Eson-Trace-Id` (or similar) from UI → agent → memory for request tracing.

## Local debug bundle (no telemetry)

- v0.1: manually collect `workspace/logs/`, redacted `.env`, and last 500 lines of terminal output.
- Future: `eson-agent` `GET /debug/bundle` returning a redacted JSON snapshot (opt-in only).
