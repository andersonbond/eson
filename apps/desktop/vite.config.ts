import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri CLI sets these when it runs `beforeDevCommand` (e.g. `npm run dev`).
const tauriCli = Boolean(process.env.TAURI_ENV_PLATFORM);
// Physical device / special Tauri setups (see https://v2.tauri.app/start/frontend/vite/)
const tauriDevHost = process.env.TAURI_DEV_HOST;

export default defineConfig(({ command }) => ({
  // `./` only for `vite build` (Tauri asset paths). `base: "./"` in dev breaks module loading → blank UI.
  base: command === "build" ? "./" : "/",
  plugins: [svelte()],
  clearScreen: false,
  envPrefix: ["VITE_", "TAURI_ENV_"],
  server: {
    port: 1420,
    strictPort: true,
    host: tauriDevHost ?? "127.0.0.1",
    hmr: tauriCli
      ? false
      : tauriDevHost
        ? { protocol: "ws", host: tauriDevHost, port: 1421 }
        : { protocol: "ws", host: "127.0.0.1", port: 1420 },
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target:
      process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
    outDir: "build",
    emptyOutDir: true,
  },
}));
