# macOS release engineering

## Signing and notarization (checklist)

1. **Apple Developer Program** — Team ID and signing certificates (Developer ID Application for distribution outside Mac App Store, or Apple Development for dev).
2. **Entitlements** — start minimal; add only what Tauri features require (e.g. microphone when voice ships).
3. **Hardened Runtime** — required for notarization; follow [Tauri distribution](https://v2.tauri.app/distribute/) docs.
4. **`notarytool`** — submit `.app` or zip; staple ticket after success.
5. **Gatekeeper** — users should see a clean open on first launch after notarization.

## CI

- Run `cargo build -p eson-desktop --release` and `npm run build` in `apps/desktop` on **macOS** runners (Apple Silicon + optional Intel).
- Cache `~/.cargo` and `node_modules`.

## Privacy strings

When enabling microphone or file access outside the sandboxed workspace, add the corresponding keys to `Info.plist` via Tauri config.
