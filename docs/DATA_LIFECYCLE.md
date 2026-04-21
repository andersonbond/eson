# Data lifecycle

## SQLite migrations

- Memory DB path: `$ESON_WORKSPACE_ROOT/db/eson_memory.db`.
- **Rule**: schema changes go through versioned migration scripts (e.g. `services/memory/migrations/NNN_description.sql`) applied on startup in order.
- v0.1 uses `CREATE TABLE IF NOT EXISTS` in code; promote to explicit migrations when schemas stabilize.

## Backup / export

- User-triggered export (future): zip `workspace/db/`, `workspace/index/`, and optional `workspace/exports/`.
- Document restore: stop services, replace `eson_memory.db`, restart.

## Encryption at rest (optional)

- Design flag `ESON_ENCRYPT_DB` (not implemented in v0.1): would wrap SQLite with SQLCipher or OS keychain-backed key; requires security review.
