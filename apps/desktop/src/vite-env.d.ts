/// <reference types="svelte" />
/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_ESON_AGENT_URL?: string;
  /** Set by Tauri CLI when `beforeDevCommand` runs (e.g. `tauri dev`). */
  readonly TAURI_ENV_PLATFORM?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
