# Contributing to Eson

Thanks for your interest in contributing.

## Before you start

- Read the project overview in `README.md`.
- Check existing issues/PRs to avoid duplicate work.
- For major features or architecture changes, open an issue first to align on scope.

## Development setup

1. Clone the repo.
2. Copy environment variables:

   ```bash
   cp .env.example .env
   ```

3. Start backend services:

   ```bash
   just dev
   ```

4. In another terminal, run desktop UI:

   ```bash
   cd apps/desktop
   pnpm install
   pnpm tauri dev
   ```

## Branch and commit guidelines

- Create a focused branch from `main`.
- Keep PRs small and reviewable.
- Write clear commit messages that explain intent.
- Reference related issues in the PR description.

## Code style

- Rust: keep code `cargo fmt` clean and clippy-friendly.
- Frontend: keep Svelte/TypeScript code readable and maintainable.
- Prefer explicit names over compact clever code.
- Add comments only where logic is non-obvious.

## Testing

Run tests before opening a PR:

```bash
just test
```

If your change touches desktop build integration, also run:

```bash
just test-workspace
```

## Pull requests

A good PR should include:

- What changed and why
- Screenshots/GIFs for UI changes (if applicable)
- Test coverage or manual verification notes
- Any known limitations or follow-up tasks

## Security

- Do not commit secrets (API keys, tokens, private credentials).
- Use local `.env` files for sensitive configuration.
- If you find a security issue, please report it privately to the maintainer.

## Questions

If anything is unclear, open an issue and ask.
