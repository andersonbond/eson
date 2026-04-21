---
name: daily-summary
description: Optional daily digest (disabled by default — set enabled: true to use)
enabled: false
cron: "22:00"
---

If enabled: list `workspace/inbox` and `workspace/exports` for new artifacts, then **store_memory** a one-line digest summary under topics `["digest","daily"]`.
