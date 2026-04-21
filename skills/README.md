# Eson skills ( `SKILL.md`)

Markdown runbooks the agent loads with **`skill_list`** / **`skill_run`**.

## Layout

| Folder | Purpose |
|--------|---------|
| `cron/` | Time-driven skills; YAML `cron:` trigger (`every:15m`, `every:6h`, or `HH:MM` local) |
| `inbox/` | Fired when a file appears under `workspace/inbox/`; YAML `inbox_ext:` (e.g. `pdf`) |
| `user/` | User-authored procedures (no auto trigger) |
| `auto/` | Draft skills from **`propose_skill`** (gitignored by default) |

## Frontmatter

```yaml
---
name: my-skill
description: One line for skill_list
enabled: true
cron: "every:30m"
inbox_ext: pdf
---
```

Body is instructions for the LLM. Omit `cron` / `inbox_ext` when not used.

Override skills root: `ESON_SKILLS_DIR=/path/to/skills`.
