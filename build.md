# Building Eson for macOS (installer artifacts)

This guide covers producing a **macOS `.app`** and **`.dmg`** disk image for the **Eson desktop shell** (Tauri). The DMG is what most users drag-install on a Mac.

> **Architecture note (current layout)**  
> The installer ships the **desktop UI** only. The **agent** (`eson-agent`) and **memory** (`eson-memory`) services are separate Rust binaries and are **not** bundled inside the `.app` yet. For a full stack, users (or your own packaging step) still run those services—or you extend the bundle later (e.g. helper tools, `launchd`, or embedded sidecars). See [development.md](development.md) for running all three in dev.

## Prerequisites

- **macOS** (Apple Silicon or Intel) with **Xcode Command Line Tools** (`xcode-select --install`)
- **Rust** + **Cargo** ([rustup](https://rustup.rs/))
- **Node.js** + **npm** (for the desktop frontend and Tauri CLI)

Optional for **distribution outside your machine**:

- **Apple Developer Program** membership  
- **Developer ID Application** certificate + **notarization** (see [docs/MACOS_RELEASE.md](docs/MACOS_RELEASE.md))

Unsigned local builds install with extra Gatekeeper steps (right-click → Open, or System Settings → Privacy & Security).

## One-command desktop release build

From the **desktop app** directory:

```bash
cd apps/desktop
npm install
```

Unset **`CI`** if your environment sets `CI=1` (some CI systems do). Tauri’s CLI treats that flag strictly and can error with `invalid value '1' for '--ci'`:

```bash
env -u CI npm run tauri build
```

This will:

1. Run `npm run build` (Vite production build → `apps/desktop/build/`)
2. Compile `eson-desktop` in **release** mode
3. Produce bundles under `apps/desktop/src-tauri/target/release/bundle/`:
   - **`macos/Eson.app`** — installable application bundle  
   - **`dmg/Eson_<version>_<arch>.dmg`** — disk image for distribution (e.g. drag `Eson.app` → Applications)

Version and architecture suffix come from `src-tauri/tauri.conf.json` and the build machine (e.g. `aarch64` on Apple Silicon).

### Equivalent from the `eson/` root

```bash
cd apps/desktop && npm install && env -u CI npm run tauri build
```

Or use **just** (if installed):

```bash
just build-desktop
```

(`build-desktop` runs `npm run build` and `cargo build -p eson-desktop --release`; for a **DMG**, prefer `npm run tauri build` in `apps/desktop`, which runs the full Tauri bundler.)

## Install on another Mac

1. Copy the **`.dmg`** to the target machine (AirDrop, download, shared drive).
2. Open the DMG, drag **Eson** into **Applications**.
3. First launch: if the app is **not signed and notarized**, macOS may block it—use **Open** from the context menu once, or allow it under **System Settings → Privacy & Security**.

## Signing, notarization, and Gatekeeper

For builds you give to other people without security warnings:

- Sign the `.app` with your **Developer ID Application** identity.
- Enable **Hardened Runtime**, submit to **Apple notarization** (`notarytool`), then **staple** the ticket.

Concrete checklist and CI notes: **[docs/MACOS_RELEASE.md](docs/MACOS_RELEASE.md)** and the official [Tauri distribution](https://v2.tauri.app/distribute/) guide.

## Icons

Place source icons under `apps/desktop/src-tauri/icons/`. To regenerate standard sizes from a single PNG:

```bash
cd apps/desktop
npx tauri icon path/to/your-1024.png
```

## Building the Rust services (no installer)

The workspace root is `eson/`. Release binaries for the backend:

```bash
cd /path/to/eson
cargo build -p eson-agent -p eson-memory --release
```

Artifacts:

- `target/release/eson-agent`
- `target/release/eson-memory`

Copy these alongside your deployment docs or automate a future “full stack” package that includes `Eson.app` plus these binaries and a startup script.

## Configuration on the installed machine

The desktop app talks to the agent at **`VITE_ESON_AGENT_URL`** (baked in at **build time** for production). Default in [apps/desktop/.env.example](apps/desktop/.env.example) is `http://127.0.0.1:8787`.

Ensure the **agent** and **memory** processes are running on the target Mac (or change the URL and rebuild if you use a different host/port).

## Troubleshooting

| Issue | What to try |
|--------|-------------|
| `invalid value '1' for '--ci'` | Run with `env -u CI npm run tauri build`. |
| Missing `build/index.html` during `cargo build -p eson-desktop` | Run `npm run build` in `apps/desktop` first (Tauri embeds the Vite output). |
| Gatekeeper blocks the app | Sign + notarize, or use **Open** once for local unsigned builds. |
| Wrong architecture on another Mac | Build on that architecture, or use `rustup target add x86_64-apple-darwin` and Tauri’s cross-compile options as needed. |

## Related docs

- [development.md](development.md) — local dev (memory + agent + UI)
- [README.md](README.md) — project overview
- [docs/MACOS_RELEASE.md](docs/MACOS_RELEASE.md) — signing, notarization, privacy strings
