# Eson debugging notes

## Tab stuck “loading”, blank UI, DevTools / right‑click unusable (desktop + browser)

### Root cause (fixed)

In `apps/desktop/src/App.svelte`, **`afterUpdate`** called an **async** handler that **`await tick()`**. That pattern schedules another Svelte flush, which runs **`afterUpdate` again**, repeating forever. The main thread never settles, so:

- The browser tab looks like it is still loading.
- The page may stay blank or frozen.
- Context menu / Inspect can feel broken.

**Fix:** Do not combine **`afterUpdate` + `await tick()`** in a loop. Scroll-to-bottom (or similar) should run from explicit triggers (e.g. a `$:` block on `messages` / `loading` with **`requestAnimationFrame`**, or event handlers), not from `afterUpdate` with `tick()`.

**Takeaway:** Treat `afterUpdate` as synchronous work only. Use `tick()` from `onMount`, actions, or user events—not in a way that re-enters the update cycle every flush.

---

### Symptoms that looked like other bugs (red herrings)

These showed up at the same time but were **not** the cause of the frozen tab:

- **`fetch` to `http://127.0.0.1:8787`** or Vite proxy hanging in DevTools (Network “pending”).
- **Tauri `invoke` / `plugin-http` IPC** appearing stuck (`ipc://localhost/...`).
- **WKWebView** quirks and **macOS** `RemoteLayerTreeDrawingAreaProxyMac` logs.

Those are still worth knowing about for **real** network or WebView issues, but a **synchronous infinite UI loop** should be ruled out first: open DevTools on a **minimal route** or strip the app to `main.ts` + empty component—if the tab still never finishes loading, suspect **JS infinite loop / Svelte update cycle** before deep-diving on Tauri or CORS.

---

### Vite: `base` in dev vs build

- **`base: "./"`** during **`vite dev`** can break module resolution and produce a **blank** page (different from an infinite loop: often no runaway CPU, but scripts fail to load correctly).
- Use **`base: "/"`** for dev and **`base: "./"`** only for **`vite build`** (e.g. Tauri packaged assets). This project uses `defineConfig(({ command }) => ({ base: command === "build" ? "./" : "/" }))`.

---

### Quick checks

1. **CPU** — Is one core pegged? Suspect an infinite loop (including Svelte reactivity).
2. **Console** — Any errors before first paint?
3. **Network** — Does `document` complete while JS is pending? Distinguish “no JS ran” from “JS ran but `fetch` hangs”.
4. **Minimal repro** — Comment out `onMount` / reactive blocks until the tab loads normally.

---

### Related paths

- UI: `apps/desktop/src/App.svelte` (scroll / reactivity).
- Vite: `apps/desktop/vite.config.ts` (`base`, HMR).
- Tauri shell: `apps/desktop/src-tauri/` (currently minimal wrapper; no custom `invoke` required for the chat UI in browser).

---

### What was removed or simplified while debugging (restore only if needed)

| Removed / simplified | Reason | When to bring back |
|---------------------|--------|-------------------|
| Custom Rust `invoke` + `reqwest` (`eson_agent_http`) | Suspected WKWebView / IPC hangs | Only if **`fetch` to the agent fails again** inside the Tauri webview (not in normal browsers). |
| `@tauri-apps/plugin-http` | Same | Same as above. |
| `desktopSettings` / gating agent on load | Reduced surface while isolating the bug | When you add a **Settings** screen and want an explicit “allow agent network” toggle. |
| Vite `/eson-api` proxy | Simplified to direct `AGENT` URL | Optional convenience for same-origin dev; not required if CORS on the agent allows the dev origin. |
| `tauri-plugin-shell` | Unused in minimal shell | When you need **open URL** or other shell actions from the UI. |

**Recommendation:** Keep the **minimal Tauri shell + plain `fetch`** until a real limitation appears again. The freeze was **not** fixed by those layers—it was the Svelte update loop.

---

### WKWebView: `fetch` error “The string did not match the expected pattern.”

WebKit throws this when **`fetch` / `URL`** get a string that is not a valid absolute URL in that context. Typical causes:

- **`VITE_ESON_AGENT_URL=/eson-api`** (relative) while **`window.location.origin`** is not a normal **`http:` / `https:`** origin (e.g. some Tauri embedded schemes), so resolving the path fails.
- **Untrimmed** env values (spaces) producing a malformed base.

**Fix (in app):** Resolve the agent base with `new URL(relativePrefix, origin)` only when `origin` starts with `http://` or `https://`; otherwise fall back to **`http://127.0.0.1:8787`** (or set `VITE_ESON_AGENT_URL` to a full **`http://…` / `https://…`** URL for the packaged app). See **`agentBase()`** in `apps/desktop/src/App.svelte`.
