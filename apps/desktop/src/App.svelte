<script lang="ts">
  import { onDestroy, onMount, tick } from "svelte";
  import { io, type Socket } from "socket.io-client";
  import { marked } from "marked";
  import DOMPurify from "dompurify";
  import Chart from "chart.js/auto";
  // Tauri's HTTP plugin routes `fetch` through Rust's `reqwest` instead
  // of the WebKit `URLSession`, so long-running POSTs against
  // `eson-agent` (multi-round local-Ollama turns, vision tools) don't
  // hit WebKit's hard ~60 s `timeoutIntervalForRequest` cap. We import
  // it lazily-aware (it throws when running outside Tauri, e.g. `vite
  // dev` in a plain browser) and fall back to the WebView's `fetch`.
  import { fetch as tauriFetch } from "@tauri-apps/plugin-http";
  marked.setOptions({ gfm: true, breaks: true });

  // ---- Hardware preflight (Tauri) ---------------------------------------
  // The desktop shell exposes a `system_info` command that reports CPU + RAM
  // and whether the host meets the minimum requirements. We block app boot
  // until the user either passes the check or explicitly clicks "Continue
  // anyway". When run in a plain browser (e.g. `vite dev`), the Tauri global
  // is missing and we skip the check.
  type PreflightInfo = {
    cpu_logical: number;
    cpu_physical: number | null;
    memory_total_bytes: number;
    memory_total_gib: number;
    os_name: string | null;
    os_version: string | null;
    host_name: string | null;
    arch: string;
    is_apple_silicon: boolean;
    meets_requirements: boolean;
    failed_checks: string[];
    requirements: {
      min_cpu_logical: number;
      min_memory_gib: number;
      require_apple_silicon: boolean;
      platform_label: string;
    };
  };
  let preflight: PreflightInfo | null = null;
  let preflightChecked = false;
  let preflightBypassed = false;
  let preflightError: string | null = null;

  async function tauriInvoke<T>(cmd: string, args?: unknown): Promise<T | null> {
    try {
      const mod = await import("@tauri-apps/api/core");
      return (await mod.invoke(cmd, args as never)) as T;
    } catch {
      return null;
    }
  }

  async function runPreflight() {
    const info = await tauriInvoke<PreflightInfo>("system_info");
    if (!info) {
      preflight = null;
      preflightChecked = true;
      return;
    }
    preflight = info;
    preflightChecked = true;
  }

  async function preflightContinueAnyway() {
    preflightBypassed = true;
    const r = await tauriInvoke<unknown>("start_services_cmd");
    if (r === null) {
      preflightError = "Could not request the desktop shell to start services.";
    }
  }

  // Inline reactive declaration so Svelte 4 actually tracks the dependencies.
  // (Using a function call inside `{#if ...}` does not cause re-evaluation
  // when its internal `let` reads change, only when its arg list does.)
  $: showPreflightOverlay =
    !preflightChecked ||
    (preflight !== null && !preflight.meets_requirements && !preflightBypassed);

  function fmtGib(g: number): string {
    return `${g.toFixed(1)} GiB`;
  }

  function renderMd(text: string): string {
    const raw = marked.parse(text, { async: false }) as string;
    return DOMPurify.sanitize(raw);
  }

  /** Direct agent URL when we cannot use a same-origin proxy (e.g. Tauri `tauri://` origin). */
  const DEFAULT_AGENT = "http://127.0.0.1:8787";
  const RAW_AGENT =
    import.meta.env.VITE_ESON_AGENT_URL &&
    String(import.meta.env.VITE_ESON_AGENT_URL).trim().length > 0
      ? String(import.meta.env.VITE_ESON_AGENT_URL).trim().replace(/\/$/, "")
      : DEFAULT_AGENT;

  let cachedAgentBase: string | null = null;

  /**
   * Absolute base URL for fetch + Socket.IO. Relative values like `/eson-api` resolve against
   * `window.location` in real browsers; in WKWebView with a non-http(s) origin, fall back to
   * DEFAULT_AGENT so requests do not throw (WebKit: "The string did not match the expected pattern.").
   */
  function agentBase(): string {
    if (cachedAgentBase !== null) return cachedAgentBase;
    if (/^https?:\/\//i.test(RAW_AGENT)) {
      cachedAgentBase = RAW_AGENT;
      return cachedAgentBase;
    }
    if (typeof window === "undefined") {
      cachedAgentBase = DEFAULT_AGENT;
      return cachedAgentBase;
    }
    const origin = window.location?.origin ?? "";
    if (origin.startsWith("http://") || origin.startsWith("https://")) {
      const prefix = RAW_AGENT.startsWith("/") ? RAW_AGENT : `/${RAW_AGENT}`;
      try {
        cachedAgentBase = new URL(prefix, origin).href.replace(/\/$/, "");
        return cachedAgentBase;
      } catch {
        cachedAgentBase = DEFAULT_AGENT;
        return cachedAgentBase;
      }
    }
    cachedAgentBase = DEFAULT_AGENT;
    return cachedAgentBase;
  }

  const DEFAULT_FETCH_TIMEOUT_MS = 15_000;

  async function fetchWithTimeout(
    url: string,
    init: RequestInit = {},
    timeoutMs = DEFAULT_FETCH_TIMEOUT_MS,
  ): Promise<Response> {
    const ctrl = new AbortController();
    const t = setTimeout(() => ctrl.abort(), timeoutMs);
    try {
      return await fetch(url, { ...init, signal: ctrl.signal });
    } finally {
      clearTimeout(t);
    }
  }

  /** True when running inside the Tauri shell (so the HTTP plugin is
   * available). `vite dev` in a plain browser leaves `__TAURI__`
   * unset; we fall back to the WebView fetch in that case so the dev
   * loop keeps working. */
  function inTauri(): boolean {
    return (
      typeof window !== "undefined" &&
      // Tauri 2 sets `__TAURI_INTERNALS__`; the older `__TAURI__`
      // global is also still present in some builds. Either is fine.
      (("__TAURI_INTERNALS__" in window) || ("__TAURI__" in window))
    );
  }

  /** Long-lived `fetch` for `/session/message`. Routes through the
   * Tauri HTTP plugin (Rust `reqwest`) so the request isn't subject
   * to WebKit's ~60 s `URLSession.timeoutIntervalForRequest`. The
   * agent-side timeout (`ESON_LLM_HTTP_TIMEOUT_SECS`, default 600 s,
   * user-overridable in Settings → Advanced) is the only one that
   * matters for slow local-model turns now. Falls back to the
   * WebView fetch outside Tauri so `vite dev` still works. */
  async function fetchLongLived(
    url: string,
    init: RequestInit = {},
  ): Promise<Response> {
    if (inTauri()) {
      try {
        return await tauriFetch(url, init);
      } catch (e) {
        // Surface the underlying transport error to the caller — same
        // shape (throws on network failure / abort) as `fetch`.
        throw e;
      }
    }
    return fetch(url, init);
  }

  const STORAGE_META = "eson_chat_meta_v1";
  const msgsKey = (id: string) => `eson_msgs_${id}`;

  type Role = "user" | "assistant";

  /** One row in an assistant message's "Reasoning" trail. Mirrors the
   * orchestrator events (`llm_call_*`, `tool`, `provider_fallback`) so the
   * user can see exactly *what the agent thought and did* between their
   * prompt and the final reply — instead of having to dig through the
   * separate Activity panel. */
  interface ReasoningStep {
    id: string;
    /** `api` = LLM round-trip; `tool` = workspace tool call;
     * `fallback` = provider switch mid-turn; `thinking` = extended-thinking
     * / reasoning text emitted by the model itself (Anthropic thinking
     * blocks, OpenAI `reasoning_content`, Ollama `reasoning`, or
     * `<think>…</think>` tags from gemma / deepseek-r1 / qwq, etc.). */
    kind: "api" | "tool" | "fallback" | "thinking";
    headline: string;
    detail?: string;
    /** True while the underlying call is still in flight (e.g. `llm_call_begin`
     * received but not yet `llm_call_end`). Renders a spinner. */
    pending?: boolean;
    ok?: boolean;
    durationMs?: number;
    /** Optional collapsible payload — JSON args / shell command / output
     * preview / full thinking text. Renders behind a per-step "show
     * details" toggle. */
    input?: string;
    output?: string;
    shell?: string;
    /** Full reasoning prose — rendered inline (no toggle) so the user
     * actually reads what the model was thinking. Updated incrementally
     * as `llm_thinking_delta` chunks arrive on the SSE feed. */
    thinking?: string;
    /** Tag matching `api` and `thinking` steps to a single LLM call so
     * streamed deltas land on the right step even across provider
     * fallbacks within the same turn. */
    callId?: string;
    /** Tag matching `tool` steps to their `tool_begin` / `tool_progress`
     * heartbeats so the in-flight row gets updated in place rather than
     * appended every five seconds while a slow tool runs. */
    toolId?: string;
    /** Live elapsed time for an in-flight tool (driven by
     * `tool_progress`). Cleared when the final `tool` event lands and
     * `durationMs` becomes the canonical value. */
    elapsedMs?: number;
  }

  interface Msg {
    id: string;
    role: Role;
    text: string;
    /** Set on assistant placeholder while waiting for a turn to complete.
     * Cleared when the orchestrator's `turn_end` (or HTTP response, whichever
     * arrives first) populates the final answer. Stripped before persisting
     * so a refresh while the agent is mid-turn never leaves a stuck bubble. */
    pending?: boolean;
    /** Inline Chart.js payload — set when an assistant message *is* a chart
     * (emitted by the `render_chart` tool via the `chart_render` socket
     * event). Lets charts interleave with text in chronological order
     * instead of always pinning to the bottom of the thread. */
    chart?: ChartTile;
    /** Chain-of-thought trail (tools + LLM hops + fallbacks). Persisted with
     * the message so a user re-opening the conversation still sees how the
     * agent reached the answer. */
    steps?: ReasoningStep[];
    /** Whether the "Reasoning" panel is expanded. Auto-expanded while
     * `pending`, auto-collapsed once the turn finishes. User toggles persist
     * for the session via this flag. */
    stepsOpen?: boolean;
  }

  interface SessionMeta {
    id: string;
    title: string;
    updated: number;
    provider?: AiProvider;
  }

  interface ChartTile {
    id: string;
    title: string;
    chart_type: string;
    labels: string[];
    series: { name?: string; values?: number[] }[];
  }

  /** In-flight reconciliation handle for `send()`. The HTTP POST to
   * `/session/message` can be dropped by WebKit's URLSession after ~60 s
   * even though the agent is still working (e.g. a 100 s `analyze_visual`
   * call). The orchestrator emits `turn_end` over WebSocket when the
   * answer is actually ready, so we let either path resolve the placeholder
   * — whichever arrives first wins, the other is a no-op. */
  type PendingTurn = {
    sessionId: string;
    placeholderId: string;
    startedAt: number;
    watchdog: ReturnType<typeof setTimeout> | null;
    /** AbortController for the in-flight `/session/message` HTTP request.
     * Lives inside the pending turn (instead of a single module-level
     * variable) so *each session's* turn is aborted independently — a
     * user switching chats mid-turn must not cancel the background turn
     * they just walked away from. */
    abort: AbortController | null;
  };
  /** One entry per session with an in-flight turn. Keyed by `sessionId`
   * so background sessions keep streaming reasoning + answer even while
   * the user browses another chat; when they return, the placeholder
   * picks up where it left off because the orchestrator kept writing
   * into that session's message store. */
  let pendingTurns = new Map<string, PendingTurn>();
  /** Hard ceiling: if neither HTTP nor `turn_end` resolves within this
   * window, mark the placeholder as failed so the chat doesn't stay
   * "Thinking…" forever. Defaults to 30 min — local Ollama on a slow
   * CPU can chew through 5+ rounds of tool-loop with each round taking
   * 2-3 min. User-overridable via Settings → AI Provider → Advanced
   * (persisted in localStorage as `pendingTurnTimeoutMs`). Hard floor
   * of 60 s and ceiling of 6 h to guard against typos. */
  const DEFAULT_PENDING_TURN_TIMEOUT_MS = 30 * 60 * 1000;
  const PENDING_TURN_TIMEOUT_MIN_MS = 60 * 1000;
  const PENDING_TURN_TIMEOUT_MAX_MS = 6 * 60 * 60 * 1000;

  /** Mount / update Chart.js on a canvas (tool `render_chart` emits `chart_render`). */
  function chartCanvas(node: HTMLCanvasElement, tile: ChartTile) {
    let inst: InstanceType<typeof Chart> | null = null;
    const palette = [
      "#4f9cf9",
      "#38bdf8",
      "#22d3ee",
      "#34d399",
      "#f59e0b",
      "#f97316",
      "#a78bfa",
      "#f472b6",
    ];

    function draw(t: ChartTile) {
      inst?.destroy();
      const raw = (t.chart_type || "bar").toLowerCase();
      const chartJsType =
        raw === "area" ? "line" : raw === "scatter" ? "scatter" : raw === "pie" ? "pie" : raw === "radar" ? "radar" : raw === "line" ? "line" : "bar";
      const css = getComputedStyle(document.documentElement);
      const axisGrid = css.getPropertyValue("--wash-border").trim() || "rgba(127,127,127,0.2)";
      const axisText = css.getPropertyValue("--wash-muted").trim() || "rgba(127,127,127,0.85)";
      const tooltipBg = css.getPropertyValue("--wash-panel").trim() || "#111";
      const tooltipText = css.getPropertyValue("--wash-text").trim() || "#fff";

      const datasets = (t.series || []).map((s, i) => {
        const base = palette[i % palette.length];
        const grad = node
          .getContext("2d")
          ?.createLinearGradient(0, 0, 0, node.height || 280);
        if (grad) {
          grad.addColorStop(0, base + "aa");
          grad.addColorStop(1, base + "14");
        }
        return {
          label: s.name ?? `Series ${i + 1}`,
          data: s.values ?? [],
          borderColor: base,
          backgroundColor: raw === "pie" || raw === "radar" ? base + "99" : (grad ?? base + "33"),
          pointBackgroundColor: base,
          pointBorderColor: "#ffffff",
          pointBorderWidth: raw === "line" || raw === "area" ? 1.5 : 1,
          pointRadius: raw === "line" || raw === "area" ? 3 : raw === "scatter" ? 4 : 2,
          pointHoverRadius: raw === "line" || raw === "area" ? 5 : 6,
          borderWidth: raw === "line" || raw === "area" ? 2.5 : 1.8,
          fill: raw === "area",
          tension: raw === "line" || raw === "area" ? 0.38 : 0,
          cubicInterpolationMode: raw === "line" || raw === "area" ? "monotone" : undefined,
          borderRadius: raw === "bar" ? 9 : 0,
        };
      });
      inst = new Chart(node, {
        type: chartJsType as "bar",
        data: { labels: t.labels, datasets },
        options: {
          responsive: true,
          maintainAspectRatio: false,
          interaction: { mode: "index", intersect: false },
          animation: { duration: 700, easing: "easeOutQuart" },
          plugins: {
            legend: {
              position: "top",
              labels: {
                usePointStyle: true,
                boxWidth: 8,
                color: axisText,
                padding: 14,
              },
            },
            title: {
              display: Boolean(t.title),
              text: t.title,
              color: tooltipText,
              font: { size: 14, weight: 600 },
              padding: { top: 6, bottom: 14 },
            },
            tooltip: {
              backgroundColor: tooltipBg,
              titleColor: tooltipText,
              bodyColor: tooltipText,
              borderColor: axisGrid,
              borderWidth: 1,
              padding: 10,
              cornerRadius: 8,
            },
          },
          scales:
            raw === "pie" || raw === "radar"
              ? undefined
              : {
                  x: {
                    grid: { color: axisGrid },
                    ticks: { color: axisText, maxRotation: 0 },
                  },
                  y: {
                    beginAtZero: true,
                    grid: { color: axisGrid },
                    ticks: { color: axisText },
                  },
                },
        },
      });
    }
    draw(tile);
    return {
      update(newTile: ChartTile) {
        draw(newTile);
      },
      destroy() {
        inst?.destroy();
        inst = null;
      },
    };
  }

  let sessions: SessionMeta[] = [];
  let activeSessionId: string | null = null;
  let messages: Msg[] = [];
  let input = "";
  /** True while the **active** session has an in-flight turn. Derived
   * reactively from `pendingTurns` + `activeSessionId` so a session
   * switch back to a still-working chat automatically re-shows the
   * "Stop" button and disables the composer — without us having to
   * manually set it on every transition. */
  let loading = false;
  $: loading = !!activeSessionId && pendingTurns.has(activeSessionId);
  let agentOk = false;

  /** Line from agent `GET /system/health` (host running eson-agent). */
  let systemStatsText = "";
  let systemStatsReady = false;
  let systemStatsPoll: ReturnType<typeof setInterval> | undefined;

  function formatBytes(n: number): string {
    if (!Number.isFinite(n) || n < 0) return "—";
    if (n < 1024) return `${Math.round(n)} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 * 1024 * 1024)
      return `${(n / (1024 * 1024)).toFixed(1)} MB`;
    return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
  }

  async function refreshSystemStats() {
    try {
      const r = await fetchWithTimeout(`${agentBase()}/system/health`);
      systemStatsReady = true;
      if (!r.ok) {
        systemStatsText = "";
        return;
      }
      const j = (await r.json()) as {
        cpu_usage_percent_avg?: number | null;
        memory_total_bytes?: number | null;
        memory_used_bytes?: number | null;
        memory_used_percent?: number | null;
        platform?: string;
      };
      const cpu = j.cpu_usage_percent_avg;
      const total = j.memory_total_bytes;
      const used = j.memory_used_bytes;
      if (
        cpu == null ||
        total == null ||
        used == null ||
        typeof cpu !== "number" ||
        typeof total !== "number" ||
        typeof used !== "number"
      ) {
        systemStatsText =
          j.platform === "other"
            ? "System: CPU/RAM metrics not available on this agent build"
            : "";
        return;
      }
      const memPct =
        j.memory_used_percent != null &&
        typeof j.memory_used_percent === "number"
          ? j.memory_used_percent
          : total > 0
            ? (used / total) * 100
            : 0;
      systemStatsText = `CPU ${cpu.toFixed(0)}% · RAM ${formatBytes(used)} / ${formatBytes(total)} (${memPct.toFixed(0)}% used)`;
    } catch {
      systemStatsReady = true;
      systemStatsText = "";
    }
  }
  let socketConnected = false;
  let voiceOpen = false;
  let activityOpen = true;
  let actListEl: HTMLDivElement | null = null;
  let threadEl: HTMLDivElement | null = null;
  let scrollPinned = true;
  /// Whether the Activity panel should auto-stick to the newest entry. Becomes
  /// `false` as soon as the user scrolls upward; restored to `true` once they
  /// scroll back near the bottom (or click the "Jump to latest" affordance).
  let actScrollPinned = true;

  type ActivityItem = {
    id: string;
    t: number;
    kind: "info" | "ok" | "err";
    text: string;
    /** Orchestrator / tool timeline (distinct styling in Activity panel). */
    orch?: boolean;
    /** Streaming coalesce tag (e.g. `thinking:<call_id>`). When set, a new
     *  `pushActivity` with the same key updates this row in place instead of
     *  appending — keeps SSE token floods from burying every other event. */
    coalesceKey?: string;
  };
  let activity: ActivityItem[] = [];
  /** Running per-call accumulator for streamed reasoning. Used to render a
   *  rolling tail in the single coalesced Activity row so the user sees a
   *  meaningful "…assist the user with cybersecurity" preview instead of
   *  isolated tokens. Cleared opportunistically when the call ends. */
  const thinkingAccum = new Map<string, string>();
  const THINKING_ACCUM_MAX = 2000;
  const THINKING_ACCUM_PREVIEW = 180;
  /** Per-call accumulator for the streaming **answer** (everything outside
   *  `<think>` / `thinking_delta`). Mirrors `thinkingAccum` but powers a
   *  coalesced "Generating" row in Activity so a slow Ollama answer phase
   *  shows continuous progress instead of dead silence between the last
   *  reasoning token and `turn_end`. */
  const contentAccum = new Map<string, string>();
  const CONTENT_ACCUM_MAX = 4000;
  const CONTENT_ACCUM_PREVIEW = 180;

  let socket: Socket | null = null;

  type ShellView = "chat" | "settings" | "workspace";
  let shellView: ShellView = "chat";
  type AiProvider = "anthropic" | "openai" | "ollama";
  const PROVIDER_STORAGE = "eson_provider";
  const PROVIDER_FIELDS_STORAGE = "eson_provider_settings_v1";

  type ProviderFieldsStored = {
    anthropicModel?: string;
    anthropicApiKey?: string;
    openaiModel?: string;
    openaiApiKey?: string;
    ollamaUrl?: string;
    ollamaModel?: string;
    visionProvider?: string;
    visionModel?: string;
    /** Per-request HTTP timeout sent to the agent on `/session/message`.
     * Maps to `ProviderSettings.http_timeout_secs` server-side. Empty/0
     * → fall through to `ESON_LLM_HTTP_TIMEOUT_SECS` (default 600 s). */
    httpTimeoutSecs?: number;
    /** Frontend-only watchdog (no server round-trip). Caps how long the
     * UI waits for `turn_end` before giving up on the placeholder. */
    pendingTurnTimeoutMs?: number;
  };

  type VisionProvider = "ollama" | "anthropic" | "openai";
  function isVisionProvider(v: unknown): v is VisionProvider {
    return v === "ollama" || v === "anthropic" || v === "openai";
  }

  function readProviderFieldsFromStorage(): ProviderFieldsStored {
    if (typeof localStorage === "undefined") return {};
    try {
      const raw = localStorage.getItem(PROVIDER_FIELDS_STORAGE);
      if (!raw) return {};
      const j = JSON.parse(raw) as unknown;
      return j && typeof j === "object" ? (j as ProviderFieldsStored) : {};
    } catch {
      return {};
    }
  }

  const _pf = readProviderFieldsFromStorage();
  const str = (v: unknown) => (typeof v === "string" ? v : "");

  let aiProvider: AiProvider =
    typeof localStorage !== "undefined" &&
    ["anthropic", "openai", "ollama"].includes(
      localStorage.getItem(PROVIDER_STORAGE) ?? "",
    )
      ? (localStorage.getItem(PROVIDER_STORAGE) as AiProvider)
      : "anthropic";
  // `editingProvider` drives which provider's form is visible in Settings →
  // AI Provider. It's decoupled from `aiProvider` (the *primary* used for
  // chat) so the user can inspect or edit Anthropic's fields without
  // accidentally switching their primary away from Ollama, and vice versa.
  // The explicit "Set as primary" checkbox inside each form is what flips
  // `aiProvider`. Starts aligned with the current primary on load.
  let editingProvider: AiProvider = aiProvider;
  let providerAvailability: Record<AiProvider, boolean> = {
    anthropic: true,
    openai: true,
    ollama: true,
  };
  // `ready` = configured AND reachable. For Ollama the agent does a 1.5 s
  // TCP/HTTP probe; for cloud providers `ready` mirrors `available`.
  let providerReady: Record<AiProvider, boolean> = {
    anthropic: true,
    openai: true,
    ollama: true,
  };
  // One-line dismissable notice shown above the chat input when we
  // auto-switched the active provider because the persisted choice
  // wasn't reachable.
  let providerSwitchNotice: string | null = null;
  let anthropicModel = str(_pf.anthropicModel);
  let anthropicApiKey = str(_pf.anthropicApiKey);
  let openaiModel = str(_pf.openaiModel);
  let openaiApiKey = str(_pf.openaiApiKey);
  let ollamaUrl = str(_pf.ollamaUrl);
  let ollamaModel = str(_pf.ollamaModel);
  // Vision routing — independent of chat provider so a user can chat with
  // Anthropic while still letting Ollama handle multimodal locally (or vice
  // versa). Empty values fall through to the agent's `secrets.env` defaults.
  let visionProvider: VisionProvider = isVisionProvider(_pf.visionProvider)
    ? _pf.visionProvider
    : "ollama";
  let visionModel = str(_pf.visionModel);

  /** Per-request HTTP timeout in seconds. Sent to the agent in every
   * `/session/message` body so a slow Ollama round can be given more
   * headroom from the UI without restarting the agent. `0` / blank →
   * use the agent-side default (`ESON_LLM_HTTP_TIMEOUT_SECS`, 600 s).
   * Must round-trip through `Number.isFinite` because old localStorage
   * blobs may contain a string. */
  let httpTimeoutSecs: number =
    typeof _pf.httpTimeoutSecs === "number" && Number.isFinite(_pf.httpTimeoutSecs)
      ? Math.max(0, Math.floor(_pf.httpTimeoutSecs))
      : 0;
  /** Frontend-only watchdog. Drives the `setTimeout` that fails a stuck
   * placeholder. Survives reloads via localStorage. Reactive — bumping
   * it in the UI takes effect on the **next** turn. */
  let pendingTurnTimeoutMs: number =
    typeof _pf.pendingTurnTimeoutMs === "number" &&
    Number.isFinite(_pf.pendingTurnTimeoutMs) &&
    _pf.pendingTurnTimeoutMs >= PENDING_TURN_TIMEOUT_MIN_MS &&
    _pf.pendingTurnTimeoutMs <= PENDING_TURN_TIMEOUT_MAX_MS
      ? _pf.pendingTurnTimeoutMs
      : DEFAULT_PENDING_TURN_TIMEOUT_MS;

  // Defaults reported by `/session/provider-defaults` (sourced from the
  // bundled `secrets.env` → sidecar env vars). Shown as muted helper text
  // under each input + via the "Reset to defaults" button. Independent of
  // user-typed values so the user can always see what the agent would use
  // if a field were left blank.
  type ProviderDefaults = {
    anthropicModel: string;
    anthropicApiKey: string;
    anthropicConfigured: boolean;
    openaiModel: string;
    openaiApiKey: string;
    openaiConfigured: boolean;
    ollamaUrl: string;
    ollamaModel: string;
    visionProvider: VisionProvider;
    visionModel: string;
  };
  let providerDefaults: ProviderDefaults = {
    anthropicModel: "",
    anthropicApiKey: "",
    anthropicConfigured: false,
    openaiModel: "",
    openaiApiKey: "",
    openaiConfigured: false,
    ollamaUrl: "",
    ollamaModel: "",
    visionProvider: "ollama",
    visionModel: "",
  };
  type OllamaInstallPhase =
    | "idle"
    | "checking"
    | "installing_ollama"
    | "starting_ollama"
    | "pulling_model"
    | "ready"
    | "failed";
  type OllamaInstallStatus = {
    installed: boolean;
    running: boolean;
    model_ready: boolean;
    phase: OllamaInstallPhase;
    in_progress: boolean;
    last_error?: string | null;
    progress_log_tail: string[];
  };
  let ollamaInstall: OllamaInstallStatus = {
    installed: false,
    running: false,
    model_ready: false,
    phase: "idle",
    in_progress: false,
    last_error: null,
    progress_log_tail: [],
  };
  let ollamaInstallBusy = false;
  let ollamaInstallPoll: ReturnType<typeof setInterval> | null = null;

  // Background-automation provider (cron skills + inbox watcher).
  // Persisted server-side under workspace/db/background_settings.json.
  let bgProvider: AiProvider = "ollama";
  let bgAnthropicModel = "";
  let bgOpenaiModel = "";
  let bgOllamaUrl = "";
  let bgOllamaModel = "";
  let bgResolvedProvider: AiProvider | null = null;
  let bgEnvDefault: AiProvider = "ollama";
  let bgSaveHint = "Saved automatically.";
  // Loop knobs (null = inherit env default; the resolved view shows what's
  // actually used). All persisted server-side too.
  let bgLoopEnabled: boolean | null = null;
  let bgInboxAuto: boolean | null = null;
  let bgHeartbeatSec: number | null = null;
  let bgInboxDebounceMs: number | null = null;
  let bgResolvedLoopEnabled = false;
  let bgResolvedInboxAuto = true;
  let bgResolvedHeartbeatSec = 60;
  let bgResolvedInboxDebounceMs = 500;
  let bgEnvLoopEnabled = false;
  let bgEnvInboxAuto = true;
  let bgEnvHeartbeatSec = 60;
  let bgEnvInboxDebounceMs = 500;

  const THEME_STORAGE = "eson_theme";
  const SIDEBAR_STORAGE = "eson_sidebar_collapsed";

  let themeMode: "light" | "dark" =
    typeof localStorage !== "undefined" &&
    localStorage.getItem(THEME_STORAGE) === "dark"
      ? "dark"
      : "light";

  if (typeof document !== "undefined") {
    document.documentElement.dataset.theme = themeMode;
  }

  let sidebarCollapsed: boolean =
    typeof localStorage !== "undefined" &&
    localStorage.getItem(SIDEBAR_STORAGE) === "1";

  function applyTheme(mode: "light" | "dark") {
    themeMode = mode;
    document.documentElement.dataset.theme = mode;
    try {
      localStorage.setItem(THEME_STORAGE, mode);
    } catch {
      /* ignore */
    }
  }

  function toggleSidebar() {
    sidebarCollapsed = !sidebarCollapsed;
    try {
      localStorage.setItem(SIDEBAR_STORAGE, sidebarCollapsed ? "1" : "0");
    } catch {
      /* ignore */
    }
  }

  function setAiProvider(p: AiProvider) {
    aiProvider = p;
    try {
      localStorage.setItem(PROVIDER_STORAGE, p);
    } catch {
      /* ignore */
    }
  }

  /// Handle the "Set as primary" checkbox on each provider's form. There must
  /// always be exactly one primary — users unchecking a checkbox would leave
  /// the chat with nothing to call — so we no-op (and snap the checkbox back
  /// to its checked state) on attempts to uncheck. Checking a different
  /// provider's box flips `aiProvider` and surfaces an Activity row so the
  /// change is visible above the chat.
  function togglePrimary(p: AiProvider, input: HTMLInputElement) {
    if (input.checked) {
      if (aiProvider === p) return;
      const from = aiProvider;
      setAiProvider(p);
      pushActivity(
        "info",
        `Primary AI provider · ${labelFor(from)} → ${labelFor(p)}`,
      );
    } else {
      // Can't uncheck — a primary must always be selected.
      input.checked = true;
    }
  }

  function persistProviderFields() {
    if (typeof localStorage === "undefined") return;
    try {
      localStorage.setItem(
        PROVIDER_FIELDS_STORAGE,
        JSON.stringify({
          anthropicModel,
          anthropicApiKey,
          openaiModel,
          openaiApiKey,
          ollamaUrl,
          ollamaModel,
          visionProvider,
          visionModel,
          httpTimeoutSecs,
          pendingTurnTimeoutMs,
        }),
      );
    } catch {
      /* ignore */
    }
  }

  /** Explicit save for UX; fields also persist automatically on change.
   * After saving we re-probe provider availability so a freshly-fixed
   * Ollama URL (or new key) flips its `ready` state without restart. */
  function saveProviderSettings() {
    persistProviderFields();
    pushActivity("ok", "AI provider settings saved");
    void loadProviderAvailability();
  }

  $: {
    anthropicModel;
    anthropicApiKey;
    openaiModel;
    openaiApiKey;
    ollamaUrl;
    ollamaModel;
    visionProvider;
    visionModel;
    httpTimeoutSecs;
    pendingTurnTimeoutMs;
    persistProviderFields();
  }

  function providerSettingsBody() {
    return {
      anthropic: {
        model: anthropicModel || undefined,
        api_key: anthropicApiKey || undefined,
      },
      openai: {
        model: openaiModel || undefined,
        api_key: openaiApiKey || undefined,
      },
      ollama: {
        url: ollamaUrl || undefined,
        model: ollamaModel || undefined,
      },
      vision: {
        provider: visionProvider || undefined,
        model: visionModel || undefined,
      },
      // 0/blank → fall through to the agent's `ESON_LLM_HTTP_TIMEOUT_SECS`
      // (default 600 s). Only forward a positive value to keep the body
      // minimal and let the agent honor its own defaults when the user
      // hasn't customized.
      http_timeout_secs:
        httpTimeoutSecs > 0 ? Math.floor(httpTimeoutSecs) : undefined,
    };
  }

  async function loadProviderDefaultsFromServer() {
    try {
      const r = await fetchWithTimeout(`${agentBase()}/session/provider-defaults`);
      if (!r.ok) return;
      const d = (await r.json()) as {
        anthropic?: {
          model?: string;
          api_key?: string | null;
          api_key_configured?: boolean;
        };
        openai?: {
          model?: string;
          api_key?: string | null;
          api_key_configured?: boolean;
        };
        ollama?: { url?: string; model?: string };
        vision?: { provider?: string; model?: string };
      };
      const visionDefault: VisionProvider = isVisionProvider(d.vision?.provider)
        ? (d.vision!.provider as VisionProvider)
        : "ollama";
      providerDefaults = {
        anthropicModel: str(d.anthropic?.model),
        anthropicApiKey: d.anthropic?.api_key != null ? String(d.anthropic.api_key) : "",
        anthropicConfigured: Boolean(d.anthropic?.api_key_configured),
        openaiModel: str(d.openai?.model),
        openaiApiKey: d.openai?.api_key != null ? String(d.openai.api_key) : "",
        openaiConfigured: Boolean(d.openai?.api_key_configured),
        ollamaUrl: str(d.ollama?.url),
        ollamaModel: str(d.ollama?.model),
        visionProvider: visionDefault,
        visionModel: str(d.vision?.model),
      };
      // Only seed empty fields — never overwrite a value the user explicitly
      // typed. The "Reset to defaults" button gives them a way to discard.
      if (!anthropicModel.trim() && providerDefaults.anthropicModel)
        anthropicModel = providerDefaults.anthropicModel;
      if (!anthropicApiKey.trim() && providerDefaults.anthropicApiKey)
        anthropicApiKey = providerDefaults.anthropicApiKey;
      if (!openaiModel.trim() && providerDefaults.openaiModel)
        openaiModel = providerDefaults.openaiModel;
      if (!openaiApiKey.trim() && providerDefaults.openaiApiKey)
        openaiApiKey = providerDefaults.openaiApiKey;
      if (!ollamaUrl.trim() && providerDefaults.ollamaUrl)
        ollamaUrl = providerDefaults.ollamaUrl;
      if (!ollamaModel.trim() && providerDefaults.ollamaModel)
        ollamaModel = providerDefaults.ollamaModel;
      // Seed vision model only when the user hasn't explicitly set one.
      // The provider is *always* user-controlled (we keep their selection)
      // so the env default doesn't override their UI pick on every load.
      if (!visionModel.trim() && providerDefaults.visionModel)
        visionModel = providerDefaults.visionModel;
    } catch {
      /* ignore */
    }
  }

  /// Render an API key as `sk-ant-…XYZW` for the muted "Default" hint so we
  /// don't print full secrets, but the user can still recognize which key
  /// is in `secrets.env` vs what they pasted into the UI.
  function maskKey(k: string): string {
    if (!k) return "";
    if (k.length <= 8) return "••••";
    return `${k.slice(0, 6)}…${k.slice(-4)}`;
  }

  /// Wipe localStorage values for the active provider's fields so they
  /// re-take the defaults from `secrets.env`. Also nudges the agent to
  /// re-probe reachability after a save (so e.g. switching Ollama from a
  /// dead LAN box → localhost flips `ready.ollama` immediately).
  function resetActiveProviderToDefaults() {
    if (editingProvider === "anthropic") {
      anthropicModel = providerDefaults.anthropicModel;
      anthropicApiKey = providerDefaults.anthropicApiKey;
    } else if (editingProvider === "openai") {
      openaiModel = providerDefaults.openaiModel;
      openaiApiKey = providerDefaults.openaiApiKey;
    } else {
      ollamaUrl = providerDefaults.ollamaUrl;
      ollamaModel = providerDefaults.ollamaModel;
    }
    pushActivity("info", `${labelFor(editingProvider)} fields reset to secrets.env defaults`);
    void loadProviderAvailability();
  }

  /// Canonical default model for each vision provider. Mirrors
  /// `vision::VisionConfig::default_model_for` on the agent so the UI shows
  /// the same placeholder the agent will substitute when the field is
  /// blank.
  function defaultVisionModelFor(p: VisionProvider): string {
    if (p === "anthropic") return "claude-haiku-4-5-20251001";
    if (p === "openai") return "gpt-4o-mini";
    return "gemma4:e4b";
  }

  /// Switch the vision provider. Clears the model field if it looked like
  /// a model from the *previous* provider so we don't accidentally POST
  /// e.g. `gemma4:e4b` to the Anthropic API on the next image.
  function setVisionProvider(p: VisionProvider) {
    if (p === visionProvider) return;
    const prevDefault = defaultVisionModelFor(visionProvider);
    if (visionModel === prevDefault || !visionModel.trim()) {
      visionModel = "";
    }
    visionProvider = p;
  }

  function resetVisionToDefaults() {
    visionProvider = providerDefaults.visionProvider;
    visionModel = providerDefaults.visionModel;
    pushActivity("info", "Vision provider reset to secrets.env default");
  }

  function backgroundSettingsBody() {
    const settings: Record<string, unknown> = {};
    if (bgProvider === "anthropic" && bgAnthropicModel.trim()) {
      settings.anthropic = { model: bgAnthropicModel.trim() };
    }
    if (bgProvider === "openai" && bgOpenaiModel.trim()) {
      settings.openai = { model: bgOpenaiModel.trim() };
    }
    if (bgProvider === "ollama") {
      const ollama: Record<string, string> = {};
      if (bgOllamaUrl.trim()) ollama.url = bgOllamaUrl.trim();
      if (bgOllamaModel.trim()) ollama.model = bgOllamaModel.trim();
      if (Object.keys(ollama).length) settings.ollama = ollama;
    }
    const body: Record<string, unknown> = {
      provider: bgProvider,
      settings,
    };
    if (bgLoopEnabled !== null) body.loop_enabled = bgLoopEnabled;
    if (bgInboxAuto !== null) body.inbox_auto = bgInboxAuto;
    if (bgHeartbeatSec !== null) body.heartbeat_sec = bgHeartbeatSec;
    if (bgInboxDebounceMs !== null) body.inbox_debounce_ms = bgInboxDebounceMs;
    return body;
  }

  function applyBackgroundResponse(j: unknown) {
    if (!j || typeof j !== "object") return;
    const o = j as Record<string, unknown>;
    if (typeof o.provider === "string" &&
        ["anthropic", "openai", "ollama"].includes(o.provider)) {
      bgProvider = o.provider as AiProvider;
    }
    const env = o.env_defaults as Record<string, unknown> | undefined;
    if (env) {
      if (typeof env.provider === "string" &&
          ["anthropic", "openai", "ollama"].includes(env.provider)) {
        bgEnvDefault = env.provider as AiProvider;
      }
      if (typeof env.loop_enabled === "boolean") bgEnvLoopEnabled = env.loop_enabled;
      if (typeof env.inbox_auto === "boolean") bgEnvInboxAuto = env.inbox_auto;
      if (typeof env.heartbeat_sec === "number") bgEnvHeartbeatSec = env.heartbeat_sec;
      if (typeof env.inbox_debounce_ms === "number") bgEnvInboxDebounceMs = env.inbox_debounce_ms;
    }
    const r = o.resolved as Record<string, unknown> | undefined;
    if (r) {
      bgResolvedProvider =
        typeof r.provider === "string" &&
        ["anthropic", "openai", "ollama"].includes(r.provider)
          ? (r.provider as AiProvider)
          : null;
      if (typeof r.loop_enabled === "boolean") bgResolvedLoopEnabled = r.loop_enabled;
      if (typeof r.inbox_auto === "boolean") bgResolvedInboxAuto = r.inbox_auto;
      if (typeof r.heartbeat_sec === "number") bgResolvedHeartbeatSec = r.heartbeat_sec;
      if (typeof r.inbox_debounce_ms === "number") bgResolvedInboxDebounceMs = r.inbox_debounce_ms;
    }
    bgLoopEnabled = typeof o.loop_enabled === "boolean" ? o.loop_enabled : null;
    bgInboxAuto = typeof o.inbox_auto === "boolean" ? o.inbox_auto : null;
    bgHeartbeatSec = typeof o.heartbeat_sec === "number" ? o.heartbeat_sec : null;
    bgInboxDebounceMs = typeof o.inbox_debounce_ms === "number" ? o.inbox_debounce_ms : null;
    const s = o.settings as Record<string, Record<string, string>> | undefined;
    if (s) {
      if (s.anthropic?.model) bgAnthropicModel = s.anthropic.model;
      if (s.openai?.model) bgOpenaiModel = s.openai.model;
      if (s.ollama?.url) bgOllamaUrl = s.ollama.url;
      if (s.ollama?.model) bgOllamaModel = s.ollama.model;
    }
  }

  function setBgLoopEnabled(v: boolean) {
    bgLoopEnabled = v;
    void saveBackgroundSettings();
  }

  function setBgInboxAuto(v: boolean) {
    bgInboxAuto = v;
    void saveBackgroundSettings();
  }

  function commitBgHeartbeat(raw: string) {
    const n = Number.parseInt(raw, 10);
    if (Number.isFinite(n) && n >= 10 && n <= 3600) {
      bgHeartbeatSec = n;
      void saveBackgroundSettings();
    }
  }

  function commitBgDebounce(raw: string) {
    const n = Number.parseInt(raw, 10);
    if (Number.isFinite(n) && n >= 50 && n <= 10_000) {
      bgInboxDebounceMs = n;
      void saveBackgroundSettings();
    }
  }

  async function loadBackgroundSettings() {
    try {
      const r = await fetchWithTimeout(`${agentBase()}/background/settings`);
      if (!r.ok) return;
      applyBackgroundResponse(await r.json());
    } catch {
      /* ignore */
    }
  }

  let bgSaveTimer: ReturnType<typeof setTimeout> | null = null;
  async function saveBackgroundSettings() {
    try {
      const r = await fetchWithTimeout(`${agentBase()}/background/settings`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(backgroundSettingsBody()),
      });
      if (!r.ok) {
        const txt = await r.text();
        bgSaveHint = `Save failed: ${txt.slice(0, 120)}`;
        return;
      }
      applyBackgroundResponse(await r.json());
      bgSaveHint = "Saved.";
    } catch (e) {
      bgSaveHint = `Save failed: ${(e as Error).message}`;
    }
  }

  function scheduleBackgroundSave() {
    if (bgSaveTimer) clearTimeout(bgSaveTimer);
    bgSaveTimer = setTimeout(() => {
      void saveBackgroundSettings();
    }, 350);
  }

  async function loadOllamaInstallStatus() {
    const s = await tauriInvoke<OllamaInstallStatus>("ollama_status");
    if (!s) return;
    const prevPhase = ollamaInstall.phase;
    const prevBusy = ollamaInstall.in_progress;
    ollamaInstall = s;
    ollamaInstallBusy = false;
    if (prevBusy && !s.in_progress) {
      if (s.phase === "ready") {
        pushActivity("ok", `Ollama ready · model ${ollamaModel || "gemma4:e4b"}`);
      } else if (s.phase === "failed") {
        pushActivity(
          "err",
          `Ollama install failed${s.last_error ? ` · ${s.last_error}` : ""}`,
        );
      }
      void loadProviderAvailability();
    } else if (prevPhase !== s.phase && s.in_progress) {
      pushActivity("info", `Ollama setup · ${ollamaPhaseLabel(s.phase)}`);
    }
  }

  function ollamaPhaseLabel(phase: OllamaInstallPhase): string {
    if (phase === "checking") return "Checking installation";
    if (phase === "installing_ollama") return "Installing Ollama";
    if (phase === "starting_ollama") return "Starting Ollama service";
    if (phase === "pulling_model") return "Pulling gemma4:e4b";
    if (phase === "ready") return "Ready";
    if (phase === "failed") return "Failed";
    return "Idle";
  }

  function ensureOllamaPoll() {
    if (ollamaInstallPoll) return;
    ollamaInstallPoll = setInterval(() => {
      if (shellView !== "settings") return;
      void loadOllamaInstallStatus();
    }, 1500);
  }

  function stopOllamaPoll() {
    if (ollamaInstallPoll) {
      clearInterval(ollamaInstallPoll);
      ollamaInstallPoll = null;
    }
  }

  async function installOllamaFromSettings() {
    ollamaInstallBusy = true;
    const s = await tauriInvoke<OllamaInstallStatus>("install_ollama_with_model");
    if (!s) {
      ollamaInstallBusy = false;
      pushActivity(
        "err",
        "Ollama installer unavailable (desktop shell only, macOS).",
      );
      return;
    }
    ollamaInstall = s;
    pushActivity("info", "Ollama setup started · install + pull gemma4:e4b");
    ensureOllamaPoll();
  }

  async function loadProviderAvailability() {
    try {
      const r = await fetchWithTimeout(`${agentBase()}/session/providers`);
      const text = await r.text();
      const parsed = parseJsonBody<{
        available?: Partial<Record<AiProvider, boolean>>;
        ready?: Partial<Record<AiProvider, boolean>>;
        default?: AiProvider;
      }>(text);
      if (r.ok && parsed.ok && parsed.value.available) {
        // Server only sees keys that came from `secrets.env` at startup. The
        // user can also supply per-message keys via the Settings panel
        // (persisted in localStorage and threaded through `providerSettingsBody`),
        // so OR-merge those locally — otherwise a fresh install would keep
        // Anthropic/OpenAI flagged as "unavailable" even after the user pastes
        // a key, and `autoSwitchIfUnreachable` would yank them back to Ollama.
        const localHasKey = {
          anthropic:
            anthropicApiKey.trim().length > 0 ||
            providerDefaults.anthropicConfigured,
          openai:
            openaiApiKey.trim().length > 0 || providerDefaults.openaiConfigured,
          // Ollama is URL-based; if the user typed any URL we'll let them
          // try it. The actual reachability check stays with the server probe.
          ollama: ollamaUrl.trim().length > 0,
        };
        providerAvailability = {
          anthropic: Boolean(parsed.value.available.anthropic) || localHasKey.anthropic,
          openai: Boolean(parsed.value.available.openai) || localHasKey.openai,
          ollama: Boolean(parsed.value.available.ollama) || localHasKey.ollama,
        };
        // Older agents won't return `ready`; treat it as identical to
        // `available` in that case (no auto-switch will fire).
        const serverReady = parsed.value.ready
          ? {
              anthropic: Boolean(parsed.value.ready.anthropic),
              openai: Boolean(parsed.value.ready.openai),
              ollama: Boolean(parsed.value.ready.ollama),
            }
          : { ...providerAvailability };
        // Cloud providers: trust a user-supplied key for "ready" too — the
        // first call will surface auth errors via the Activity panel. We
        // intentionally do NOT override Ollama readiness from the client
        // because Ollama needs an actual TCP probe.
        providerReady = {
          anthropic: serverReady.anthropic || localHasKey.anthropic,
          openai: serverReady.openai || localHasKey.openai,
          ollama: serverReady.ollama,
        };
        const fallback: AiProvider | null =
          parsed.value.default && providerReady[parsed.value.default]
            ? parsed.value.default
            : (["anthropic", "openai", "ollama"] as AiProvider[]).find(
                (p) => providerReady[p],
              ) ?? null;
        autoSwitchIfUnreachable(fallback);
      }
    } catch {
      /* ignore */
    }
  }

  function reasonForUnready(p: AiProvider): string {
    if (!providerAvailability[p]) {
      if (p === "anthropic") return "no ANTHROPIC_API_KEY configured";
      if (p === "openai") return "no OPENAI_API_KEY configured";
      return "no Ollama base URL configured";
    }
    if (p === "ollama") return "Ollama host did not respond (~1.5 s probe)";
    return "provider not reachable";
  }

  function autoSwitchIfUnreachable(fallback: AiProvider | null) {
    if (providerReady[aiProvider]) {
      providerSwitchNotice = null;
      return;
    }
    if (!fallback || fallback === aiProvider) {
      providerSwitchNotice = `${labelFor(aiProvider)} isn't reachable (${reasonForUnready(aiProvider)}) and no other provider is configured. Add a key under Settings.`;
      pushActivity("err", providerSwitchNotice);
      return;
    }
    const from = aiProvider;
    setAiProvider(fallback);
    providerSwitchNotice = `Switched to ${labelFor(fallback)} — ${labelFor(from)} ${reasonForUnready(from)}.`;
    pushActivity("info", `Provider switched · ${from} → ${fallback} (${reasonForUnready(from)})`);
  }

  function labelFor(p: AiProvider): string {
    if (p === "anthropic") return "Anthropic";
    if (p === "openai") return "OpenAI";
    return "Ollama";
  }

  type BrowseEntry = { name: string; is_dir: boolean };

  // Finder-style column browser. Each column is a directory listing; the
  // selected entry in column N drives column N+1 (a child directory listing
  // or a file preview pane).
  type WorkspaceColumn = {
    path: string; // workspace-relative; "" is root
    entries: BrowseEntry[];
    selectedName?: string;
    err?: string;
    loading?: boolean;
  };

  type CsvPreview = {
    kind: "csv";
    name: string;
    path: string;
    size: number;
    delimiter: string;
    headers: string[];
    rows: string[][];
    max_cols: number;
    total_data_rows: number;
    rows_truncated: boolean;
    preview_row_cap: number;
    preview_col_cap: number;
    skipped?: boolean;
    note?: string;
  };

  type ExcelSheet = {
    name: string;
    headers: string[];
    rows: string[][];
    total_rows: number;
    total_cols: number;
    rows_truncated: boolean;
    cols_truncated: boolean;
    error?: string;
  };

  type ExcelPreview = {
    kind: "excel";
    name: string;
    path: string;
    size: number;
    sheets: ExcelSheet[];
    sheet_count: number;
    preview_row_cap: number;
    preview_col_cap: number;
    skipped?: boolean;
    note?: string;
  };

  type TextPreview = {
    kind: "markdown" | "text" | "code";
    name: string;
    path: string;
    size: number;
    mime: string;
    text: string;
    truncated: boolean;
    preview_max_bytes: number;
  };

  type ImagePreview = {
    kind: "image";
    name: string;
    path: string;
    size: number;
    mime: string;
    data_base64?: string;
    skipped?: boolean;
    note?: string;
  };

  type StubPreview = {
    kind: "pdf" | "binary";
    name: string;
    path: string;
    size: number;
    mime?: string;
    note?: string;
  };

  type PreviewData =
    | CsvPreview
    | ExcelPreview
    | TextPreview
    | ImagePreview
    | StubPreview;

  type WorkspacePreviewState = {
    path: string;
    name: string;
    loading: boolean;
    err?: string;
    data?: PreviewData;
  };

  let workspaceRootLabel = "";
  let workspaceColumns: WorkspaceColumn[] = [];
  let workspacePreview: WorkspacePreviewState | null = null;
  let workspacePreviewSeq = 0;

  const BROWSE_404_HINT =
    "GET /workspace/browse returned 404. Rebuild and restart the agent from the eson/ folder: cargo build -p eson-agent && cargo run -p eson-agent (older binaries do not expose this route).";

  function parseJsonBody<T extends Record<string, unknown>>(
    text: string,
  ): { ok: true; value: T } | { ok: false; err: string } {
    const t = text.trim();
    if (!t) return { ok: true, value: {} as T };
    try {
      return { ok: true, value: JSON.parse(t) as T };
    } catch {
      return { ok: false, err: "Response was not valid JSON" };
    }
  }

  async function fetchWorkspaceListing(
    path: string,
  ): Promise<{ entries: BrowseEntry[]; path: string } | { err: string }> {
    const q = path ? `?path=${encodeURIComponent(path)}` : "";
    try {
      const r = await fetch(`${agentBase()}/workspace/browse${q}`);
      const text = await r.text();
      const parsed = parseJsonBody<{
        path?: string;
        entries?: BrowseEntry[];
        error?: string;
      }>(text);
      if (!parsed.ok) return { err: parsed.err };
      const j = parsed.value;
      if (!r.ok) {
        if (r.status === 404) return { err: j.error ?? BROWSE_404_HINT };
        return { err: j.error ?? `Request failed (${r.status})` };
      }
      return {
        entries: Array.isArray(j.entries) ? j.entries : [],
        path: j.path ?? path,
      };
    } catch (e) {
      return { err: e instanceof Error ? e.message : String(e) };
    }
  }

  async function loadRootColumn() {
    const res = await fetchWorkspaceListing("");
    if ("err" in res) {
      workspaceColumns = [
        { path: "", entries: [], err: res.err },
      ];
    } else {
      workspaceColumns = [
        { path: "", entries: res.entries },
      ];
    }
    workspacePreview = null;
  }

  async function openWorkspacePanel() {
    try {
      const ir = await fetch(`${agentBase()}/workspace/info`);
      const itext = await ir.text();
      const ip = parseJsonBody<{ root?: string }>(itext);
      if (ir.ok && ip.ok) {
        workspaceRootLabel = ip.value.root ?? "";
      } else {
        workspaceRootLabel = "";
      }
    } catch {
      workspaceRootLabel = "";
    }
    await loadRootColumn();
  }

  function toggleShellView(next: ShellView) {
    if (shellView === next) {
      shellView = "chat";
      return;
    }
    shellView = next;
    if (next === "workspace") {
      void openWorkspacePanel();
    } else if (next === "settings") {
      void loadProviderDefaultsFromServer();
      void loadProviderAvailability();
      void loadOllamaInstallStatus();
      ensureOllamaPoll();
    } else {
      stopOllamaPoll();
    }
  }

  /// Build the workspace-relative child path for an entry inside `parentPath`.
  function joinWsPath(parentPath: string, name: string): string {
    return parentPath ? `${parentPath}/${name}` : name;
  }

  /// Select an entry inside column index `colIdx`. If it's a directory, load
  /// its listing into column `colIdx + 1` (truncating any further columns).
  /// If it's a file, load a preview into the side preview pane.
  async function selectInColumn(colIdx: number, entry: BrowseEntry) {
    const col = workspaceColumns[colIdx];
    if (!col) return;
    workspaceColumns = workspaceColumns.map((c, i) =>
      i === colIdx ? { ...c, selectedName: entry.name } : c,
    );
    const childPath = joinWsPath(col.path, entry.name);
    if (entry.is_dir) {
      // Truncate everything to the right; insert a loading placeholder so the
      // UI shows the new column immediately, then fill it in.
      workspaceColumns = [
        ...workspaceColumns.slice(0, colIdx + 1),
        { path: childPath, entries: [], loading: true },
      ];
      workspacePreview = null;
      const res = await fetchWorkspaceListing(childPath);
      const next: WorkspaceColumn =
        "err" in res
          ? { path: childPath, entries: [], err: res.err }
          : { path: childPath, entries: res.entries };
      // Make sure the user hasn't navigated away in the meantime.
      const currentSelection = workspaceColumns[colIdx]?.selectedName;
      if (currentSelection !== entry.name) return;
      workspaceColumns = [
        ...workspaceColumns.slice(0, colIdx + 1),
        next,
      ];
    } else {
      // File click: drop any deeper navigation columns and show a preview.
      workspaceColumns = workspaceColumns.slice(0, colIdx + 1);
      await loadWorkspacePreview(childPath, entry.name);
    }
  }

  async function loadWorkspacePreview(path: string, name: string) {
    const seq = ++workspacePreviewSeq;
    workspacePreview = { path, name, loading: true };
    try {
      const r = await fetch(
        `${agentBase()}/workspace/preview?path=${encodeURIComponent(path)}`,
      );
      const text = await r.text();
      const parsed = parseJsonBody<Record<string, unknown>>(text);
      if (seq !== workspacePreviewSeq) return; // user moved on
      if (!parsed.ok) {
        workspacePreview = { path, name, loading: false, err: parsed.err };
        return;
      }
      const body = parsed.value;
      if (!r.ok) {
        const errMsg =
          (typeof body.error === "string" && body.error) ||
          `Request failed (${r.status})`;
        workspacePreview = { path, name, loading: false, err: errMsg };
        return;
      }
      workspacePreview = {
        path,
        name,
        loading: false,
        data: body as unknown as PreviewData,
      };
    } catch (e) {
      if (seq !== workspacePreviewSeq) return;
      workspacePreview = {
        path,
        name,
        loading: false,
        err: e instanceof Error ? e.message : String(e),
      };
    }
  }

  /// Active path = deepest column path, or selected file path if one is open.
  $: workspaceActivePath = workspacePreview?.path
    ?? workspaceColumns[workspaceColumns.length - 1]?.path
    ?? "";

  /// Reveal the currently-focused folder (or parent of the selected file)
  /// in Finder.
  async function revealWorkspaceInFinder() {
    let target = "";
    if (workspacePreview?.path) {
      const parts = workspacePreview.path.split("/").filter(Boolean);
      parts.pop();
      target = parts.join("/");
    } else {
      target =
        workspaceColumns[workspaceColumns.length - 1]?.path ?? "";
    }
    const opened = await tauriInvoke<string>("reveal_workspace_path", {
      rel: target || null,
    });
    if (opened) {
      pushActivity("ok", `Opened ${opened} in Finder`);
    } else {
      pushActivity(
        "err",
        "Could not open workspace in Finder (Tauri shell unavailable). Use the path shown above.",
      );
    }
  }

  function renderMarkdownPreview(text: string): string {
    try {
      const html = marked.parse(text) as string;
      return DOMPurify.sanitize(html);
    } catch {
      return DOMPurify.sanitize(`<pre>${escapeHtml(text)}</pre>`);
    }
  }

  function escapeHtml(s: string): string {
    return s
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function humanizeBytes(n: number): string {
    if (!Number.isFinite(n) || n < 0) return "—";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let value = n;
    let i = 0;
    while (value >= 1024 && i + 1 < units.length) {
      value /= 1024;
      i += 1;
    }
    return i === 0 ? `${n} ${units[0]}` : `${value.toFixed(1)} ${units[i]}`;
  }

  function persistMeta() {
    try {
      localStorage.setItem(
        STORAGE_META,
        JSON.stringify({ sessions, activeSessionId }),
      );
    } catch {
      /* ignore */
    }
  }

  function persistMessages() {
    if (!activeSessionId) return;
    try {
      // Strip transient `pending` flags so a refresh mid-turn doesn't
      // restore a permanent "Thinking…" placeholder. The chart payload
      // *is* persisted so charts re-render when switching back to the
      // session.
      const serializable = messages.map(({ pending: _p, ...rest }) => rest);
      localStorage.setItem(msgsKey(activeSessionId), JSON.stringify(serializable));
    } catch {
      /* ignore */
    }
  }

  function loadStored() {
    try {
      const raw = localStorage.getItem(STORAGE_META);
      if (raw) {
        const j = JSON.parse(raw) as {
          sessions?: SessionMeta[];
          activeSessionId?: string | null;
        };
        if (j.sessions?.length) sessions = j.sessions;
        if (j.activeSessionId) activeSessionId = j.activeSessionId;
      }
    } catch {
      sessions = [];
    }
  }

  function loadMessagesForSession(id: string) {
    try {
      const raw = localStorage.getItem(msgsKey(id));
      if (raw) {
        const parsed = JSON.parse(raw) as Msg[];
        if (Array.isArray(parsed)) {
          messages = parsed;
          // `persistMessages` strips the transient `pending` flag (so a
          // hard refresh doesn't restore a stuck placeholder). If this
          // session still has a live turn, re-mark its placeholder as
          // pending so the spinner + streaming re-attach cleanly.
          const pt = pendingTurns.get(id);
          if (pt) {
            messages = messages.map((m) =>
              m.id === pt.placeholderId && !m.pending
                ? { ...m, pending: true }
                : m,
            );
          }
          return;
        }
      }
    } catch {
      /* ignore */
    }
    messages = [];
  }

  /** Apply `mapper` to a session's message list, regardless of whether
   * it's the currently-active chat. Active session: updates the in-memory
   * `messages` reactive array (so the UI re-renders). Background session:
   * reads/writes `localStorage` directly so the placeholder keeps growing
   * while the user is on another chat. Called by every orchestrator
   * handler that mutates the in-flight bubble (tool steps, LLM thinking,
   * LLM content, final turn_end, …). */
  function updateMessagesForSession(
    sid: string,
    mapper: (msgs: Msg[]) => Msg[],
  ): void {
    if (sid === activeSessionId) {
      messages = mapper(messages);
      persistMessages();
      return;
    }
    try {
      const raw = localStorage.getItem(msgsKey(sid));
      const current = raw ? (JSON.parse(raw) as Msg[]) : [];
      if (!Array.isArray(current)) return;
      const next = mapper(current);
      // Mirror `persistMessages`: strip the transient `pending` flag
      // before writing. It'll be restored by `loadMessagesForSession`
      // if the session is still tracked in `pendingTurns`.
      const serializable = next.map(({ pending: _p, ...rest }) => rest);
      localStorage.setItem(msgsKey(sid), JSON.stringify(serializable));
    } catch {
      /* ignore */
    }
  }

  async function startServerSession(): Promise<string | null> {
    try {
      const start = await fetchWithTimeout(`${agentBase()}/session/start`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          provider: aiProvider,
          settings: providerSettingsBody(),
        }),
      });
      if (!start.ok) return null;
      const j = (await start.json()) as {
        session_id: string;
        provider?: AiProvider;
      };
      if (j.provider && j.provider !== aiProvider) setAiProvider(j.provider);
      return j.session_id;
    } catch {
      return null;
    }
  }

  async function newChat() {
    const id = await startServerSession();
    if (!id) return;
    shellView = "chat";
    const meta: SessionMeta = {
      id,
      title: "New chat",
      updated: Date.now(),
      provider: aiProvider,
    };
    sessions = [meta, ...sessions.filter((s) => s.id !== id)];
    // Persist the previous session's messages (including any in-flight
    // placeholder so it rehydrates on switch-back) before we flip the
    // active id. We deliberately do NOT abandon its pending turn — it's
    // still working and will keep streaming into its own store.
    persistMessages();
    activeSessionId = id;
    messages = [];
    persistMeta();
    persistMessages();
    pushActivity("info", "New conversation started");
  }

  function selectSession(id: string) {
    // Re-selecting the active session from Settings/Workspace must still
    // return to chat (early return used to trap users on those panels).
    if (id === activeSessionId && shellView === "chat") return;
    shellView = "chat";
    if (id === activeSessionId) return;
    // Persist the outgoing session so its in-flight placeholder + any
    // streamed-so-far text survives the switch. Background orchestrator
    // events for that session will continue writing into localStorage
    // via `updateMessagesForSession` so the chain-of-thought keeps
    // growing even while the user is looking at another chat.
    persistMessages();
    activeSessionId = id;
    loadMessagesForSession(id);
    persistMeta();
  }

  /** Drop any in-flight turn handle (e.g. when the user switches chats while
   * the previous one is still working). Leaves the placeholder bubble in the
   * old session so the answer can still arrive there via `turn_end`. */
  /** Tear down a single session's pending-turn bookkeeping without
   * touching its placeholder message. Used for explicit cancels and for
   * the "Stop" button path; **not** called on session switches any more
   * (the turn keeps running in the background and streaming into that
   * session's stored messages). */
  function abandonPendingTurn(sessionId: string | null, _reason: string) {
    if (!sessionId) return;
    const pt = pendingTurns.get(sessionId);
    if (!pt) return;
    if (pt.watchdog) clearTimeout(pt.watchdog);
    pendingTurns.delete(sessionId);
    pendingTurns = pendingTurns;
  }

  /** User clicked the **Stop** button. Unlike `abandonPendingTurn`, this
   * also (a) aborts the in-flight HTTP fetch via the captured
   * `AbortController`, (b) replaces the placeholder bubble with a clear
   * "Stopped" message, and (c) best-effort tells the agent to bail out
   * between LLM tool rounds via `POST /session/cancel`. The current LLM
   * HTTP call may still finish on the server side (we can't kill it
   * mid-flight), but no further rounds will run for this turn. */
  function cancelPendingTurn() {
    const sid = activeSessionId;
    if (!sid) return;
    const pt = pendingTurns.get(sid);
    if (!pt) return;
    const sessionForTurn = pt.sessionId;
    const placeholderId = pt.placeholderId;
    if (pt.abort) {
      try {
        pt.abort.abort();
      } catch {
        /* ignore */
      }
    }
    if (pt.watchdog) clearTimeout(pt.watchdog);
    pendingTurns.delete(sid);
    pendingTurns = pendingTurns;

    messages = messages.map((m) => {
      if (m.id !== placeholderId) return m;
      const finalizedSteps = (m.steps ?? []).map((s) =>
        s.pending ? { ...s, pending: false, ok: false } : s,
      );
      return {
        ...m,
        text: "_Stopped by user._",
        pending: false,
        steps: finalizedSteps,
        stepsOpen: false,
      };
    });
    persistMessages();

    pushActivity("info", "Turn cancelled by user");
    // Best-effort server-side cancel — don't await, don't surface errors.
    // If the endpoint isn't there (older agent) this is a harmless 404.
    void fetch(`${agentBase()}/session/cancel`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ session_id: sessionForTurn }),
    }).catch(() => {
      /* agent may not support cancel — UI is already unstuck */
    });
  }

  function deleteSession(id: string, ev: MouseEvent) {
    ev.stopPropagation();
    sessions = sessions.filter((s) => s.id !== id);
    try {
      localStorage.removeItem(msgsKey(id));
    } catch {
      /* ignore */
    }
    // Abandoning the pending turn for the deleted session is safe —
    // there's no placeholder left to update anyway. We do it for *that*
    // session specifically (not the active one, which may be about to
    // be reassigned below).
    abandonPendingTurn(id, "Conversation deleted");
    if (activeSessionId === id) {
      activeSessionId = null;
      messages = [];
      if (sessions[0]) selectSession(sessions[0].id);
      else void bootstrapSession();
    }
    persistMeta();
  }

  function orchOneLine(s: string, max = 280): string {
    const t = s.replace(/\r/g, "").replace(/\s+/g, " ").trim();
    return t.length > max ? `${t.slice(0, max)}…` : t;
  }

  function formatOrchestratorPayload(d: Record<string, unknown>): {
    kind: ActivityItem["kind"];
    text: string;
    coalesceKey?: string;
  } {
    const k = d.kind;
    if (k === "turn_begin") {
      const prov = String(d.provider ?? "?");
      const pref = String(d.session_prefix ?? "");
      const chars =
        typeof d.user_chars === "number" ? d.user_chars : String(d.user_chars ?? "?");
      const prev = orchOneLine(String(d.user_preview ?? ""), 200);
      return {
        kind: "info",
        text: `Turn start · ${prov} · session ${pref}… · user ${chars} chars\n  “${prev}”`,
      };
    }
    if (k === "turn_end") {
      const ms = String(d.duration_ms ?? "?");
      const ac = String(d.answer_chars ?? "?");
      const ap = orchOneLine(String(d.answer_preview ?? ""), 240);
      return {
        kind: "ok",
        text: `Turn done · ${ms} ms · reply ${ac} chars\n  ${ap}`,
      };
    }
    if (k === "turn_error") {
      const ms = String(d.duration_ms ?? "?");
      const err = orchOneLine(String(d.error ?? "unknown"), 400);
      return {
        kind: "err",
        text: `Turn error · ${ms} ms\n  ${err}`,
      };
    }
    if (k === "turn_cancel") {
      const src = String(d.source ?? "user");
      const had = d.had_pending_turn ? "in-flight" : "no-op";
      return {
        kind: "info",
        text: `Turn cancelled · ${src} · ${had}`,
      };
    }
    if (k === "tool_begin") {
      // Surfaced *immediately* when a tool starts — without this the gap
      // between the model emitting a tool_call and the tool returning
      // (which can be 100+ s for `analyze_visual` on a slow CPU) looks
      // identical to the agent being hung.
      const tool = String(d.tool ?? "?");
      const cmd = String(d.command ?? "").trim();
      const headline = cmd ? `▸ ${cmd}` : `▸ ${tool}`;
      const toolId = typeof d.tool_id === "string" ? (d.tool_id as string) : "";
      return {
        kind: "info",
        text: `${headline}\n  ${tool} · running…`,
        coalesceKey: toolId ? `tool:${toolId}` : undefined,
      };
    }
    if (k === "tool_progress") {
      const tool = String(d.tool ?? "?");
      const cmd = String(d.command ?? "").trim();
      const headline = cmd ? `▸ ${cmd}` : `▸ ${tool}`;
      const toolId = typeof d.tool_id === "string" ? (d.tool_id as string) : "";
      const elapsedMs = typeof d.elapsed_ms === "number" ? d.elapsed_ms : 0;
      const elapsedSec = Math.round(elapsedMs / 1000);
      return {
        kind: "info",
        text: `${headline}\n  ${tool} · running… ${elapsedSec}s elapsed`,
        coalesceKey: toolId ? `tool:${toolId}` : undefined,
      };
    }
    if (k === "tool") {
      const tool = String(d.tool ?? "?");
      const cmd = String(d.command ?? "").trim();
      const ms = String(d.duration_ms ?? "?");
      const ok = d.ok === true;
      const rc = String(d.result_chars ?? "?");
      const shell =
        typeof d.shell_command === "string" && d.shell_command.trim().length > 0
          ? d.shell_command.trim()
          : "";
      const argsPretty =
        typeof d.args_pretty === "string" && d.args_pretty.trim().length > 0
          ? d.args_pretty.trim()
          : orchOneLine(String(d.args_preview ?? "{}"), 720);
      const rp = String(d.result_preview ?? "");
      const headline = cmd ? `▸ ${cmd}` : `▸ ${tool}`;
      const lines = [
        headline,
        `  ${tool} · ${ms} ms · ${ok ? "OK" : "FAIL"} · ${rc} chars out`,
      ];
      if (shell) lines.push(`  $ ${shell}`);
      lines.push(`  Input:\n  ${argsPretty}`);
      lines.push(`  Output:\n  ${rp}`);
      const toolId = typeof d.tool_id === "string" ? (d.tool_id as string) : "";
      return {
        kind: ok ? "ok" : "err",
        text: lines.join("\n"),
        // Coalesce with the in-flight `tool_begin`/`tool_progress` row so
        // the user sees the live "running…" line transition cleanly into
        // the finalized "OK · 12 345 ms" line instead of getting two rows.
        coalesceKey: toolId ? `tool:${toolId}` : undefined,
      };
    }
    if (k === "image_scan_begin") {
      const n = String(d.files_total ?? "?");
      return {
        kind: "info",
        text: `Image scan · workspace · ${n} file(s) queued`,
      };
    }
    if (k === "image_scan_end") {
      const ix = String(d.indexed ?? "?");
      const mem = d.memory_reachable === true ? "memory sidecar on" : "memory sidecar off";
      return {
        kind: "ok",
        text: `Image scan · done · indexed ${ix} · ${mem}`,
      };
    }
    if (k === "background_turn") {
      const trig = String(d.trigger ?? "?");
      const sid = String(d.session_id ?? "").slice(0, 12);
      const skill = d.skill_id ? String(d.skill_id) : "";
      const path = d.path ? String(d.path) : "";
      const extra = skill || path || sid;
      return {
        kind: "info",
        text: `Background · ${trig}${extra ? ` · ${extra}` : ""}`,
      };
    }
    if (k === "llm_call_begin") {
      const prov = String(d.provider ?? "?");
      const model = String(d.model ?? "?");
      const ep = String(d.endpoint ?? "?");
      return {
        kind: "info",
        text: `API → ${prov} · ${model}\n  ${ep}`,
      };
    }
    if (k === "llm_round_begin") {
      // Per-HTTP-round signal — fires for round ≥ 2 only (round 1 is
      // already covered by `llm_call_begin`). Tells the user the model
      // has accepted the tool result and started streaming the
      // follow-up; without this the gap between a tool returning and
      // the next deltas landing (30-90 s on slow local models) looks
      // identical to the agent being hung.
      const prov = String(d.provider ?? "?");
      const model = String(d.model ?? "?");
      const round = typeof d.round === "number" ? d.round : 2;
      const ep = String(d.endpoint ?? "?");
      return {
        kind: "info",
        text: `API → ${prov} · ${model} · round ${round} (processing tool result)\n  ${ep}`,
      };
    }
    if (k === "llm_call_end") {
      const prov = String(d.provider ?? "?");
      const model = String(d.model ?? "?");
      const ep = String(d.endpoint ?? "?");
      const ms = String(d.duration_ms ?? "?");
      const ok = d.ok === true;
      if (ok) {
        return {
          kind: "ok",
          text: `API ✓ ${prov} · ${model} · ${ms} ms\n  ${ep}`,
        };
      }
      const err = orchOneLine(String(d.error ?? "unknown"), 400);
      return {
        kind: "err",
        text: `API ✗ ${prov} · ${model} · ${ms} ms\n  ${ep}\n  ${err}`,
      };
    }
    if (k === "llm_thinking_delta") {
      // Per-chunk reasoning streamed off the LLM's SSE feed. We render
      // the *full* streaming text in the inline reasoning panel (see the
      // socket.on("orchestrator") handler below); here in the Activity
      // panel we coalesce every delta for a given `call_id` into a single
      // row that updates in place — otherwise Ollama's per-token stream
      // would push a new row every ~50 ms and bury every other event.
      const prov = String(d.provider ?? "?");
      const model = String(d.model ?? "?");
      const callId = typeof d.call_id === "string" ? (d.call_id as string) : "";
      const delta = typeof d.delta === "string" ? (d.delta as string) : "";
      if (callId) {
        const prev = thinkingAccum.get(callId) ?? "";
        const next = (prev + delta).slice(-THINKING_ACCUM_MAX);
        thinkingAccum.set(callId, next);
        const tail = orchOneLine(
          next.slice(-THINKING_ACCUM_PREVIEW),
          THINKING_ACCUM_PREVIEW,
        );
        return {
          kind: "info",
          text: `Thinking · ${prov} · ${model}\n  …${tail}`,
          coalesceKey: `thinking:${callId}`,
        };
      }
      // Legacy fallback for agents that don't tag deltas with a call_id.
      return {
        kind: "info",
        text: `Thinking · ${prov} · ${model}\n  ${orchOneLine(delta, 200)}`,
      };
    }
    if (k === "llm_content_delta") {
      // Per-chunk **answer** stream. Same coalescing strategy as thinking
      // — without it Ollama's per-token output spams the Activity panel
      // and looks identical to the "stuck" state the user complained about.
      const prov = String(d.provider ?? "?");
      const model = String(d.model ?? "?");
      const callId = typeof d.call_id === "string" ? (d.call_id as string) : "";
      const delta = typeof d.delta === "string" ? (d.delta as string) : "";
      if (callId) {
        const prev = contentAccum.get(callId) ?? "";
        const next = (prev + delta).slice(-CONTENT_ACCUM_MAX);
        contentAccum.set(callId, next);
        const tail = orchOneLine(
          next.slice(-CONTENT_ACCUM_PREVIEW),
          CONTENT_ACCUM_PREVIEW,
        );
        return {
          kind: "info",
          text: `Generating · ${prov} · ${model}\n  …${tail}`,
          coalesceKey: `content:${callId}`,
        };
      }
      return {
        kind: "info",
        text: `Generating · ${prov} · ${model}\n  ${orchOneLine(delta, 200)}`,
      };
    }
    if (k === "provider_fallback") {
      const req = String(d.requested ?? "?");
      const using = String(d.using ?? "?");
      const reason = orchOneLine(String(d.reason ?? "primary unavailable"), 240);
      return {
        kind: "info",
        text: `Fallback · ${req} → ${using}\n  ${reason}`,
      };
    }
    return {
      kind: "info",
      text: `Orchestrator · ${JSON.stringify(d)}`,
    };
  }

  function pushActivity(
    kind: ActivityItem["kind"],
    text: string,
    orch = false,
    coalesceKey?: string,
  ) {
    if (coalesceKey) {
      // Look back a short window for a prior row with the same coalesce key
      // and update it in place (keeps streaming rows from spamming the
      // timeline). We bound the scan to the tail so this stays O(1) even
      // when the activity log is full.
      const scanFrom = Math.max(0, activity.length - 40);
      for (let i = activity.length - 1; i >= scanFrom; i--) {
        if (activity[i].coalesceKey === coalesceKey) {
          const updated = { ...activity[i], t: Date.now(), kind, text };
          activity = [
            ...activity.slice(0, i),
            updated,
            ...activity.slice(i + 1),
          ];
          void scrollActivityToBottom();
          return;
        }
      }
    }
    activity = [
      ...activity,
      {
        id: crypto.randomUUID(),
        t: Date.now(),
        kind,
        text,
        orch,
        coalesceKey,
      },
    ].slice(-200);
    void scrollActivityToBottom();
  }

  /** After DOM catches up with `activity`, pin the Activity list to the latest
   * row — but only if the user is already near the bottom. Manually scrolling
   * up flips `actScrollPinned` off so streaming events stop yanking the
   * viewport away. */
  async function scrollActivityToBottom(force = false) {
    if (!activityOpen) return;
    if (!force && !actScrollPinned) return;
    await tick();
    if (actListEl) {
      actListEl.scrollTop = actListEl.scrollHeight;
      if (force) actScrollPinned = true;
    }
  }

  function onActivityScroll() {
    if (!actListEl) return;
    const { scrollTop, scrollHeight, clientHeight } = actListEl;
    const nearBottom = scrollHeight - scrollTop - clientHeight < 24;
    if (actScrollPinned !== nearBottom) actScrollPinned = nearBottom;
  }

  function onThreadScroll() {
    if (!threadEl) return;
    const { scrollTop, scrollHeight, clientHeight } = threadEl;
    const next = scrollHeight - scrollTop - clientHeight < 120;
    if (scrollPinned !== next) scrollPinned = next;
  }

  /** Never use `await tick()` from `afterUpdate` — it schedules endless flush cycles and freezes the tab. */
  function scrollToBottomIfPinned() {
    if (!threadEl || !scrollPinned) return;
    threadEl.scrollTop = threadEl.scrollHeight;
  }

  let scrollRaf = 0;
  function queueThreadScroll() {
    if (scrollRaf) cancelAnimationFrame(scrollRaf);
    scrollRaf = requestAnimationFrame(() => {
      scrollRaf = 0;
      scrollToBottomIfPinned();
    });
  }

  // Scroll when thread content changes (not every DOM tick).
  $: {
    messages;
    loading;
    activeSessionId;
    queueThreadScroll();
  }

  async function bootstrapSession() {
    loadStored();
    if (activeSessionId && sessions.some((s) => s.id === activeSessionId)) {
      loadMessagesForSession(activeSessionId);
      return;
    }
    await newChat();
  }

  onMount(async () => {
    loadStored();
    if (activeSessionId && sessions.some((s) => s.id === activeSessionId)) {
      loadMessagesForSession(activeSessionId);
    }

    await runPreflight();
    if (showPreflightOverlay) {
      // Sidecars deferred by the desktop shell; nothing else to bootstrap
      // until the user resolves the hardware blocker.
      return;
    }

    try {
      const r = await fetchWithTimeout(`${agentBase()}/health`);
      agentOk = r.ok;
    } catch {
      agentOk = false;
    }

    socket = io(agentBase(), { transports: ["websocket", "polling"] });
    socket.on("connect", () => {
      socketConnected = true;
      pushActivity("ok", "Realtime connected");
    });
    socket.on("disconnect", () => {
      socketConnected = false;
      pushActivity("err", "Realtime disconnected");
    });
    socket.on("orchestrator", (d: unknown) => {
      if (!d || typeof d !== "object") return;
      const o = d as Record<string, unknown>;
      const row = formatOrchestratorPayload(o);
      pushActivity(row.kind, row.text, true, row.coalesceKey);
      // Reconcile in-flight turns. The HTTP POST is unreliable for long
      // tool runs (WebKit drops the connection at ~60 s) but the agent
      // still completes the turn and emits a `turn_end` event with the
      // full answer — that's what lets the user actually *see* the reply.
      const kind = String(o.kind ?? "");
      const sid = typeof o.session_id === "string" ? o.session_id : "";
      if (kind === "turn_end") {
        const answer = typeof o.answer === "string"
          ? o.answer
          : typeof o.answer_preview === "string"
            ? (o.answer_preview as string)
            : "";
        resolvePendingTurn(sid, answer, false);
      } else if (kind === "turn_error") {
        const err = typeof o.error === "string" ? o.error : "Unknown error";
        resolvePendingTurn(
          sid,
          `**Could not complete reply**\n\n${err}`,
          true,
        );
      } else if (kind === "turn_cancel") {
        // Defensive: the client usually finalizes the placeholder *before*
        // POSTing /session/cancel, so this is normally a no-op. If we
        // somehow still have a pending turn for this session (e.g. cancel
        // came from another window on the same session), close it cleanly.
        resolvePendingTurn(sid, "_Stopped by user._", false);
      } else if (kind === "llm_call_begin") {
        const provider = String(o.provider ?? "?");
        const model = String(o.model ?? "?");
        const endpoint = String(o.endpoint ?? "");
        const callId = typeof o.call_id === "string" ? (o.call_id as string) : undefined;
        appendStepToPending(sid, (steps) => [
          ...steps,
          {
            id: crypto.randomUUID(),
            kind: "api",
            headline: `${provider} · ${model}`,
            detail: endpoint,
            pending: true,
            callId,
          },
        ]);
      } else if (kind === "llm_round_begin") {
        // Round ≥ 2 (round 1 is suppressed agent-side because
        // `llm_call_begin` already covers it). Append a pending "api"
        // step labeled with the round number so the user can see in the
        // inline reasoning panel that the model is working on the
        // tool result. Without this row the bubble shows the tool
        // finishing and then nothing — looks indistinguishable from
        // hung on slow local models. Tagged with `callId` so the
        // existing `llm_call_end` handler clears it.
        const provider = String(o.provider ?? "?");
        const model = String(o.model ?? "?");
        const endpoint = String(o.endpoint ?? "");
        const round = typeof o.round === "number" ? (o.round as number) : 2;
        const callId = typeof o.call_id === "string" ? (o.call_id as string) : undefined;
        appendStepToPending(sid, (steps) => [
          ...steps,
          {
            id: crypto.randomUUID(),
            kind: "api",
            headline: `${provider} · ${model} · round ${round}`,
            detail: endpoint,
            pending: true,
            callId,
          },
        ]);
      } else if (kind === "llm_call_end") {
        const provider = String(o.provider ?? "?");
        const model = String(o.model ?? "?");
        const ok = o.ok === true;
        const ms = typeof o.duration_ms === "number" ? o.duration_ms : 0;
        const err = typeof o.error === "string" ? o.error : undefined;
        const callId = typeof o.call_id === "string" ? (o.call_id as string) : undefined;
        // The streamed-reasoning + answer tails are per-call; once the
        // call finishes the accumulators are dead weight (and we don't
        // want a later call on the same session to append onto a prior
        // call's thoughts/answer).
        if (callId) {
          thinkingAccum.delete(callId);
          contentAccum.delete(callId);
        }
        appendStepToPending(sid, (steps) => {
          // Match by call_id first (most precise) and fall back to the
          // most-recent in-flight api step with the same headline.
          let realIdx = -1;
          if (callId) {
            // Clear *every* pending api step for this call_id — a single
            // call can include multiple rounds (round 1 from
            // `llm_call_begin`, round ≥ 2 from `llm_round_begin`); only
            // the last one gets the OK/FAIL stamp, the rest get marked
            // done so they stop spinning.
            const updated = steps.map((s) => {
              if (s.kind === "api" && s.callId === callId && s.pending) {
                return { ...s, pending: false, ok: true };
              }
              return s;
            });
            for (let i = updated.length - 1; i >= 0; i--) {
              if (updated[i].kind === "api" && updated[i].callId === callId) {
                realIdx = i;
                break;
              }
            }
            steps = updated;
          }
          if (realIdx === -1) {
            const back = [...steps]
              .reverse()
              .findIndex(
                (s) =>
                  s.kind === "api" &&
                  s.pending === true &&
                  s.headline === `${provider} · ${model}`,
              );
            if (back !== -1) realIdx = steps.length - 1 - back;
          }
          if (realIdx === -1) {
            return [
              ...steps,
              {
                id: crypto.randomUUID(),
                kind: "api",
                headline: `${provider} · ${model}`,
                detail: err,
                pending: false,
                ok,
                durationMs: ms,
                callId,
              },
            ];
          }
          const updated = [...steps];
          updated[realIdx] = {
            ...updated[realIdx],
            pending: false,
            ok,
            durationMs: ms,
            detail: err ?? updated[realIdx].detail,
          };
          // Also stop the spinner on any in-flight `thinking` step tied
          // to this call — its deltas have stopped streaming now.
          if (callId) {
            for (let i = 0; i < updated.length; i++) {
              const s = updated[i];
              if (s.kind === "thinking" && s.callId === callId && s.pending) {
                updated[i] = { ...s, pending: false, ok: true };
              }
            }
          }
          return updated;
        });
      } else if (kind === "tool_begin") {
        // Surface the in-flight step the moment the tool starts so the
        // user can see *what* is running during the long silent gap
        // before the result lands. Without this the inline reasoning
        // panel jumps straight from "Thinking…" to a finalized tool
        // step, with nothing in between for tools like `analyze_visual`
        // that take 90+ s on slow hardware.
        const tool = String(o.tool ?? "?");
        const command = String(o.command ?? tool);
        const toolId = typeof o.tool_id === "string" ? (o.tool_id as string) : "";
        const argsPretty = typeof o.args_pretty === "string"
          ? (o.args_pretty as string)
          : typeof o.args_preview === "string"
            ? (o.args_preview as string)
            : "";
        appendStepToPending(sid, (steps) => {
          if (toolId && steps.some((s) => s.kind === "tool" && s.toolId === toolId)) {
            return steps;
          }
          return [
            ...steps,
            {
              id: crypto.randomUUID(),
              kind: "tool",
              headline: command,
              detail: tool,
              pending: true,
              input: clipPreview(argsPretty, 1200),
              toolId: toolId || undefined,
              elapsedMs: 0,
            },
          ];
        });
      } else if (kind === "tool_progress") {
        // Periodic heartbeat — bump the elapsed time on the matching
        // in-flight tool step so the user sees motion (and knows the
        // agent isn't deadlocked) while a slow tool grinds.
        const toolId = typeof o.tool_id === "string" ? (o.tool_id as string) : "";
        const elapsedMs = typeof o.elapsed_ms === "number" ? o.elapsed_ms : 0;
        if (!toolId) return;
        appendStepToPending(sid, (steps) => {
          const idx = steps.findIndex(
            (s) => s.kind === "tool" && s.toolId === toolId,
          );
          if (idx === -1) {
            const tool = String(o.tool ?? "?");
            const command = String(o.command ?? tool);
            return [
              ...steps,
              {
                id: crypto.randomUUID(),
                kind: "tool",
                headline: command,
                detail: tool,
                pending: true,
                toolId,
                elapsedMs,
              },
            ];
          }
          const updated = [...steps];
          updated[idx] = { ...updated[idx], elapsedMs };
          return updated;
        });
      } else if (kind === "tool") {
        const tool = String(o.tool ?? "?");
        const command = String(o.command ?? tool);
        const ok = o.ok === true;
        const ms = typeof o.duration_ms === "number" ? o.duration_ms : 0;
        const toolId = typeof o.tool_id === "string" ? (o.tool_id as string) : "";
        const argsPretty = typeof o.args_pretty === "string"
          ? (o.args_pretty as string)
          : typeof o.args_preview === "string"
            ? (o.args_preview as string)
            : "";
        const result = typeof o.result_preview === "string"
          ? (o.result_preview as string)
          : "";
        const shell = typeof o.shell_command === "string"
          ? (o.shell_command as string)
          : undefined;
        appendStepToPending(sid, (steps) => {
          // Replace the in-flight step (created by `tool_begin`) with
          // the finalized one so the row transitions cleanly from
          // "running… 25s" to "OK · 25 312 ms" without flashing two
          // entries side-by-side.
          const idx = toolId
            ? steps.findIndex((s) => s.kind === "tool" && s.toolId === toolId)
            : -1;
          const finalized = {
            id: idx === -1 ? crypto.randomUUID() : steps[idx].id,
            kind: "tool" as const,
            headline: command,
            detail: tool,
            pending: false,
            ok,
            durationMs: ms,
            input: clipPreview(argsPretty, 1200),
            output: clipPreview(result, 1200),
            shell: shell ? clipPreview(shell, 600) : undefined,
            toolId: toolId || undefined,
          };
          if (idx === -1) return [...steps, finalized];
          const updated = [...steps];
          updated[idx] = finalized;
          return updated;
        });
      } else if (kind === "llm_thinking_delta") {
        // Streamed reasoning chunk. Append to the existing `thinking`
        // step for this call (matched by call_id); create one if this is
        // the first delta of the call. Updating *in place* makes the
        // text grow live in the inline panel without flicker.
        const provider = String(o.provider ?? "?");
        const model = String(o.model ?? "?");
        const callId = typeof o.call_id === "string" ? (o.call_id as string) : "";
        const delta = typeof o.delta === "string" ? (o.delta as string) : "";
        if (!delta) return;
        appendStepToPending(sid, (steps) => {
          const idx = callId
            ? steps.findIndex((s) => s.kind === "thinking" && s.callId === callId)
            : -1;
          if (idx === -1) {
            return [
              ...steps,
              {
                id: crypto.randomUUID(),
                kind: "thinking",
                headline: `Thinking · ${provider} · ${model}`,
                pending: true,
                thinking: delta,
                callId: callId || undefined,
              },
            ];
          }
          const updated = [...steps];
          const existing = updated[idx];
          updated[idx] = {
            ...existing,
            thinking: (existing.thinking ?? "") + delta,
          };
          return updated;
        });
        scrollToBottomIfPinned();
      } else if (kind === "llm_content_delta") {
        // Streamed **answer** chunk (everything outside `<think>` /
        // `thinking_delta`). We append it directly to the in-flight
        // assistant placeholder so the chat bubble fills as the model
        // writes — without this the bubble stayed empty for the entire
        // answer-generation phase (often 30-90 s on a slow local Ollama
        // model after the last reasoning token), which the user
        // experienced as "Working… stuck forever".
        const delta = typeof o.delta === "string" ? (o.delta as string) : "";
        if (!delta || !sid) return;
        const pt = pendingTurns.get(sid);
        if (!pt) return;
        const placeholderId = pt.placeholderId;
        updateMessagesForSession(sid, (msgs) =>
          msgs.map((m) =>
            m.id === placeholderId
              ? { ...m, text: (m.text ?? "") + delta }
              : m,
          ),
        );
        // Keep the user pinned to the bottom *only* if they're on the
        // session currently receiving deltas — background updates must
        // not hijack the active chat's scroll position.
        if (sid === activeSessionId) scrollToBottomIfPinned();
      } else if (kind === "provider_fallback") {
        const requested = String(o.requested ?? "?");
        const using = String(o.using ?? "?");
        const reason = typeof o.reason === "string" ? (o.reason as string) : "";
        appendStepToPending(sid, (steps) => [
          ...steps,
          {
            id: crypto.randomUUID(),
            kind: "fallback",
            headline: `Fallback · ${requested} → ${using}`,
            detail: reason,
            pending: false,
            ok: true,
          },
        ]);
        // The failed attempt may have streamed partial answer text into
        // the bubble before erroring out; that text is stale and would
        // get prepended to the fallback provider's reply. Reset it so
        // the next `llm_content_delta` starts from a clean slate.
        if (sid) {
          const pt = pendingTurns.get(sid);
          if (pt) {
            const placeholderId = pt.placeholderId;
            updateMessagesForSession(sid, (msgs) =>
              msgs.map((m) =>
                m.id === placeholderId ? { ...m, text: "" } : m,
              ),
            );
          }
        }
      }
    });
    socket.on("tool_error", (d: unknown) => {
      pushActivity("err", `Tool error · ${JSON.stringify(d)}`, true);
    });
    socket.on("chart_render", (d: unknown) => {
      if (!d || typeof d !== "object") return;
      const o = d as Record<string, unknown>;
      const title = String(o.title ?? "Chart");
      const labels = Array.isArray(o.labels)
        ? (o.labels as unknown[]).map((x) => String(x))
        : [];
      const series = Array.isArray(o.series)
        ? (o.series as { name?: string; values?: number[] }[])
        : [];
      // Charts are emitted mid-turn by the `render_chart` tool; attach
      // them to the active conversation as inline assistant messages so
      // they appear in chronological order with text replies (and don't
      // pin to the bottom of the thread forever).
      const chart: ChartTile = {
        id: crypto.randomUUID(),
        title,
        chart_type: String(o.chart_type ?? "bar"),
        labels,
        series,
      };
      // Insert before the pending placeholder if one exists for the
      // **active** session, so the chart sits between the user prompt
      // and the (still-loading) reply. Charts for background-session
      // turns are no-op'd here (we'd never see them anyway) — they'd
      // require the same `updateMessagesForSession` dance as text.
      const activePt = activeSessionId
        ? pendingTurns.get(activeSessionId)
        : undefined;
      const insertIdx = activePt
        ? messages.findIndex((m) => m.id === activePt.placeholderId)
        : -1;
      const newMsg: Msg = {
        id: crypto.randomUUID(),
        role: "assistant",
        text: "",
        chart,
      };
      if (insertIdx >= 0) {
        messages = [
          ...messages.slice(0, insertIdx),
          newMsg,
          ...messages.slice(insertIdx),
        ];
      } else {
        messages = [...messages, newMsg];
      }
      persistMessages();
      pushActivity("ok", `Chart · ${title}`, true);
      void tick().then(() => queueThreadScroll());
    });

    await loadProviderDefaultsFromServer();
    // Probe providers BEFORE creating a session so we can pick a reachable
    // one if the user's persisted choice (e.g. a LAN Ollama box) is dead.
    await loadProviderAvailability();
    await loadOllamaInstallStatus();
    await bootstrapSession();
    await loadBackgroundSettings();

    void refreshSystemStats();
    systemStatsPoll = setInterval(() => void refreshSystemStats(), 4000);
  });

  onDestroy(() => {
    if (scrollRaf) cancelAnimationFrame(scrollRaf);
    if (systemStatsPoll) clearInterval(systemStatsPoll);
    stopOllamaPoll();
    persistMessages();
    socket?.disconnect();
  });

  function bumpSessionTitle(userText: string) {
    if (!activeSessionId) return;
    const title =
      userText.slice(0, 48).trim() + (userText.length > 48 ? "…" : "") ||
      "Chat";
    sessions = sessions.map((s) =>
      s.id === activeSessionId ? { ...s, title, updated: Date.now() } : s,
    );
    sessions = [...sessions].sort((a, b) => b.updated - a.updated);
    persistMeta();
  }

  /** Append (or update) a reasoning step on the assistant placeholder
   * matching the in-flight turn. No-ops once the turn has finished, so a
   * straggling event after `turn_end` doesn't mutate a closed message.
   * Session-aware: writes into the correct chat even if the user has
   * switched away to another session mid-turn. */
  function appendStepToPending(
    sessionId: string,
    update: (steps: ReasoningStep[]) => ReasoningStep[],
  ) {
    if (!sessionId) return;
    const pt = pendingTurns.get(sessionId);
    if (!pt) return;
    const placeholderId = pt.placeholderId;
    updateMessagesForSession(sessionId, (msgs) =>
      msgs.map((m) => {
        if (m.id !== placeholderId) return m;
        // Background-session placeholders have `pending` stripped on
        // persist; we treat them as still-pending as long as the
        // matching entry is in `pendingTurns`. For active-session
        // placeholders the flag is preserved in memory.
        const next = update(m.steps ?? []);
        return { ...m, steps: next, stepsOpen: true };
      }),
    );
  }

  /** Trim long tool I/O previews so the chat thread stays light. The full
   * payload still lives in the Activity panel for power users. */
  function clipPreview(s: string, max = 600): string {
    if (!s) return "";
    if (s.length <= max) return s;
    return `${s.slice(0, max)}\n… (${s.length - max} more chars)`;
  }

  /** Reasoning panel chevron toggle. Updates the message in place and
   * persists so refreshing the app doesn't lose the user's expand state. */
  function toggleStepsOpen(msgId: string) {
    messages = messages.map((m) =>
      m.id === msgId ? { ...m, stepsOpen: !m.stepsOpen } : m,
    );
    persistMessages();
  }

  /** Per-step details (input/output/shell) collapse state lives in a Set
   * keyed by step id so it doesn't pollute the persisted message shape. */
  let openStepDetails: Set<string> = new Set();
  function toggleStepDetail(stepId: string) {
    const next = new Set(openStepDetails);
    if (next.has(stepId)) next.delete(stepId);
    else next.add(stepId);
    openStepDetails = next;
  }

  /** Aggregate duration for the "Reasoning · Ns" header chip. */
  function stepsTotalMs(steps: ReasoningStep[] | undefined): number {
    if (!steps) return 0;
    return steps.reduce((acc, s) => acc + (s.durationMs ?? 0), 0);
  }
  function fmtMs(ms: number): string {
    if (ms < 1000) return `${ms} ms`;
    if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
    const m = Math.floor(ms / 60_000);
    const s = Math.round((ms % 60_000) / 1000);
    return `${m}m ${s}s`;
  }
  function stepIcon(kind: ReasoningStep["kind"]): string {
    if (kind === "api") return "↗";
    if (kind === "tool") return "▸";
    if (kind === "thinking") return "✻";
    return "↪";
  }

  /** Fill the in-flight assistant placeholder created by `send()` with the
   * final reply (or an error message). Idempotent — only the first
   * resolution wins, so a slow HTTP response that arrives after the
   * orchestrator has already populated the bubble becomes a no-op. */
  function resolvePendingTurn(sessionId: string, text: string, isError: boolean) {
    if (!sessionId) return;
    const pt = pendingTurns.get(sessionId);
    if (!pt) return;
    const placeholderId = pt.placeholderId;
    if (pt.watchdog) clearTimeout(pt.watchdog);
    pendingTurns.delete(sessionId);
    // Reassigning the same Map reference is how we tell Svelte 4 its
    // contents changed (for the `$: loading = …` derived store).
    pendingTurns = pendingTurns;
    updateMessagesForSession(sessionId, (msgs) =>
      msgs.map((m) => {
        if (m.id !== placeholderId) return m;
        // Mark any still-pending in-flight steps as failed so the
        // spinner stops; auto-collapse the trail once the turn settles
        // (the user can re-open it via the chevron if they want to
        // inspect the steps).
        const finalizedSteps = (m.steps ?? []).map((s) =>
          s.pending ? { ...s, pending: false, ok: !isError } : s,
        );
        return {
          ...m,
          text,
          pending: false,
          steps: finalizedSteps,
          stepsOpen: false,
        };
      }),
    );
  }

  async function send() {
    const text = input.trim();
    if (!text || !activeSessionId || loading) return;
    input = "";
    scrollPinned = true;
    const userId = crypto.randomUUID();
    const placeholderId = crypto.randomUUID();
    const sessionForTurn = activeSessionId;
    messages = [
      ...messages,
      { id: userId, role: "user", text },
      { id: placeholderId, role: "assistant", text: "", pending: true },
    ];
    bumpSessionTitle(text);
    persistMessages();

    // Defensive: if a prior turn for *this same* session is still
    // tracked (it shouldn't be — `loading` would have blocked us above
    // — but belt-and-suspenders) clear it so we don't leak a watchdog.
    const existing = pendingTurns.get(sessionForTurn);
    if (existing?.watchdog) clearTimeout(existing.watchdog);

    const abort = new AbortController();
    const abortSignal = abort.signal;
    pendingTurns.set(sessionForTurn, {
      sessionId: sessionForTurn,
      placeholderId,
      startedAt: Date.now(),
      watchdog: setTimeout(() => {
        const minutes = Math.round(pendingTurnTimeoutMs / 60_000);
        resolvePendingTurn(
          sessionForTurn,
          `**The agent did not respond in time.**\n\nNo \`turn_end\` event arrived within ${minutes} minute${minutes === 1 ? "" : "s"}. Bump the watchdog under Settings → AI Provider → Advanced if you're running long local-model turns.`,
          true,
        );
      }, pendingTurnTimeoutMs),
      abort,
    });
    // Trigger the `$: loading = pendingTurns.has(activeSessionId)` store.
    pendingTurns = pendingTurns;

    const sessionMessageUrl = `${agentBase()}/session/message`;
    pushActivity("info", `POST ${sessionMessageUrl} · provider=${aiProvider}`);
    try {
      const res = await fetchLongLived(sessionMessageUrl, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          session_id: sessionForTurn,
          message: text,
          // Send the current primary on every turn so flipping it in
          // Settings (without starting a new chat) actually changes which
          // provider the agent calls. The agent updates the session's
          // pinned `provider` from this field before executing.
          provider: aiProvider,
          settings: providerSettingsBody(),
        }),
        signal: abortSignal,
      });
      const j = (await res.json()) as {
        answer?: string;
        error?: string;
        session_id?: string;
      };
      if (!res.ok) {
        if (res.status === 404) {
          const newId = await startServerSession();
          if (newId && activeSessionId === sessionForTurn) {
            const oldId = sessionForTurn;
            activeSessionId = newId;
            sessions = sessions.map((s) =>
              s.id === oldId ? { ...s, id: newId, provider: aiProvider } : s,
            );
            try {
              localStorage.setItem(msgsKey(newId), JSON.stringify(messages));
              localStorage.removeItem(msgsKey(oldId));
            } catch {
              /* ignore */
            }
            persistMeta();
            // Re-target the in-flight handle to the new session id so a
            // late-arriving `turn_end` for the recreated session still
            // resolves the same placeholder bubble. We move the entry
            // in `pendingTurns` from the old id to the new id.
            const movedPt = pendingTurns.get(oldId);
            if (movedPt) {
              pendingTurns.delete(oldId);
              pendingTurns.set(newId, { ...movedPt, sessionId: newId });
              pendingTurns = pendingTurns;
            }
            const retry = await fetchLongLived(`${agentBase()}/session/message`, {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({
                session_id: newId,
                message: text,
                provider: aiProvider,
                settings: providerSettingsBody(),
              }),
              signal: abortSignal,
            });
            const rj = (await retry.json()) as { answer?: string; error?: string };
            if (retry.ok) {
              resolvePendingTurn(newId, rj.answer ?? "", false);
              pushActivity("ok", "Session restored after server restart");
              return;
            }
          }
        }
        const errText = j.error ?? `Request failed (${res.status})`;
        if (res.status === 404) pushActivity("err", "Session expired on server");
        resolvePendingTurn(
          sessionForTurn,
          `**Could not complete reply**\n\n${errText}`,
          true,
        );
        return;
      }
      resolvePendingTurn(sessionForTurn, j.answer ?? "", false);
    } catch (e) {
      // User-initiated abort — `cancelPendingTurn` already finalized the
      // placeholder + activity log. Nothing else to do here.
      if (
        (e instanceof DOMException && e.name === "AbortError") ||
        (e instanceof Error && e.name === "AbortError")
      ) {
        return;
      }
      // We now route this POST through `@tauri-apps/plugin-http` (Rust
      // `reqwest`), which doesn't share WebKit's ~60 s
      // `URLSession.timeoutIntervalForRequest` cap, so this branch is
      // rare in production. The orchestrator is also detached into a
      // `tokio::spawn` agent-side, so even when transport does die the
      // turn keeps running and resolves via the `turn_end` socket
      // event. We keep the placeholder and let the watchdog be the
      // only timeout we trust at the chat level.
      const detail = e instanceof Error ? e.message : String(e);
      const livePt = pendingTurns.get(sessionForTurn);
      const stillPending =
        livePt != null && livePt.placeholderId === placeholderId;
      if (stillPending) {
        pushActivity(
          "info",
          `Slow turn · HTTP dropped (${detail}); waiting for orchestrator turn_end…`,
        );
      } else {
        pushActivity("err", `Network error · ${sessionMessageUrl}\n  ${detail}`);
      }
    }
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void send();
    }
  }

  function formatTime(ts: number): string {
    return new Date(ts).toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  }
</script>

{#if showPreflightOverlay}
  <div
    class="preflight-overlay"
    role="alertdialog"
    aria-modal="true"
    aria-labelledby="preflight-title"
  >
    <div class="preflight-card">
      <header class="preflight-head">
        <img class="preflight-logo" src="/logo.png" alt="" width="48" height="48" />
        <div>
          <h1 id="preflight-title" class="preflight-title">
            {#if !preflightChecked}
              Checking your machine…
            {:else if !preflight}
              Hardware check unavailable
            {:else}
              This {preflight.requirements.platform_label} doesn't meet Eson's minimum requirements
            {/if}
          </h1>
          <p class="preflight-sub">
            Eson runs a local agent + memory service per chat, embeds the
            persona context every turn, and processes files in the background.
            We require a baseline so it stays responsive on your machine.
          </p>
        </div>
      </header>

      {#if preflight}
        <table class="preflight-table">
          <thead>
            <tr>
              <th>Resource</th>
              <th>Required</th>
              <th>Detected</th>
              <th>Status</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td>Memory</td>
              <td>≥ {preflight.requirements.min_memory_gib} GiB</td>
              <td>{fmtGib(preflight.memory_total_gib)}</td>
              <td>
                {#if preflight.memory_total_gib >= preflight.requirements.min_memory_gib}
                  <span class="preflight-ok">OK</span>
                {:else}
                  <span class="preflight-fail">Below minimum</span>
                {/if}
              </td>
            </tr>
            <tr>
              <td>Logical CPUs</td>
              <td>≥ {preflight.requirements.min_cpu_logical}</td>
              <td>
                {preflight.cpu_logical}{#if preflight.cpu_physical}
                  ({preflight.cpu_physical} physical){/if}
              </td>
              <td>
                {#if preflight.cpu_logical >= preflight.requirements.min_cpu_logical}
                  <span class="preflight-ok">OK</span>
                {:else}
                  <span class="preflight-fail">Below minimum</span>
                {/if}
              </td>
            </tr>
            {#if preflight.requirements.require_apple_silicon}
              <tr>
                <td>Chip</td>
                <td>Apple Silicon (M1 or newer)</td>
                <td>{preflight.is_apple_silicon ? "Apple Silicon" : `Intel (${preflight.arch})`}</td>
                <td>
                  {#if preflight.is_apple_silicon}
                    <span class="preflight-ok">OK</span>
                  {:else}
                    <span class="preflight-fail">Unsupported</span>
                  {/if}
                </td>
              </tr>
            {/if}
            <tr>
              <td>Host</td>
              <td>—</td>
              <td colspan="2">
                {preflight.os_name ?? preflight.requirements.platform_label}
                {preflight.os_version ?? ""} · {preflight.arch}
                {#if preflight.host_name}
                  · {preflight.host_name}{/if}
              </td>
            </tr>
          </tbody>
        </table>
      {:else if preflightChecked}
        <p class="preflight-p">
          The desktop shell couldn't query system info (running outside the
          installed app?). You can continue, but local services may not start
          automatically.
        </p>
      {/if}

      {#if preflightError}
        <p class="preflight-err">{preflightError}</p>
      {/if}

      <footer class="preflight-actions">
        {#if preflightChecked && preflight && !preflight.meets_requirements}
          <button
            type="button"
            class="preflight-btn preflight-btn-secondary"
            on:click={preflightContinueAnyway}
          >
            Continue anyway
          </button>
        {/if}
        {#if preflightChecked}
          <button
            type="button"
            class="preflight-btn"
            on:click={runPreflight}
          >
            Re-check
          </button>
        {/if}
        <a
          class="preflight-btn preflight-btn-link"
          href="https://www.apple.com/shop/buy-mac"
          target="_blank"
          rel="noopener noreferrer"
        >
          See compatible Macs
        </a>
      </footer>
    </div>
  </div>
{/if}

<div class="shell" class:sidebar-collapsed={sidebarCollapsed}>
  <aside
    class="sidebar"
    class:collapsed={sidebarCollapsed}
    aria-label="Workspace"
  >
    <div class="sb-brand">
      <div class="sb-brand-row">
        <img class="sb-logo-img" src="/logo.png" alt="" width="36" height="36" />
        <div class="sb-brand-text">
          <span class="sb-logo">Eson</span>
          <span class="sb-tag">An AI Employee</span>
        </div>
        <button
          type="button"
          class="sb-collapse-btn"
          aria-label={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
          aria-expanded={!sidebarCollapsed}
          title={sidebarCollapsed ? "Expand sidebar" : "Collapse sidebar"}
          on:click={toggleSidebar}
        >
          <svg class="sb-svg" viewBox="0 0 24 24" aria-hidden="true">
            <path
              fill="none"
              stroke="currentColor"
              stroke-width="1.6"
              stroke-linecap="round"
              stroke-linejoin="round"
              d="M4 5h16a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1Zm5 0v14"
            />
          </svg>
        </button>
      </div>
      <div class="sb-status" aria-label="Connection status">
        <span class="pill" class:ok={agentOk} class:bad={!agentOk}>
          {agentOk ? "Agent" : "Offline"}
        </span>
        <span class="pill" class:ok={socketConnected} class:bad={!socketConnected}>
          {socketConnected ? "Live" : "Socket"}
        </span>
      </div>
    </div>

    <nav class="sb-nav" aria-label="Main navigation">
      <button
        type="button"
        class="sb-nav-btn"
        class:active={shellView === "chat"}
        aria-label="Chat"
        title="Chat"
        on:click={() => (shellView = "chat")}
      >
        <svg class="sb-svg" viewBox="0 0 24 24" aria-hidden="true">
          <path
            fill="currentColor"
            d="M20 2H4c-1.1 0-2 .9-2 2v18l4-4h14c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2m0 14H5.17L4 17.17V4h16v12Z"
          />
        </svg>
        <span class="sb-nav-label">Chat</span>
      </button>
      <button
        type="button"
        class="sb-nav-btn"
        class:active={shellView === "workspace"}
        aria-label="Workspace files"
        title="Workspace"
        on:click={() => toggleShellView("workspace")}
      >
        <svg class="sb-svg" viewBox="0 0 24 24" aria-hidden="true">
          <path
            fill="currentColor"
            d="M10 4H4c-1.1 0-2 .9-2 2v12c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V8c0-1.1-.9-2-2-2h-8l-2-2Z"
          />
        </svg>
        <span class="sb-nav-label">Workspace</span>
      </button>
      <button
        type="button"
        class="sb-nav-btn"
        class:active={shellView === "settings"}
        aria-label="Settings"
        title="Settings"
        on:click={() => toggleShellView("settings")}
      >
        <svg class="sb-svg" viewBox="0 0 24 24" aria-hidden="true">
          <path
            fill="currentColor"
            d="M12 15.5A3.5 3.5 0 0 1 8.5 12 3.5 3.5 0 0 1 12 8.5a3.5 3.5 0 0 1 3.5 3.5 3.5 3.5 0 0 1-3.5 3.5m7.43-2.53c.04-.32.07-.64.07-.97 0-.33-.03-.66-.07-1l2.11-1.63c.19-.15.24-.42.12-.64l-2-3.46c-.12-.22-.39-.31-.61-.22l-2.49 1c-.52-.4-1.06-.73-1.69-.98l-.37-2.65A.506.506 0 0 0 14 2h-4c-.25 0-.46.18-.5.42l-.37 2.65c-.63.25-1.17.59-1.69.98l-2.49-1c-.23-.09-.5 0-.61.22l-2 3.46c-.13.22-.07.49.12.64L4.57 11c-.04.34-.07.67-.07 1 0 .33.03.65.07.97l-2.11 1.66c-.19.15-.24.42-.12.64l2 3.46c.12.22.39.3.61.22l2.49-1.01c.52.4 1.06.74 1.69.99l.37 2.65c.04.24.25.42.5.42h4c.25 0 .46-.18.5-.42l.37-2.65c.63-.26 1.17-.59 1.69-.99l2.49 1.01c.22.08.49 0 .61-.22l2-3.46c.12-.22.07-.49-.12-.64l-2.11-1.66Z"
          />
        </svg>
        <span class="sb-nav-label">Settings</span>
      </button>
    </nav>

    <button
      type="button"
      class="sb-new"
      title="New chat"
      aria-label="New chat"
      on:click={() => void newChat()}
    >
      <span class="sb-new-icon" aria-hidden="true">+</span>
      <span class="sb-new-text">New chat</span>
    </button>

    <nav class="sb-sessions" aria-label="Conversations">
      {#each sessions as s (s.id)}
        <div
          class="sb-row"
          class:active={s.id === activeSessionId}
          role="button"
          tabindex="0"
          on:click={() => selectSession(s.id)}
          on:keydown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              selectSession(s.id);
            }
          }}
        >
          <div class="sb-row-title">{s.title}</div>
          <div class="sb-row-meta">{formatTime(s.updated)}</div>
          <button
            type="button"
            class="sb-row-del"
            aria-label="Delete chat"
            on:click={(e) => deleteSession(s.id, e)}
          >
            ×
          </button>
        </div>
      {/each}
    </nav>

    <div class="sb-footer">
      <button
        type="button"
        class="sb-link"
        on:click={() => (voiceOpen = !voiceOpen)}
      >
        {voiceOpen ? "Hide voice" : "Voice"}
      </button>
      <button
        type="button"
        class="sb-link"
        on:click={() => {
          activityOpen = !activityOpen;
          if (activityOpen) void scrollActivityToBottom(true);
        }}
      >
        {activityOpen ? "Hide activity" : "Activity"}
      </button>
    </div>
  </aside>

  <section class="main">
    {#if shellView === "chat"}
      <header class="chat-header">
        <div>
          <h1 class="chat-title">Chat</h1>
          <p
            class="chat-sys-stats"
            title="Host machine running eson-agent (GET /system/health)"
          >
            {#if !agentOk}
              <span class="chat-sys-muted"
                >Host CPU &amp; RAM when the agent is reachable.</span
              >
            {:else if systemStatsText}
              {systemStatsText}
            {:else if !systemStatsReady}
              <span class="chat-sys-muted">Loading system metrics…</span>
            {:else}
              <span class="chat-sys-muted">System metrics unavailable</span>
            {/if}
          </p>
          {#if !agentOk}
            <p class="chat-warn">
              Agent unreachable — start <code>eson-memory</code> then
              <code>eson-agent</code> from the <code>eson/</code> directory.
            </p>
          {/if}
        </div>
      </header>

      <div
        class="chat-body"
        bind:this={threadEl}
        on:scroll={onThreadScroll}
        role="log"
        aria-live="polite"
      >
        {#if messages.length === 0 && !loading}
          <div class="empty">
            <div class="empty-icon" aria-hidden="true">◇</div>
            <h2>Start a conversation</h2>
            <p>
              Eson, here. What can I do for you?
            </p>
          </div>
        {/if}

        <div class="chat-stream">
          {#each messages as m (m.id)}
            <article
              class="msg"
              class:user={m.role === "user"}
              class:assistant={m.role === "assistant"}
              class:chart-msg={m.chart != null}
            >
              <div class="msg-label">
                {m.chart ? "Chart" : m.role === "user" ? "You" : "Eson"}
              </div>
              {#if m.chart}
                <div class="msg-body chart-wrap">
                  <canvas use:chartCanvas={m.chart} width="520" height="280"></canvas>
                </div>
              {:else}
                {#if m.role === "assistant" && m.steps && m.steps.length > 0}
                  <div class="reasoning" class:open={m.stepsOpen}>
                    <button
                      type="button"
                      class="reasoning-toggle"
                      on:click={() => toggleStepsOpen(m.id)}
                      aria-expanded={m.stepsOpen ? "true" : "false"}
                    >
                      <span class="reasoning-chevron">{m.stepsOpen ? "▾" : "▸"}</span>
                      <span class="reasoning-title">
                        {m.pending ? "Reasoning…" : "Reasoning"}
                      </span>
                      <span class="reasoning-meta">
                        {m.steps.length} step{m.steps.length === 1 ? "" : "s"}
                        {#if stepsTotalMs(m.steps) > 0}
                          · {fmtMs(stepsTotalMs(m.steps))}
                        {/if}
                      </span>
                    </button>
                    {#if m.stepsOpen}
                      <ol class="reasoning-list">
                        {#each m.steps as step (step.id)}
                          <li
                            class="reasoning-step"
                            class:step-api={step.kind === "api"}
                            class:step-tool={step.kind === "tool"}
                            class:step-fallback={step.kind === "fallback"}
                            class:step-thinking={step.kind === "thinking"}
                            class:step-pending={step.pending}
                            class:step-fail={step.ok === false}
                          >
                            <div class="reasoning-row">
                              <span class="reasoning-icon" aria-hidden="true">
                                {step.pending ? "⏳" : stepIcon(step.kind)}
                              </span>
                              <span class="reasoning-headline">{step.headline}</span>
                              {#if step.durationMs}
                                <span class="reasoning-dur">{fmtMs(step.durationMs)}</span>
                              {:else if step.pending && step.kind === "tool" && (step.elapsedMs ?? 0) > 0}
                                <span class="reasoning-dur">running… {Math.round((step.elapsedMs ?? 0) / 1000)}s</span>
                              {/if}
                              {#if step.ok === false}
                                <span class="reasoning-badge fail">FAIL</span>
                              {:else if !step.pending && step.kind !== "fallback"}
                                <span class="reasoning-badge ok">OK</span>
                              {/if}
                            </div>
                            {#if step.detail}
                              <div class="reasoning-detail">{step.detail}</div>
                            {/if}
                            {#if step.thinking}
                              <div class="reasoning-thinking">{step.thinking}</div>
                            {/if}
                            {#if step.input || step.output || step.shell}
                              <button
                                type="button"
                                class="reasoning-detail-toggle"
                                on:click={() => toggleStepDetail(step.id)}
                              >
                                {openStepDetails.has(step.id) ? "Hide" : "Show"} details
                              </button>
                              {#if openStepDetails.has(step.id)}
                                <div class="reasoning-payload">
                                  {#if step.shell}
                                    <div class="reasoning-payload-section">
                                      <div class="reasoning-payload-label">$ shell</div>
                                      <pre class="reasoning-pre">{step.shell}</pre>
                                    </div>
                                  {/if}
                                  {#if step.input}
                                    <div class="reasoning-payload-section">
                                      <div class="reasoning-payload-label">input</div>
                                      <pre class="reasoning-pre">{step.input}</pre>
                                    </div>
                                  {/if}
                                  {#if step.output}
                                    <div class="reasoning-payload-section">
                                      <div class="reasoning-payload-label">output</div>
                                      <pre class="reasoning-pre">{step.output}</pre>
                                    </div>
                                  {/if}
                                </div>
                              {/if}
                            {/if}
                          </li>
                        {/each}
                      </ol>
                    {/if}
                  </div>
                {/if}
                {#if m.pending && (!m.steps || m.steps.length === 0)}
                  <div class="msg-body thinking-inline" aria-busy="true">
                    <span class="thinking-dots"></span>
                    <span>Thinking…</span>
                  </div>
                {:else if m.role === "assistant"}
                  {#if m.text}
                    <div class="msg-body markdown">{@html renderMd(m.text)}</div>
                  {:else if m.pending}
                    <div class="msg-body thinking-inline" aria-busy="true">
                      <span class="thinking-dots"></span>
                      <span>Working…</span>
                    </div>
                  {/if}
                {:else}
                  <div class="msg-body user-txt">{m.text}</div>
                {/if}
              {/if}
            </article>
          {/each}
        </div>
      </div>

      {#if voiceOpen}
        <div class="voice-drawer" role="region" aria-label="Voice">
          <p class="voice-title">Voice</p>
          <p class="voice-muted">
            Shell for future STT/TTS — same session as chat. See
            <code>docs/INTERACTION_ROADMAP.md</code>.
          </p>
        </div>
      {/if}

      <div class="composer">
        {#if providerSwitchNotice}
          <div class="provider-switch-notice" role="status">
            <span>{providerSwitchNotice}</span>
            <button
              type="button"
              class="dismiss"
              aria-label="Dismiss"
              on:click={() => (providerSwitchNotice = null)}>×</button
            >
          </div>
        {/if}
        <div class="composer-inner">
          <textarea
            rows="1"
            bind:value={input}
            on:keydown={onKey}
            placeholder={loading
              ? "Working… click Stop to interrupt, or compose your next message."
              : "Message Eson… (Enter send · Shift+Enter newline)"}
            disabled={!activeSessionId}
            aria-label="Message"
          />
          {#if loading}
            <button
              type="button"
              class="send-btn stop-btn"
              on:click={cancelPendingTurn}
              title="Stop the in-flight turn (the agent's current LLM call may still finish in the background)"
              aria-label="Stop"
            >
              Stop
            </button>
          {:else}
            <button
              type="button"
              class="send-btn"
              disabled={!activeSessionId || !input.trim()}
              on:click={() => void send()}
            >
              Send
            </button>
          {/if}
        </div>
      </div>
    {:else if shellView === "settings"}
      <header class="chat-header">
        <h1 class="chat-title">Settings</h1>
        <p class="chat-sub">Appearance and preferences</p>
      </header>
      <div class="panel-scroll">
        <div class="panel-section">
          <h2 class="panel-h">AI Provider</h2>
          <p class="panel-p">
            Model and keys are stored on this device and sent to the agent on each
            message (and when starting a new chat). You can also click Save to
            confirm.
          </p>
          <div class="primary-summary">
            <span class="primary-summary-label">Primary:</span>
            <strong>{labelFor(aiProvider)}</strong>
            <span class="primary-summary-note">
              Falls back in order to the other configured providers if this one
              is unreachable or rate-limited.
            </span>
          </div>
          <div class="theme-toggle">
            <button
              type="button"
              class="theme-btn"
              class:selected={editingProvider === "anthropic"}
              class:provider-primary={aiProvider === "anthropic"}
              class:provider-unconfigured={!providerAvailability.anthropic}
              title={providerAvailability.anthropic ? "" : "No API key yet — pick this and add one below"}
              on:click={() => (editingProvider = "anthropic")}
            >
              Anthropic
              {#if aiProvider === "anthropic"}
                <span class="primary-pill" aria-label="primary">★ primary</span>
              {/if}
            </button>
            <button
              type="button"
              class="theme-btn"
              class:selected={editingProvider === "openai"}
              class:provider-primary={aiProvider === "openai"}
              class:provider-unconfigured={!providerAvailability.openai}
              title={providerAvailability.openai ? "" : "No API key yet — pick this and add one below"}
              on:click={() => (editingProvider = "openai")}
            >
              OpenAI
              {#if aiProvider === "openai"}
                <span class="primary-pill" aria-label="primary">★ primary</span>
              {/if}
            </button>
            <button
              type="button"
              class="theme-btn"
              class:selected={editingProvider === "ollama"}
              class:provider-primary={aiProvider === "ollama"}
              class:provider-unconfigured={!providerAvailability.ollama}
              title={providerAvailability.ollama ? "" : "No URL yet — pick this and add one below"}
              on:click={() => (editingProvider = "ollama")}
            >
              Ollama
              {#if aiProvider === "ollama"}
                <span class="primary-pill" aria-label="primary">★ primary</span>
              {/if}
            </button>
          </div>
          {#if !providerAvailability[editingProvider]}
            <div class="provider-hint" role="note">
              {#if editingProvider === "anthropic"}
                No Anthropic key detected. Paste your <code>sk-ant-…</code> key
                below and click <strong>Save</strong> — it's stored on this
                device and sent with every message.
              {:else if editingProvider === "openai"}
                No OpenAI key detected. Paste your <code>sk-…</code> key below
                and click <strong>Save</strong> — it's stored on this device
                and sent with every message.
              {:else}
                No reachable Ollama URL detected. Set the URL (default
                <code>http://127.0.0.1:11434</code>) and model below, then
                click <strong>Save</strong>.
              {/if}
            </div>
          {/if}
          {#if editingProvider === "anthropic"}
            <div class="provider-fields">
              <label class="field-label" for="anthropic-model">Model</label>
              <input
                id="anthropic-model"
                class="field-input"
                bind:value={anthropicModel}
                placeholder={providerDefaults.anthropicModel || "claude-haiku-4-5-20251001"}
              />
              {#if providerDefaults.anthropicModel && providerDefaults.anthropicModel !== anthropicModel}
                <div class="field-default">
                  Default from <code>secrets.env</code>: <code>{providerDefaults.anthropicModel}</code>
                  <button type="button" class="field-default-link" on:click={() => (anthropicModel = providerDefaults.anthropicModel)}>Use</button>
                </div>
              {/if}
              <label class="field-label" for="anthropic-key">API key</label>
              <input
                id="anthropic-key"
                class="field-input"
                type="password"
                bind:value={anthropicApiKey}
                placeholder={providerDefaults.anthropicConfigured ? "•••••• (from secrets.env)" : "sk-ant-..."}
              />
              <div class="field-default">
                {#if providerDefaults.anthropicApiKey && providerDefaults.anthropicApiKey !== anthropicApiKey}
                  Default from <code>secrets.env</code>: <code>{maskKey(providerDefaults.anthropicApiKey)}</code>
                  <button type="button" class="field-default-link" on:click={() => (anthropicApiKey = providerDefaults.anthropicApiKey)}>Use</button>
                {:else if providerDefaults.anthropicConfigured && !anthropicApiKey}
                  Using key from <code>secrets.env</code>.
                {/if}
              </div>
              <label class="primary-toggle" for="anthropic-primary">
                <input
                  id="anthropic-primary"
                  type="checkbox"
                  checked={aiProvider === "anthropic"}
                  on:change={(e) => togglePrimary("anthropic", e.currentTarget)}
                />
                <span>
                  <strong>Set as primary</strong> — chat uses Anthropic first,
                  then falls back through the other configured providers.
                </span>
              </label>
            </div>
          {:else if editingProvider === "openai"}
            <div class="provider-fields">
              <label class="field-label" for="openai-model">Model</label>
              <input
                id="openai-model"
                class="field-input"
                bind:value={openaiModel}
                placeholder={providerDefaults.openaiModel || "gpt-4o-mini"}
              />
              {#if providerDefaults.openaiModel && providerDefaults.openaiModel !== openaiModel}
                <div class="field-default">
                  Default from <code>secrets.env</code>: <code>{providerDefaults.openaiModel}</code>
                  <button type="button" class="field-default-link" on:click={() => (openaiModel = providerDefaults.openaiModel)}>Use</button>
                </div>
              {/if}
              <label class="field-label" for="openai-key">API key</label>
              <input
                id="openai-key"
                class="field-input"
                type="password"
                bind:value={openaiApiKey}
                placeholder={providerDefaults.openaiConfigured ? "•••••• (from secrets.env)" : "sk-..."}
              />
              <div class="field-default">
                {#if providerDefaults.openaiApiKey && providerDefaults.openaiApiKey !== openaiApiKey}
                  Default from <code>secrets.env</code>: <code>{maskKey(providerDefaults.openaiApiKey)}</code>
                  <button type="button" class="field-default-link" on:click={() => (openaiApiKey = providerDefaults.openaiApiKey)}>Use</button>
                {:else if providerDefaults.openaiConfigured && !openaiApiKey}
                  Using key from <code>secrets.env</code>.
                {/if}
              </div>
              <label class="primary-toggle" for="openai-primary">
                <input
                  id="openai-primary"
                  type="checkbox"
                  checked={aiProvider === "openai"}
                  on:change={(e) => togglePrimary("openai", e.currentTarget)}
                />
                <span>
                  <strong>Set as primary</strong> — chat uses OpenAI first,
                  then falls back through the other configured providers.
                </span>
              </label>
            </div>
          {:else}
            <div class="provider-fields">
              <label class="field-label" for="ollama-url">URL</label>
              <input
                id="ollama-url"
                class="field-input"
                bind:value={ollamaUrl}
                placeholder={providerDefaults.ollamaUrl || "http://127.0.0.1:11434"}
              />
              {#if providerDefaults.ollamaUrl && providerDefaults.ollamaUrl !== ollamaUrl}
                <div class="field-default">
                  Default from <code>secrets.env</code>: <code>{providerDefaults.ollamaUrl}</code>
                  <button type="button" class="field-default-link" on:click={() => (ollamaUrl = providerDefaults.ollamaUrl)}>Use</button>
                </div>
              {/if}
              <label class="field-label" for="ollama-model">Model</label>
              <input
                id="ollama-model"
                class="field-input"
                bind:value={ollamaModel}
                placeholder={providerDefaults.ollamaModel || "gemma4:e4b"}
              />
              {#if providerDefaults.ollamaModel && providerDefaults.ollamaModel !== ollamaModel}
                <div class="field-default">
                  Default from <code>secrets.env</code>: <code>{providerDefaults.ollamaModel}</code>
                  <button type="button" class="field-default-link" on:click={() => (ollamaModel = providerDefaults.ollamaModel)}>Use</button>
                </div>
              {/if}
              {#if !ollamaInstall.installed}
                <div class="provider-save-row ollama-install-row">
                  <button
                    type="button"
                    class="send-btn provider-save-btn"
                    disabled={ollamaInstallBusy || ollamaInstall.in_progress}
                    on:click={() => void installOllamaFromSettings()}
                  >
                    {#if ollamaInstallBusy || ollamaInstall.in_progress}
                      Installing…
                    {:else}
                      Install Ollama + gemma4:e4b
                    {/if}
                  </button>
                  <span class="provider-save-hint">
                    macOS only · installs Ollama and pulls <code>gemma4:e4b</code> automatically.
                  </span>
                </div>
              {:else}
                <div class="field-default">
                  Ollama detected
                  {#if ollamaInstall.running}
                    · service running
                  {:else}
                    · service not reachable
                  {/if}
                  {#if ollamaInstall.model_ready}
                    · model <code>gemma4:e4b</code> ready
                  {:else}
                    · model <code>gemma4:e4b</code> missing
                  {/if}
                </div>
              {/if}
              {#if ollamaInstall.in_progress}
                <div class="field-default">
                  Setup phase: <strong>{ollamaPhaseLabel(ollamaInstall.phase)}</strong>
                </div>
                {#if ollamaInstall.progress_log_tail.length > 0}
                  <pre class="ollama-install-log">{ollamaInstall.progress_log_tail.slice(-8).join("\n")}</pre>
                {/if}
              {:else if ollamaInstall.phase === "failed" && ollamaInstall.last_error}
                <div class="provider-hint" role="alert">
                  Install failed: {ollamaInstall.last_error}
                </div>
              {/if}
              <label class="primary-toggle" for="ollama-primary">
                <input
                  id="ollama-primary"
                  type="checkbox"
                  checked={aiProvider === "ollama"}
                  on:change={(e) => togglePrimary("ollama", e.currentTarget)}
                />
                <span>
                  <strong>Set as primary</strong> — chat uses Ollama first,
                  then falls back through the other configured providers.
                </span>
              </label>
            </div>
          {/if}
          <div class="provider-save-row">
            <button
              type="button"
              class="send-btn provider-save-btn"
              on:click={saveProviderSettings}
            >
              Save
            </button>
            <button
              type="button"
              class="provider-reset-btn"
              on:click={resetActiveProviderToDefaults}
              title="Replace the fields above with the values from secrets.env"
            >
              Reset to defaults
            </button>
            <span class="provider-save-hint">Also saved as you type.</span>
          </div>
        </div>
        <div class="panel-section">
          <h2 class="panel-h">Vision</h2>
          <p class="panel-p">
            Provider used by <code>analyze_visual</code> and
            <code>pdf_to_table</code> when Eson needs to read images, PDFs,
            or screenshots. Independent of the chat provider above —
            you can chat with Anthropic while keeping vision local on
            Ollama (or any other combination). Default is Ollama
            (<code>gemma4:e4b</code>, fully local). Anthropic and OpenAI
            require their respective API keys above.
          </p>
          <div class="theme-toggle">
            <button
              type="button"
              class="theme-btn"
              class:selected={visionProvider === "ollama"}
              on:click={() => setVisionProvider("ollama")}
            >
              Ollama (local)
            </button>
            <button
              type="button"
              class="theme-btn"
              class:selected={visionProvider === "anthropic"}
              class:provider-unconfigured={!providerDefaults.anthropicConfigured && !anthropicApiKey.trim()}
              on:click={() => setVisionProvider("anthropic")}
              title={!providerDefaults.anthropicConfigured && !anthropicApiKey.trim()
                ? "No Anthropic key yet — add one in AI Provider above"
                : ""}
            >
              Anthropic
            </button>
            <button
              type="button"
              class="theme-btn"
              class:selected={visionProvider === "openai"}
              class:provider-unconfigured={!providerDefaults.openaiConfigured && !openaiApiKey.trim()}
              on:click={() => setVisionProvider("openai")}
              title={!providerDefaults.openaiConfigured && !openaiApiKey.trim()
                ? "No OpenAI key yet — add one in AI Provider above"
                : ""}
            >
              OpenAI
            </button>
          </div>
          {#if visionProvider === "anthropic" && !providerDefaults.anthropicConfigured && !anthropicApiKey.trim()}
            <div class="provider-hint" role="note">
              Anthropic vision needs your Anthropic API key. Add it in
              <strong>AI Provider</strong> above (paste a <code>sk-ant-…</code>
              key and click Save), then come back here. The chat and vision
              share the same key.
            </div>
          {:else if visionProvider === "openai" && !providerDefaults.openaiConfigured && !openaiApiKey.trim()}
            <div class="provider-hint" role="note">
              OpenAI vision needs your OpenAI API key. Add it in
              <strong>AI Provider</strong> above (paste a <code>sk-…</code>
              key and click Save), then come back here. The chat and vision
              share the same key.
            </div>
          {/if}
          <div class="provider-fields">
            <label class="field-label" for="vision-model">Model</label>
            <input
              id="vision-model"
              class="field-input"
              bind:value={visionModel}
              placeholder={defaultVisionModelFor(visionProvider)}
            />
            {#if providerDefaults.visionModel
              && providerDefaults.visionProvider === visionProvider
              && providerDefaults.visionModel !== visionModel}
              <div class="field-default">
                Default from <code>secrets.env</code>:
                <code>{providerDefaults.visionModel}</code>
                <button
                  type="button"
                  class="field-default-link"
                  on:click={() => (visionModel = providerDefaults.visionModel)}
                >Use</button>
              </div>
            {:else if !visionModel.trim()}
              <div class="field-default">
                Will use <code>{defaultVisionModelFor(visionProvider)}</code>
                when left blank.
              </div>
            {/if}
          </div>
          <div class="provider-save-row">
            <button
              type="button"
              class="send-btn provider-save-btn"
              on:click={() => {
                persistProviderFields();
                pushActivity(
                  "ok",
                  `Vision provider saved · ${visionProvider} · ${visionModel || defaultVisionModelFor(visionProvider)}`,
                );
              }}
            >
              Save
            </button>
            <button
              type="button"
              class="provider-reset-btn"
              on:click={resetVisionToDefaults}
              title="Replace the fields above with the values from secrets.env"
            >
              Reset to defaults
            </button>
            <span class="provider-save-hint">Also saved as you type.</span>
          </div>
        </div>
        <div class="panel-section">
          <h2 class="panel-h">Advanced</h2>
          <p class="panel-p">
            Tune timeouts for slow local models. Defaults work for cloud
            APIs and most Ollama setups; bump these if you're running a
            large local model on CPU and seeing turns get cut off.
          </p>
          <div class="provider-fields">
            <label class="field-label" for="advanced-http-timeout">
              Per-request HTTP timeout (seconds)
            </label>
            <input
              id="advanced-http-timeout"
              class="field-input"
              type="number"
              min="0"
              step="30"
              bind:value={httpTimeoutSecs}
              placeholder="600 (use agent default)"
            />
            <div class="field-default">
              How long the agent waits for one HTTP round against the LLM
              before giving up. Each tool-loop round is its own HTTP call,
              so a 5-round turn can use up to <code>5 ×</code> this value
              of agent time. <strong>0</strong> /
              blank → use the agent's
              <code>ESON_LLM_HTTP_TIMEOUT_SECS</code> env (default
              <code>600&nbsp;s</code> = 10&nbsp;min). Hard-capped at
              <code>3600&nbsp;s</code>. Takes effect on the next turn — no
              restart needed.
            </div>
            <label class="field-label" for="advanced-watchdog">
              UI watchdog timeout (minutes)
            </label>
            <input
              id="advanced-watchdog"
              class="field-input"
              type="number"
              min={Math.round(PENDING_TURN_TIMEOUT_MIN_MS / 60_000)}
              max={Math.round(PENDING_TURN_TIMEOUT_MAX_MS / 60_000)}
              step="5"
              value={Math.round(pendingTurnTimeoutMs / 60_000)}
              on:input={(e) => {
                const minutes = Number(e.currentTarget.value);
                if (!Number.isFinite(minutes)) return;
                const ms = Math.round(minutes * 60_000);
                pendingTurnTimeoutMs = Math.min(
                  PENDING_TURN_TIMEOUT_MAX_MS,
                  Math.max(PENDING_TURN_TIMEOUT_MIN_MS, ms),
                );
              }}
            />
            <div class="field-default">
              How long the chat UI waits for <code>turn_end</code> before
              failing the placeholder bubble. Doesn't cancel the agent —
              the turn keeps running server-side, only the spinner stops.
              Default
              <strong>{Math.round(DEFAULT_PENDING_TURN_TIMEOUT_MS / 60_000)}&nbsp;min</strong>;
              bounds
              <code>{Math.round(PENDING_TURN_TIMEOUT_MIN_MS / 60_000)}–{Math.round(PENDING_TURN_TIMEOUT_MAX_MS / 60_000)}&nbsp;min</code>.
            </div>
          </div>
          <div class="provider-save-row">
            <button
              type="button"
              class="send-btn provider-save-btn"
              on:click={() => {
                persistProviderFields();
                pushActivity(
                  "ok",
                  `Advanced timeouts saved · http=${httpTimeoutSecs > 0 ? `${httpTimeoutSecs}s` : "default"} · watchdog=${Math.round(pendingTurnTimeoutMs / 60_000)}min`,
                );
              }}
            >
              Save
            </button>
            <button
              type="button"
              class="provider-reset-btn"
              on:click={() => {
                httpTimeoutSecs = 0;
                pendingTurnTimeoutMs = DEFAULT_PENDING_TURN_TIMEOUT_MS;
                persistProviderFields();
                pushActivity("info", "Advanced timeouts reset to defaults");
              }}
              title="Restore the built-in defaults (HTTP=server default, watchdog=30 min)"
            >
              Reset to defaults
            </button>
            <span class="provider-save-hint">Also saved as you type.</span>
          </div>
        </div>
        <div class="panel-section">
          <h2 class="panel-h">Background automation</h2>
          <p class="panel-p">
            Provider used by Eson's 24/7 cron skills and inbox watcher. Default is
            Ollama (fully local). If the chosen provider isn't configured, Eson
            automatically falls back to Anthropic Claude so background tasks keep
            running.
            {#if bgResolvedProvider && bgResolvedProvider !== bgProvider}
              <br /><strong>Currently using:</strong>
              {bgResolvedProvider} (fallback — {bgProvider} unavailable)
            {:else if bgResolvedProvider}
              <br /><strong>Currently using:</strong> {bgResolvedProvider}
            {:else}
              <br /><strong>No provider configured</strong> — background tasks
              will be skipped.
            {/if}
          </p>
          <div class="theme-toggle">
            <button
              type="button"
              class="theme-btn"
              class:selected={bgProvider === "anthropic"}
              on:click={() => {
                bgProvider = "anthropic";
                scheduleBackgroundSave();
              }}
            >
              Anthropic
            </button>
            <button
              type="button"
              class="theme-btn"
              class:selected={bgProvider === "openai"}
              on:click={() => {
                bgProvider = "openai";
                scheduleBackgroundSave();
              }}
            >
              OpenAI
            </button>
            <button
              type="button"
              class="theme-btn"
              class:selected={bgProvider === "ollama"}
              on:click={() => {
                bgProvider = "ollama";
                scheduleBackgroundSave();
              }}
            >
              Ollama
            </button>
          </div>
          {#if bgProvider === "anthropic"}
            <div class="provider-fields">
              <label class="field-label" for="bg-anthropic-model">Model</label>
              <input
                id="bg-anthropic-model"
                class="field-input"
                bind:value={bgAnthropicModel}
                on:input={scheduleBackgroundSave}
                placeholder="claude-haiku-4-5-20251001"
              />
            </div>
          {:else if bgProvider === "openai"}
            <div class="provider-fields">
              <label class="field-label" for="bg-openai-model">Model</label>
              <input
                id="bg-openai-model"
                class="field-input"
                bind:value={bgOpenaiModel}
                on:input={scheduleBackgroundSave}
                placeholder="gpt-4o-mini"
              />
            </div>
          {:else}
            <div class="provider-fields">
              <label class="field-label" for="bg-ollama-url">URL</label>
              <input
                id="bg-ollama-url"
                class="field-input"
                bind:value={bgOllamaUrl}
                on:input={scheduleBackgroundSave}
                placeholder="http://127.0.0.1:11434"
              />
              <label class="field-label" for="bg-ollama-model">Model</label>
              <input
                id="bg-ollama-model"
                class="field-input"
                bind:value={bgOllamaModel}
                on:input={scheduleBackgroundSave}
                placeholder="gemma4:e4b"
              />
            </div>
          {/if}
          <div class="provider-save-row">
            <button
              type="button"
              class="send-btn provider-save-btn"
              on:click={saveBackgroundSettings}
            >
              Save
            </button>
            <span class="provider-save-hint">
              {bgSaveHint} ESON_BACKGROUND_PROVIDER env default: {bgEnvDefault}.
            </span>
          </div>
          <hr class="panel-divider" />
          <h3 class="panel-subh">Loop controls</h3>
          <p class="panel-p">
            Master switch and timing for the cron / inbox automation runtime.
            Changes apply on the next heartbeat tick — no agent restart needed.
            Resolved values reflect the env defaults when a field is left
            unset on this device.
          </p>
          <label class="toggle-row" for="bg-loop-enabled">
            <input
              id="bg-loop-enabled"
              type="checkbox"
              bind:checked={bgResolvedLoopEnabled}
              on:change={() => setBgLoopEnabled(bgResolvedLoopEnabled)}
            />
            <span class="toggle-text">
              <strong>Enable background automation</strong>
              <span class="toggle-sub">
                Runs cron skills + dispatches inbox files. Currently
                <em>{bgResolvedLoopEnabled ? "running" : "paused"}</em>
                · env default: {bgEnvLoopEnabled ? "on" : "off"}.
              </span>
            </span>
          </label>
          <label class="toggle-row" for="bg-inbox-auto">
            <input
              id="bg-inbox-auto"
              type="checkbox"
              bind:checked={bgResolvedInboxAuto}
              on:change={() => setBgInboxAuto(bgResolvedInboxAuto)}
            />
            <span class="toggle-text">
              <strong>Auto-process inbox files</strong>
              <span class="toggle-sub">
                Watches <code>workspace/inbox/</code> and runs matching
                <code>skills/inbox/*.md</code> when a file is dropped in.
                Env default: {bgEnvInboxAuto ? "on" : "off"}.
              </span>
            </span>
          </label>
          <div class="provider-fields">
            <label class="field-label" for="bg-heartbeat-sec">
              Heartbeat (seconds, 10–3600)
            </label>
            <input
              id="bg-heartbeat-sec"
              class="field-input"
              type="number"
              min="10"
              max="3600"
              step="1"
              bind:value={bgResolvedHeartbeatSec}
              on:change={() => commitBgHeartbeat(String(bgResolvedHeartbeatSec))}
              placeholder={String(bgEnvHeartbeatSec)}
            />
            <span class="provider-save-hint">
              How often the cron loop wakes up. Env default: {bgEnvHeartbeatSec}s.
            </span>
            <label class="field-label" for="bg-debounce-ms">
              Inbox debounce (ms, 50–10000)
            </label>
            <input
              id="bg-debounce-ms"
              class="field-input"
              type="number"
              min="50"
              max="10000"
              step="50"
              bind:value={bgResolvedInboxDebounceMs}
              on:change={() => commitBgDebounce(String(bgResolvedInboxDebounceMs))}
              placeholder={String(bgEnvInboxDebounceMs)}
            />
            <span class="provider-save-hint">
              Ignores duplicate filesystem events for the same path within this
              window. Env default: {bgEnvInboxDebounceMs}ms.
            </span>
          </div>
        </div>
        <div class="panel-section">
          <h2 class="panel-h">Theme</h2>
          <p class="panel-p">Choose light or dark. Stored in this browser.</p>
          <div class="theme-toggle">
            <button
              type="button"
              class="theme-btn"
              class:selected={themeMode === "dark"}
              on:click={() => applyTheme("dark")}
            >
              Dark
            </button>
            <button
              type="button"
              class="theme-btn"
              class:selected={themeMode === "light"}
              on:click={() => applyTheme("light")}
            >
              Light
            </button>
          </div>
        </div>
      </div>
    {:else}
      <header class="chat-header">
        <h1 class="chat-title">Workspace</h1>
        <p class="chat-sub">
          Files under the agent sandbox (<code>ESON_WORKSPACE_ROOT</code>)
        </p>
        {#if !agentOk}
          <p class="chat-warn">
            Start <code>eson-agent</code> to browse the workspace.
          </p>
        {/if}
      </header>
      <div class="ws-finder-wrap">
        {#if workspaceRootLabel}
          <p class="ws-root">
            <span class="ws-root-label">Root</span>
            <code class="ws-root-path">{workspaceRootLabel}</code>
          </p>
        {/if}
        <div class="ws-toolbar">
          <span class="ws-crumb" title={workspaceActivePath || "(root)"}>
            {workspaceActivePath || "(root)"}
          </span>
          <button
            type="button"
            class="ws-reveal"
            on:click={() => void revealWorkspaceInFinder()}
            title="Open the focused folder in Finder"
          >
            Open in Finder
          </button>
          <button
            type="button"
            class="ws-up"
            on:click={() => void loadRootColumn()}
            title="Reset to workspace root"
          >
            Reset
          </button>
        </div>
        <div class="ws-finder" role="tree">
          {#each workspaceColumns as col, colIdx (colIdx + ":" + col.path)}
            <div class="ws-finder-col" role="group" aria-label={col.path || "(root)"}>
              {#if col.err}
                <p class="ws-err ws-col-err">{col.err}</p>
              {:else if col.loading}
                <p class="ws-col-loading">Loading…</p>
              {:else if col.entries.length === 0}
                <p class="ws-empty">Empty folder</p>
              {:else}
                <ul class="ws-col-list" role="list">
                  {#each col.entries as e (e.name + (e.is_dir ? "/" : ""))}
                    <li>
                      <button
                        type="button"
                        class="ws-col-row"
                        class:is-dir={e.is_dir}
                        class:is-file={!e.is_dir}
                        class:selected={col.selectedName === e.name}
                        on:click={() => void selectInColumn(colIdx, e)}
                        title={e.name}
                      >
                        <span class="ws-col-icon" aria-hidden="true">
                          {e.is_dir ? "📁" : "📄"}
                        </span>
                        <span class="ws-col-label">{e.name}</span>
                        {#if e.is_dir}
                          <span class="ws-col-chev" aria-hidden="true">›</span>
                        {/if}
                      </button>
                    </li>
                  {/each}
                </ul>
              {/if}
            </div>
          {/each}

          {#if workspacePreview}
            <div class="ws-finder-col ws-preview-col" aria-label="Preview">
              <div class="ws-preview-head">
                <span class="ws-preview-name" title={workspacePreview.path}>
                  {workspacePreview.name}
                </span>
                {#if workspacePreview.data && typeof workspacePreview.data.size === "number"}
                  <span class="ws-preview-meta">
                    {humanizeBytes(workspacePreview.data.size)}
                    {#if workspacePreview.data.kind}
                      · {workspacePreview.data.kind}
                    {/if}
                  </span>
                {/if}
              </div>
              {#if workspacePreview.loading}
                <p class="ws-col-loading">Loading preview…</p>
              {:else if workspacePreview.err}
                <p class="ws-err">{workspacePreview.err}</p>
              {:else if workspacePreview.data}
                {#if workspacePreview.data.kind === "markdown"}
                  <div class="ws-preview-md">
                    {@html renderMarkdownPreview(workspacePreview.data.text)}
                  </div>
                  {#if workspacePreview.data.truncated}
                    <p class="ws-preview-trunc">
                      Truncated to first {humanizeBytes(workspacePreview.data.preview_max_bytes)}.
                    </p>
                  {/if}
                {:else if workspacePreview.data.kind === "text" || workspacePreview.data.kind === "code"}
                  <pre class="ws-preview-pre">{workspacePreview.data.text}</pre>
                  {#if workspacePreview.data.truncated}
                    <p class="ws-preview-trunc">
                      Truncated to first {humanizeBytes(workspacePreview.data.preview_max_bytes)}.
                    </p>
                  {/if}
                {:else if workspacePreview.data.kind === "image"}
                  {#if workspacePreview.data.skipped || !workspacePreview.data.data_base64}
                    <p class="ws-preview-note">
                      {workspacePreview.data.note ?? "Image preview unavailable."}
                    </p>
                  {:else}
                    <div class="ws-preview-img-wrap">
                      <img
                        class="ws-preview-img"
                        src={`data:${workspacePreview.data.mime};base64,${workspacePreview.data.data_base64}`}
                        alt={workspacePreview.name}
                      />
                    </div>
                  {/if}
                {:else if workspacePreview.data.kind === "csv"}
                  {#if workspacePreview.data.skipped}
                    <p class="ws-preview-note">{workspacePreview.data.note ?? "Preview skipped."}</p>
                  {:else}
                    <div class="ws-preview-table-wrap">
                      <table class="ws-preview-table">
                        {#if workspacePreview.data.headers && workspacePreview.data.headers.length}
                          <thead>
                            <tr>
                              {#each workspacePreview.data.headers as h, hi (hi)}
                                <th>{h}</th>
                              {/each}
                            </tr>
                          </thead>
                        {/if}
                        <tbody>
                          {#each workspacePreview.data.rows as r, ri (ri)}
                            <tr>
                              {#each r as cell, ci (ci)}
                                <td>{cell}</td>
                              {/each}
                            </tr>
                          {/each}
                        </tbody>
                      </table>
                    </div>
                    {#if workspacePreview.data.rows_truncated}
                      <p class="ws-preview-trunc">
                        Showing first {workspacePreview.data.rows.length} of
                        {workspacePreview.data.total_data_rows} data rows.
                      </p>
                    {/if}
                  {/if}
                {:else if workspacePreview.data.kind === "excel"}
                  {#if workspacePreview.data.skipped}
                    <p class="ws-preview-note">{workspacePreview.data.note ?? "Preview skipped."}</p>
                  {:else}
                    <p class="ws-preview-meta">
                      {workspacePreview.data.sheet_count} sheet(s)
                      {#if workspacePreview.data.sheets.length < workspacePreview.data.sheet_count}
                        · showing first {workspacePreview.data.sheets.length}
                      {/if}
                    </p>
                    {#each workspacePreview.data.sheets as sheet, si (si + ":" + sheet.name)}
                      <details class="ws-preview-sheet" open={si === 0}>
                        <summary>
                          <strong>{sheet.name}</strong>
                          <span class="ws-preview-meta">
                            ({sheet.total_rows}×{sheet.total_cols})
                          </span>
                        </summary>
                        {#if sheet.error}
                          <p class="ws-err">{sheet.error}</p>
                        {:else}
                          <div class="ws-preview-table-wrap">
                            <table class="ws-preview-table">
                              {#if sheet.headers.length}
                                <thead>
                                  <tr>
                                    {#each sheet.headers as h, hi (hi)}
                                      <th>{h}</th>
                                    {/each}
                                  </tr>
                                </thead>
                              {/if}
                              <tbody>
                                {#each sheet.rows as r, ri (ri)}
                                  <tr>
                                    {#each r as cell, ci (ci)}
                                      <td>{cell}</td>
                                    {/each}
                                  </tr>
                                {/each}
                              </tbody>
                            </table>
                          </div>
                          {#if sheet.rows_truncated || sheet.cols_truncated}
                            <p class="ws-preview-trunc">
                              {#if sheet.rows_truncated}
                                Showing first {sheet.rows.length} rows.
                              {/if}
                              {#if sheet.cols_truncated}
                                Columns truncated to {workspacePreview.data.preview_col_cap}.
                              {/if}
                            </p>
                          {/if}
                        {/if}
                      </details>
                    {/each}
                  {/if}
                {:else if workspacePreview.data.kind === "pdf" || workspacePreview.data.kind === "binary"}
                  <p class="ws-preview-note">
                    {workspacePreview.data.note ?? "No inline preview available."}
                  </p>
                {/if}
              {/if}
            </div>
          {/if}
        </div>
      </div>
    {/if}
  </section>

  {#if activityOpen}
    <aside class="activity" aria-label="Activity">
      <div class="act-head">
        <span>Activity</span>
        {#if !actScrollPinned}
          <span class="act-paused" title="Auto-scroll paused while you read">
            paused
          </span>
        {/if}
      </div>
      <div
        class="act-list"
        bind:this={actListEl}
        on:scroll={onActivityScroll}
        on:wheel={onActivityScroll}
      >
        {#each activity as a (a.id)}
          <div
            class="act-row"
            class:err={a.kind === "err"}
            class:ok={a.kind === "ok"}
            class:orch={a.orch === true}
          >
            <span class="act-time">{formatTime(a.t)}</span>
            <span class="act-text">{a.text}</span>
          </div>
        {/each}
        {#if activity.length === 0}
          <div class="act-empty">
            Orchestrator timeline: each chat turn, every tool call, and image scans
            (from the agent over Socket.IO).
          </div>
        {/if}
      </div>
      {#if !actScrollPinned && activity.length > 0}
        <button
          type="button"
          class="act-jump"
          on:click={() => void scrollActivityToBottom(true)}
          title="Jump to the latest entry and resume auto-scroll"
        >
          ↓ Jump to latest
        </button>
      {/if}
    </aside>
  {/if}
</div>

<style>
  .shell {
    display: grid;
    grid-template-columns: var(--wash-sidebar-w) 1fr minmax(200px, 260px);
    height: 100vh;
    background: var(--wash-bg);
    transition: grid-template-columns 0.18s ease;
  }

  .shell.sidebar-collapsed {
    grid-template-columns: var(--wash-sidebar-w-collapsed) 1fr minmax(200px, 260px);
  }

  @media (max-width: 1100px) {
    .shell {
      grid-template-columns: var(--wash-sidebar-w) 1fr;
    }
    .shell.sidebar-collapsed {
      grid-template-columns: var(--wash-sidebar-w-collapsed) 1fr;
    }
    .activity {
      display: none;
    }
  }

  .sidebar {
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--wash-border);
    background: var(--wash-panel);
    padding: 1.1rem 0.9rem;
    gap: 0.75rem;
    min-height: 0;
    overflow: hidden;
  }

  .sidebar.collapsed {
    padding: 0.9rem 0.45rem;
    gap: 0.55rem;
    align-items: stretch;
  }

  .sb-brand {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    padding: 0 0.25rem 0.65rem;
    border-bottom: 1px solid var(--wash-border);
  }

  .sidebar.collapsed .sb-brand {
    padding: 0 0 0.5rem;
    align-items: center;
  }

  .sb-brand-row {
    display: flex;
    align-items: center;
    gap: 0.65rem;
  }

  .sidebar.collapsed .sb-brand-row {
    flex-direction: column;
    gap: 0.4rem;
    width: 100%;
  }

  .sb-logo-img {
    width: 36px;
    height: 36px;
    border-radius: var(--wash-radius-sm);
    object-fit: contain;
    flex-shrink: 0;
  }

  .sb-brand-text {
    display: flex;
    flex-direction: column;
    gap: 0.08rem;
    min-width: 0;
    flex: 1;
  }

  .sidebar.collapsed .sb-brand-text {
    display: none;
  }

  .sb-collapse-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 2rem;
    height: 2rem;
    border-radius: var(--wash-radius-sm);
    color: var(--wash-muted);
    flex-shrink: 0;
    transition:
      background 0.12s ease,
      color 0.12s ease;
  }

  .sb-collapse-btn:hover {
    background: var(--wash-row-hover);
    color: var(--wash-text);
  }

  /* Connection pills sit under the logo row (compact, out of the scroll list). */
  .sb-brand .sb-status {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.35rem;
    margin-top: 0.55rem;
    padding-top: 0.55rem;
    border-top: 1px solid var(--wash-border);
  }

  .sidebar.collapsed .sb-brand .sb-status {
    display: none;
  }

  .sb-nav {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    flex-shrink: 0;
  }

  .sidebar.collapsed .sb-nav {
    align-items: center;
    gap: 0.3rem;
  }

  .sb-nav-btn {
    display: flex;
    flex-direction: row;
    align-items: center;
    justify-content: flex-start;
    gap: 0.65rem;
    width: 100%;
    min-height: 2.5rem;
    padding: 0.45rem 0.65rem;
    border-radius: var(--wash-radius-sm);
    border: none;
    color: var(--wash-muted);
    text-align: left;
    transition:
      background 0.12s ease,
      color 0.12s ease;
  }

  .sb-nav-btn:hover {
    background: var(--wash-row-hover);
    color: var(--wash-text);
  }

  .sb-nav-btn.active {
    color: var(--wash-brand);
    background: var(--wash-brand-dim);
  }

  .sb-nav-label {
    font-size: 0.84rem;
    font-weight: 400;
    letter-spacing: 0.01em;
  }

  .sidebar.collapsed .sb-nav-btn {
    width: 2.25rem;
    min-height: 2.25rem;
    padding: 0;
    justify-content: center;
    gap: 0;
  }

  .sidebar.collapsed .sb-nav-label {
    display: none;
  }

  .sb-svg {
    width: 1.2rem;
    height: 1.2rem;
    display: block;
    flex-shrink: 0;
  }

  .sb-logo {
    font-family: var(--wash-display);
    font-weight: 700;
    font-size: 1.35rem;
    letter-spacing: 0.02em;
    color: var(--wash-brand);
  }

  .sb-tag {
    font-size: 0.68rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--wash-muted);
  }

  .pill {
    font-size: 0.62rem;
    font-family: var(--wash-mono);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    padding: 0.18rem 0.42rem;
    border-radius: 999px;
    border: 1px solid var(--wash-border);
    color: var(--wash-muted);
  }

  .pill.ok {
    border-color: color-mix(in srgb, var(--wash-brand) 55%, var(--wash-border));
    color: var(--wash-brand);
  }

  .pill.bad {
    opacity: 0.55;
  }

  .sb-new {
    margin-top: 0.25rem;
    padding: 0.65rem 0.85rem;
    border-radius: var(--wash-radius-sm);
    background: var(--wash-brand);
    color: var(--wash-on-brand);
    font-weight: 700;
    font-size: 0.88rem;
    text-align: center;
    transition: filter 0.15s ease;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 0.4rem;
  }

  .sb-new-icon {
    font-size: 1.05rem;
    line-height: 1;
  }

  .sb-new:hover {
    filter: brightness(1.08);
  }

  .sidebar.collapsed .sb-new {
    padding: 0.55rem 0;
    width: 2.25rem;
    align-self: center;
  }

  .sidebar.collapsed .sb-new-text {
    display: none;
  }

  .sidebar.collapsed .sb-sessions,
  .sidebar.collapsed .sb-footer {
    display: none;
  }

  .sb-sessions {
    flex: 1;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    margin-top: 0.5rem;
    padding-right: 0.15rem;
  }

  .sb-sessions::-webkit-scrollbar {
    width: 6px;
  }
  .sb-sessions::-webkit-scrollbar-thumb {
    background: var(--wash-border);
    border-radius: 4px;
  }

  .sb-row {
    position: relative;
    padding: 0.55rem 1.6rem 0.55rem 0.65rem;
    border-radius: var(--wash-radius-sm);
    border: 1px solid transparent;
    cursor: pointer;
    transition: background 0.12s ease, border-color 0.12s ease;
  }

  .sb-row:hover {
    background: var(--wash-row-hover);
  }

  .sb-row.active {
    background: var(--wash-brand-dim);
    border-color: color-mix(in srgb, var(--wash-brand) 38%, transparent);
  }

  .sb-row-title {
    font-size: 0.82rem;
    line-height: 1.35;
    color: var(--wash-text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .sb-row-meta {
    font-size: 0.65rem;
    font-family: var(--wash-mono);
    color: var(--wash-muted);
    margin-top: 0.15rem;
  }

  .sb-row-del {
    position: absolute;
    right: 0.25rem;
    top: 50%;
    transform: translateY(-50%);
    width: 1.35rem;
    height: 1.35rem;
    line-height: 1;
    border-radius: 4px;
    color: var(--wash-muted);
    opacity: 0;
    transition: opacity 0.12s ease, background 0.12s ease;
  }

  .sb-row:hover .sb-row-del {
    opacity: 1;
  }

  .sb-row-del:hover {
    background: var(--wash-row-hover);
    color: var(--wash-text);
  }

  .sb-footer {
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
    padding-top: 0.5rem;
    border-top: 1px solid var(--wash-border);
  }

  .sb-link {
    font-size: 0.75rem;
    color: var(--wash-muted);
    text-decoration: underline;
    text-underline-offset: 3px;
  }

  .sb-link:hover {
    color: var(--wash-brand);
  }

  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    background: var(--wash-chat-bg);
    box-shadow: var(--wash-shadow);
    z-index: 1;
  }

  .chat-header {
    padding: 1rem 1.5rem 0.75rem;
    border-bottom: 1px solid var(--wash-border);
    flex-shrink: 0;
  }

  .chat-title {
    margin: 0;
    font-family: var(--wash-display);
    font-size: 1.05rem;
    font-weight: 700;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--wash-muted);
  }

  .chat-sub {
    margin: 0.35rem 0 0;
    font-size: 0.8rem;
    color: var(--wash-muted);
  }

  .chat-sys-stats {
    margin: 0.25rem 0 0;
    font-size: 0.75rem;
    font-family: var(--wash-mono);
    color: var(--wash-text);
    letter-spacing: 0.02em;
  }

  .chat-sys-muted {
    color: var(--wash-muted);
    font-style: normal;
  }

  .chat-warn {
    margin: 0.6rem 0 0;
    font-size: 0.78rem;
    line-height: 1.45;
    color: var(--wash-warn-text);
    max-width: 36rem;
  }

  .chat-warn code {
    font-family: var(--wash-mono);
    font-size: 0.9em;
    background: var(--wash-code-bg);
    padding: 0.08rem 0.3rem;
    border-radius: 4px;
    border: 1px solid var(--wash-border);
  }

  .chat-body {
    flex: 1;
    overflow-y: auto;
    min-height: 0;
  }

  .chat-body::-webkit-scrollbar {
    width: 8px;
  }
  .chat-body::-webkit-scrollbar-thumb {
    background: var(--wash-border);
    border-radius: 4px;
  }

  .empty {
    max-width: var(--wash-chat-max);
    margin: 4rem auto;
    padding: 0 1.5rem;
    text-align: center;
    color: var(--wash-muted);
  }

  .empty-icon {
    font-size: 2rem;
    color: var(--wash-brand);
    opacity: 0.5;
    margin-bottom: 0.75rem;
  }

  .empty h2 {
    margin: 0 0 0.5rem;
    font-size: 1.25rem;
    color: var(--wash-text);
    font-weight: 700;
  }

  .empty p {
    margin: 0;
    line-height: 1.6;
    font-size: 0.95rem;
  }

  .chat-stream {
    max-width: var(--wash-chat-max);
    margin: 0 auto;
    padding: 1.25rem 1.5rem 2rem;
    display: flex;
    flex-direction: column;
    gap: 1.35rem;
  }

  .msg {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    max-width: 100%;
  }

  .msg.user {
    align-self: flex-end;
    align-items: flex-end;
    max-width: min(85%, 36rem);
  }

  .msg.assistant {
    align-self: flex-start;
    width: 100%;
  }

  .msg-label {
    font-size: 0.65rem;
    font-family: var(--wash-mono);
    text-transform: uppercase;
    letter-spacing: 0.14em;
    color: var(--wash-muted);
  }

  .msg-body {
    font-size: 0.98rem;
    line-height: 1.65;
  }

  .msg-body.user-txt {
    background: var(--wash-user-bg);
    border: 1px solid var(--wash-user-border);
    padding: 0.75rem 1rem;
    border-radius: var(--wash-radius);
    white-space: pre-wrap;
    color: var(--wash-text);
  }

  .chart-wrap {
    position: relative;
    width: 100%;
    max-width: 560px;
    height: 280px;
  }

  .msg-body.markdown :global(p) {
    margin: 0 0 0.65rem;
  }
  .msg-body.markdown :global(p:last-child) {
    margin-bottom: 0;
  }
  .msg-body.markdown :global(ul),
  .msg-body.markdown :global(ol) {
    margin: 0.5rem 0;
    padding-left: 1.35rem;
  }
  .msg-body.markdown :global(pre) {
    font-family: var(--wash-mono);
    font-size: 0.85rem;
    background: var(--wash-code-bg);
    border: 1px solid var(--wash-border);
    border-radius: var(--wash-radius-sm);
    padding: 0.75rem 1rem;
    overflow-x: auto;
    margin: 0.65rem 0;
  }
  .msg-body.markdown :global(code) {
    font-family: var(--wash-mono);
    font-size: 0.88em;
    background: var(--wash-code-inline);
    padding: 0.12rem 0.35rem;
    border-radius: 4px;
  }
  .msg-body.markdown :global(pre code) {
    background: none;
    padding: 0;
  }
  .msg-body.markdown :global(table) {
    border-collapse: collapse;
    width: 100%;
    margin: 0.75rem 0;
    font-size: 0.9rem;
  }
  .msg-body.markdown :global(th),
  .msg-body.markdown :global(td) {
    border: 1px solid var(--wash-border);
    padding: 0.4rem 0.55rem;
    text-align: left;
  }
  .msg-body.markdown :global(th) {
    background: var(--wash-brand-dim);
    color: var(--wash-brand);
    font-weight: 700;
  }
  .msg-body.markdown :global(tr:nth-child(even)) {
    background: var(--wash-table-stripe);
  }
  .msg-body.markdown :global(a) {
    color: var(--wash-brand);
  }

  .thinking-inline {
    display: flex;
    align-items: center;
    gap: 0.65rem;
    font-size: 0.88rem;
    color: var(--wash-muted);
    font-family: var(--wash-mono);
    padding: 0.2rem 0;
  }

  /* ─── Reasoning / chain-of-thought panel ──────────────────────────── */

  .reasoning {
    margin: 0 0 0.55rem;
    border: 1px solid color-mix(in srgb, var(--wash-border, rgba(0, 0, 0, 0.08)) 50%, transparent);
    border-radius: 8px;
    background: transparent;
    overflow: hidden;
  }

  .reasoning-toggle {
    appearance: none;
    background: transparent;
    border: none;
    width: 100%;
    text-align: left;
    padding: 0.45rem 0.7rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    cursor: pointer;
    color: var(--wash-fg, inherit);
    font-size: 0.78rem;
    font-family: var(--wash-mono);
  }

  .reasoning-toggle:hover {
    background: color-mix(
      in srgb,
      var(--wash-hover, rgba(0, 0, 0, 0.04)) 50%,
      transparent
    );
  }

  .reasoning-chevron {
    width: 0.9rem;
    color: var(--wash-muted);
  }

  .reasoning-title {
    font-weight: 600;
  }

  .reasoning-meta {
    color: var(--wash-muted);
    font-size: 0.72rem;
    margin-left: auto;
  }

  .reasoning-list {
    list-style: none;
    margin: 0;
    padding: 0.2rem 0.7rem 0.55rem;
    border-top: 1px solid var(--wash-border, rgba(0, 0, 0, 0.06));
  }

  .reasoning-step {
    padding: 0.4rem 0;
    border-bottom: 1px dashed var(--wash-border, rgba(0, 0, 0, 0.06));
  }

  .reasoning-step:last-child {
    border-bottom: none;
  }

  .reasoning-row {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    font-size: 0.78rem;
    font-family: var(--wash-mono);
  }

  .reasoning-icon {
    width: 1rem;
    text-align: center;
    color: var(--wash-muted);
  }

  .reasoning-headline {
    flex: 1 1 auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .step-tool .reasoning-headline,
  .step-fallback .reasoning-headline {
    color: var(--wash-fg, inherit);
  }

  .step-pending .reasoning-headline {
    color: var(--wash-muted);
  }

  .reasoning-dur {
    color: var(--wash-muted);
    font-size: 0.72rem;
  }

  .reasoning-badge {
    font-size: 0.65rem;
    padding: 0.05rem 0.35rem;
    border-radius: 4px;
    font-weight: 600;
    letter-spacing: 0.04em;
  }

  .reasoning-badge.ok {
    color: #047857;
    background: rgba(16, 185, 129, 0.1);
  }

  .reasoning-badge.fail {
    color: #b91c1c;
    background: rgba(239, 68, 68, 0.1);
  }

  .reasoning-detail {
    margin: 0.2rem 0 0 1.45rem;
    color: var(--wash-muted);
    font-size: 0.72rem;
    font-family: var(--wash-mono);
    word-break: break-all;
  }

  .reasoning-thinking {
    margin: 0.35rem 0 0 1.45rem;
    padding: 0.45rem 0.65rem;
    border-left: 2px solid color-mix(
      in srgb,
      var(--wash-link, #2563eb) 45%,
      transparent
    );
    background: transparent;
    color: var(--wash-fg, inherit);
    font-style: italic;
    font-size: 0.78rem;
    line-height: 1.5;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 16rem;
    overflow: auto;
    opacity: 0.85;
  }

  .step-thinking .reasoning-icon {
    color: color-mix(
      in srgb,
      var(--wash-link, #2563eb) 70%,
      var(--wash-muted)
    );
  }

  .step-thinking .reasoning-headline {
    color: var(--wash-muted);
  }

  .reasoning-detail-toggle {
    appearance: none;
    background: none;
    border: none;
    margin: 0.25rem 0 0 1.45rem;
    padding: 0;
    color: var(--wash-link, #2563eb);
    cursor: pointer;
    font-size: 0.72rem;
    text-decoration: underline;
  }

  .reasoning-payload {
    margin: 0.35rem 0 0 1.45rem;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }

  .reasoning-payload-label {
    font-size: 0.65rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--wash-muted);
    margin-bottom: 0.15rem;
  }

  .reasoning-pre {
    margin: 0;
    padding: 0.45rem 0.6rem;
    background: color-mix(
      in srgb,
      var(--wash-code-bg, rgba(0, 0, 0, 0.05)) 35%,
      transparent
    );
    border-radius: 6px;
    font-family: var(--wash-mono);
    font-size: 0.72rem;
    color: var(--wash-fg, inherit);
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 14rem;
    overflow: auto;
  }

  .thinking-dots {
    width: 1.25rem;
    height: 1.25rem;
    border: 2px solid var(--wash-border);
    border-top-color: var(--wash-brand);
    border-radius: 50%;
    animation: spin 0.7s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }

  .preflight-overlay {
    position: fixed;
    inset: 0;
    background: color-mix(in srgb, var(--wash-bg) 92%, transparent);
    backdrop-filter: blur(10px);
    -webkit-backdrop-filter: blur(10px);
    z-index: 10000;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 1.5rem;
    overflow-y: auto;
  }

  .preflight-card {
    width: min(640px, 100%);
    max-height: calc(100vh - 3rem);
    overflow-y: auto;
    background: var(--wash-panel);
    border: 1px solid var(--wash-border);
    border-radius: var(--wash-radius);
    padding: 1.75rem;
    box-shadow: 0 30px 80px rgba(0, 0, 0, 0.45);
  }

  .preflight-head {
    display: flex;
    align-items: flex-start;
    gap: 1rem;
    margin-bottom: 1.25rem;
  }

  .preflight-logo {
    border-radius: 12px;
    flex-shrink: 0;
  }

  .preflight-title {
    margin: 0 0 0.35rem;
    font-size: 1.15rem;
    font-weight: 700;
    color: var(--wash-text);
  }

  .preflight-sub {
    margin: 0;
    color: var(--wash-muted);
    font-size: 0.88rem;
    line-height: 1.5;
  }

  .preflight-table {
    width: 100%;
    border-collapse: collapse;
    margin: 0.5rem 0 1.25rem;
    font-size: 0.85rem;
  }

  .preflight-table th,
  .preflight-table td {
    text-align: left;
    padding: 0.55rem 0.65rem;
    border-bottom: 1px solid var(--wash-border);
  }

  .preflight-table th {
    color: var(--wash-muted);
    font-weight: 600;
    font-size: 0.75rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }

  .preflight-ok {
    color: #2bbf6c;
    font-weight: 600;
  }

  .preflight-fail {
    color: #ff6f6f;
    font-weight: 600;
  }

  .preflight-p {
    margin: 0 0 1rem;
    color: var(--wash-muted);
    font-size: 0.85rem;
  }

  .preflight-err {
    margin: 0.5rem 0 1rem;
    color: #ff6f6f;
    font-size: 0.85rem;
  }

  .preflight-actions {
    display: flex;
    gap: 0.6rem;
    flex-wrap: wrap;
    align-items: center;
    justify-content: flex-end;
  }

  .preflight-btn {
    padding: 0.55rem 1.1rem;
    border: 1px solid var(--wash-border);
    background: var(--wash-bg);
    color: var(--wash-text);
    border-radius: var(--wash-radius-sm);
    font-size: 0.85rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.12s ease;
  }

  .preflight-btn:hover {
    background: var(--wash-row-hover);
  }

  .preflight-btn-secondary {
    background: var(--wash-brand);
    color: var(--wash-on-brand);
    border-color: var(--wash-brand);
  }

  .preflight-btn-secondary:hover {
    background: color-mix(in srgb, var(--wash-brand) 82%, #000);
  }

  .preflight-btn-link {
    background: transparent;
    border-color: transparent;
    color: var(--wash-brand);
    text-decoration: none;
  }

  .preflight-btn-link:hover {
    text-decoration: underline;
    background: transparent;
  }

  .voice-drawer {
    margin: 0 1.25rem;
    padding: 0.85rem 1rem;
    border: 1px dashed var(--wash-border);
    border-radius: var(--wash-radius-sm);
    background: var(--wash-code-bg);
  }

  .voice-title {
    margin: 0 0 0.35rem;
    font-size: 0.8rem;
    font-weight: 700;
    color: var(--wash-brand);
    text-transform: uppercase;
    letter-spacing: 0.08em;
  }

  .voice-muted {
    margin: 0;
    font-size: 0.82rem;
    color: var(--wash-muted);
    line-height: 1.5;
  }

  .voice-muted code {
    font-family: var(--wash-mono);
    font-size: 0.85em;
  }

  .composer {
    flex-shrink: 0;
    padding: 0.85rem 1.25rem 1.1rem;
    border-top: 1px solid var(--wash-border);
    background: linear-gradient(
      180deg,
      transparent,
      color-mix(in srgb, var(--wash-panel) 55%, transparent)
    );
  }

  .provider-switch-notice {
    max-width: var(--wash-chat-max);
    margin: 0 auto 0.55rem;
    padding: 0.45rem 0.65rem 0.45rem 0.85rem;
    background: color-mix(in srgb, var(--wash-accent) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--wash-accent) 42%, transparent);
    border-radius: var(--wash-radius);
    color: var(--wash-text);
    font-size: 0.85rem;
    line-height: 1.35;
    display: flex;
    align-items: flex-start;
    gap: 0.5rem;
  }

  .provider-switch-notice .dismiss {
    margin-left: auto;
    background: none;
    border: none;
    color: inherit;
    font-size: 1.05rem;
    line-height: 1;
    cursor: pointer;
    opacity: 0.7;
    padding: 0 0.15rem;
  }

  .provider-switch-notice .dismiss:hover {
    opacity: 1;
  }

  .composer-inner {
    max-width: var(--wash-chat-max);
    margin: 0 auto;
    display: flex;
    gap: 0.65rem;
    align-items: flex-end;
    padding: 0.5rem 0.65rem;
    background: var(--wash-bg);
    border: 1px solid var(--wash-border);
    border-radius: var(--wash-radius);
  }

  .composer-inner textarea {
    flex: 1;
    resize: none;
    min-height: 2.5rem;
    max-height: 8rem;
    padding: 0.5rem 0.35rem;
    border: none;
    background: transparent;
    color: var(--wash-text);
    line-height: 1.5;
  }

  .composer-inner textarea:focus {
    outline: none;
  }

  .composer-inner textarea::placeholder {
    color: color-mix(in srgb, var(--wash-muted) 45%, transparent);
  }

  .send-btn {
    padding: 0.55rem 1.1rem;
    border-radius: var(--wash-radius-sm);
    background: var(--wash-brand);
    color: var(--wash-on-brand);
    font-weight: 700;
    font-size: 0.85rem;
    flex-shrink: 0;
    transition: filter 0.15s ease;
  }

  .send-btn:hover:not(:disabled) {
    filter: brightness(1.08);
  }

  .send-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .send-btn.stop-btn {
    background: color-mix(in srgb, var(--wash-danger, #c0392b) 85%, transparent);
    color: #fff;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
  }

  .send-btn.stop-btn::before {
    content: "";
    display: inline-block;
    width: 0.65rem;
    height: 0.65rem;
    background: currentColor;
    border-radius: 1px;
  }

  .send-btn.stop-btn:hover {
    filter: brightness(1.05);
  }

  .panel-scroll {
    flex: 1;
    overflow-y: auto;
    min-height: 0;
    padding: 1rem 1.5rem 1.5rem;
    max-width: var(--wash-chat-max);
  }

  .panel-section {
    padding-bottom: 1.5rem;
  }

  .panel-h {
    margin: 0 0 0.35rem;
    font-family: var(--wash-display);
    font-size: 1rem;
    font-weight: 700;
    color: var(--wash-text);
  }

  .panel-p {
    margin: 0 0 0.85rem;
    font-size: 0.85rem;
    color: var(--wash-muted);
    line-height: 1.5;
  }

  .panel-subh {
    margin: 0 0 0.35rem;
    font-size: 0.9rem;
    font-weight: 700;
    color: var(--wash-text);
  }

  .panel-divider {
    border: 0;
    border-top: 1px solid var(--wash-border);
    margin: 1.4rem 0 1rem;
  }

  .toggle-row {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    padding: 0.4rem 0;
    cursor: pointer;
  }

  .toggle-row input[type="checkbox"] {
    margin-top: 0.2rem;
    width: 1rem;
    height: 1rem;
    accent-color: var(--wash-brand);
    cursor: pointer;
  }

  .toggle-text {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    font-size: 0.85rem;
    color: var(--wash-text);
    line-height: 1.4;
  }

  .toggle-sub {
    font-size: 0.78rem;
    color: var(--wash-muted);
    line-height: 1.4;
  }

  .theme-toggle {
    display: inline-flex;
    border: 1px solid var(--wash-border);
    border-radius: var(--wash-radius-sm);
    overflow: hidden;
  }

  .theme-btn {
    padding: 0.5rem 1.25rem;
    font-size: 0.85rem;
    font-weight: 600;
    color: var(--wash-muted);
    background: transparent;
    transition:
      background 0.12s ease,
      color 0.12s ease;
  }

  .theme-btn:hover {
    background: var(--wash-row-hover);
    color: var(--wash-text);
  }

  .theme-btn.selected {
    background: var(--wash-brand-dim);
    color: var(--wash-brand);
  }

  .theme-btn + .theme-btn {
    border-left: 1px solid var(--wash-border);
  }

  .theme-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .provider-fields {
    margin-top: 0.8rem;
    display: grid;
    gap: 0.45rem;
    max-width: 28rem;
  }

  /* Provider button shown when the provider has no key/URL configured yet.
     We deliberately keep it clickable (so the user can open the form to
     paste a key) but visually muted so it's clear it's "unconfigured". */
  .theme-btn.provider-unconfigured {
    opacity: 0.65;
    border-style: dashed;
  }
  .theme-btn.provider-unconfigured.selected {
    opacity: 1;
    border-style: solid;
  }

  .provider-hint {
    margin: 0.65rem 0 0.25rem;
    padding: 0.55rem 0.7rem;
    border: 1px dashed var(--wash-border, rgba(120, 120, 120, 0.35));
    border-radius: 6px;
    font-size: 0.78rem;
    color: var(--wash-muted);
    line-height: 1.5;
    max-width: 28rem;
  }
  .provider-hint code {
    font-family: var(--wash-mono);
    font-size: 0.74rem;
  }

  .primary-summary {
    margin: 0.35rem 0 0.55rem;
    font-size: 0.82rem;
    color: var(--wash-fg, inherit);
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0.4rem;
  }
  .primary-summary-label {
    color: var(--wash-muted);
  }
  .primary-summary-note {
    font-size: 0.74rem;
    color: var(--wash-muted);
  }

  /* Small pill on whichever provider tab is currently the primary. */
  .primary-pill {
    margin-left: 0.4rem;
    padding: 0 0.35rem;
    border-radius: 999px;
    background: var(--wash-accent-soft, rgba(37, 99, 235, 0.12));
    color: var(--wash-link, #2563eb);
    font-size: 0.68rem;
    font-weight: 600;
    letter-spacing: 0.02em;
    white-space: nowrap;
    vertical-align: middle;
  }
  /* Slightly stronger tab border when it is the primary (visible even when
     the tab is not the currently edited one). */
  .theme-btn.provider-primary {
    border-color: var(--wash-link, #2563eb);
  }

  /* "Set as primary" checkbox row shown at the bottom of each provider's
     form. Styled compact so it sits below the key/model inputs without
     stealing focus. */
  .primary-toggle {
    display: flex;
    align-items: flex-start;
    gap: 0.5rem;
    margin-top: 0.7rem;
    padding: 0.55rem 0.7rem;
    border: 1px solid var(--wash-border, rgba(120, 120, 120, 0.25));
    border-radius: 6px;
    font-size: 0.8rem;
    line-height: 1.45;
    color: var(--wash-fg, inherit);
    cursor: pointer;
    user-select: none;
  }
  .primary-toggle input[type="checkbox"] {
    margin-top: 0.15rem;
    flex: 0 0 auto;
  }
  .primary-toggle strong {
    color: var(--wash-link, #2563eb);
  }

  .provider-save-row {
    margin-top: 1rem;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    flex-wrap: wrap;
  }

  .provider-save-btn {
    padding: 0.45rem 1rem;
    font-size: 0.8rem;
  }

  .provider-save-hint {
    font-size: 0.78rem;
    color: var(--wash-muted);
    line-height: 1.4;
  }

  .ollama-install-row {
    margin-top: 0.2rem;
  }

  .ollama-install-log {
    margin: 0.35rem 0 0.1rem;
    max-width: 28rem;
    max-height: 8.5rem;
    overflow: auto;
    padding: 0.5rem 0.6rem;
    border-radius: 6px;
    border: 1px solid var(--wash-border, rgba(120, 120, 120, 0.25));
    background: var(--wash-code-bg, rgba(0, 0, 0, 0.04));
    color: var(--wash-text);
    font-family: var(--wash-mono);
    font-size: 0.72rem;
    line-height: 1.4;
    white-space: pre-wrap;
    word-break: break-word;
  }

  .provider-reset-btn {
    appearance: none;
    background: transparent;
    border: 1px solid var(--wash-border, #d4d4d8);
    border-radius: 6px;
    padding: 0.4rem 0.85rem;
    font-size: 0.78rem;
    color: var(--wash-fg, inherit);
    cursor: pointer;
    transition: background 0.15s ease, border-color 0.15s ease;
  }

  .provider-reset-btn:hover {
    background: var(--wash-hover, rgba(0, 0, 0, 0.04));
    border-color: var(--wash-border-strong, #a1a1aa);
  }

  .field-default {
    margin: 0.25rem 0 0.5rem;
    font-size: 0.74rem;
    color: var(--wash-muted);
    line-height: 1.45;
    display: flex;
    align-items: center;
    gap: 0.4rem;
    flex-wrap: wrap;
  }

  .field-default code {
    font-family: var(--wash-mono);
    font-size: 0.74rem;
    background: var(--wash-code-bg, rgba(0, 0, 0, 0.05));
    padding: 0.05rem 0.3rem;
    border-radius: 4px;
  }

  .field-default-link {
    appearance: none;
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    color: var(--wash-link, #2563eb);
    cursor: pointer;
    font-size: 0.74rem;
    text-decoration: underline;
  }

  .field-default-link:hover {
    color: var(--wash-link-strong, #1d4ed8);
  }

  .field-label {
    font-size: 0.78rem;
    color: var(--wash-muted);
    font-family: var(--wash-mono);
  }

  .field-input {
    border: 1px solid var(--wash-border);
    background: var(--wash-bg);
    color: var(--wash-text);
    border-radius: var(--wash-radius-sm);
    padding: 0.5rem 0.6rem;
    font: inherit;
    font-size: 0.86rem;
  }

  .field-input:focus {
    outline: 1px solid color-mix(in srgb, var(--wash-brand) 50%, transparent);
    border-color: color-mix(in srgb, var(--wash-brand) 50%, var(--wash-border));
  }

  .ws-root {
    margin: 0 0 0.75rem;
    font-size: 0.78rem;
    color: var(--wash-muted);
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    align-items: baseline;
  }

  .ws-root-label {
    font-family: var(--wash-mono);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 0.65rem;
  }

  .ws-root-path {
    font-family: var(--wash-mono);
    font-size: 0.72rem;
    word-break: break-all;
    background: var(--wash-code-bg);
    padding: 0.2rem 0.45rem;
    border-radius: 4px;
    border: 1px solid var(--wash-border);
  }

  .ws-toolbar {
    display: flex;
    align-items: center;
    gap: 0.65rem;
    margin-bottom: 0.85rem;
    flex-wrap: wrap;
  }

  .ws-up {
    padding: 0.4rem 0.85rem;
    border-radius: var(--wash-radius-sm);
    border: 1px solid var(--wash-border);
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--wash-text);
    background: var(--wash-panel);
    transition: background 0.12s ease;
  }

  .ws-up:hover:not(:disabled) {
    background: var(--wash-row-hover);
  }

  .ws-up:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }

  .ws-reveal {
    margin-left: auto;
    padding: 0.4rem 0.85rem;
    border-radius: var(--wash-radius-sm);
    border: 1px solid var(--wash-border);
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--wash-text);
    background: var(--wash-panel);
    cursor: pointer;
    transition: background 0.12s ease;
  }

  .ws-reveal:hover {
    background: var(--wash-row-hover);
  }

  .ws-crumb {
    font-family: var(--wash-mono);
    font-size: 0.78rem;
    color: var(--wash-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
  }

  .ws-err {
    margin: 0 0 0.75rem;
    font-size: 0.82rem;
    color: var(--wash-warn-text);
  }

  .ws-empty {
    margin: 0.75rem 0 0;
    font-size: 0.85rem;
    color: var(--wash-muted);
  }

  /* Finder-style column browser */
  .ws-finder-wrap {
    display: flex;
    flex-direction: column;
    flex: 1 1 auto;
    min-height: 0;
    padding: 1.1rem 1.25rem 0;
    gap: 0.65rem;
  }

  .ws-finder {
    display: flex;
    flex: 1 1 auto;
    min-height: 0;
    overflow-x: auto;
    overflow-y: hidden;
    border: 1px solid var(--wash-border);
    border-radius: var(--wash-radius);
    background: var(--wash-chat-bg);
    scroll-snap-type: x proximity;
  }

  .ws-finder-col {
    flex: 0 0 auto;
    width: 240px;
    min-width: 240px;
    max-width: 240px;
    border-right: 1px solid var(--wash-border);
    display: flex;
    flex-direction: column;
    min-height: 0;
    overflow-y: auto;
    background: var(--wash-panel);
    scroll-snap-align: start;
  }

  .ws-finder-col:last-child {
    border-right: none;
  }

  .ws-preview-col {
    width: 480px;
    min-width: 360px;
    max-width: 720px;
    flex: 1 1 480px;
    background: var(--wash-chat-bg);
    padding: 0.75rem 0.85rem 1rem;
    gap: 0.65rem;
  }

  .ws-col-list {
    list-style: none;
    margin: 0;
    padding: 0.25rem 0;
  }

  .ws-col-list li + li {
    border-top: 1px dashed
      color-mix(in srgb, var(--wash-border) 60%, transparent);
  }

  .ws-col-row {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    width: 100%;
    text-align: left;
    padding: 0.42rem 0.65rem;
    font-size: 0.82rem;
    font-family: var(--wash-mono);
    color: var(--wash-text);
    background: transparent;
    border: 0;
    cursor: pointer;
    transition: background 0.1s ease;
  }

  .ws-col-row:hover {
    background: var(--wash-row-hover);
  }

  .ws-col-row.selected {
    background: color-mix(in srgb, var(--wash-brand) 22%, transparent);
    color: var(--wash-text);
  }

  .ws-col-row.is-file {
    opacity: 0.92;
  }

  .ws-col-icon {
    flex-shrink: 0;
    width: 1.15rem;
    text-align: center;
    font-size: 0.92rem;
  }

  .ws-col-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .ws-col-chev {
    margin-left: auto;
    color: var(--wash-muted);
    font-size: 0.95rem;
  }

  .ws-col-loading,
  .ws-col-err {
    margin: 0.75rem 0.85rem;
    font-size: 0.78rem;
    color: var(--wash-muted);
  }

  .ws-col-err {
    color: var(--wash-warn-text);
  }

  /* Preview pane */
  .ws-preview-head {
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    flex-wrap: wrap;
    padding-bottom: 0.45rem;
    border-bottom: 1px solid var(--wash-border);
  }

  .ws-preview-name {
    font-family: var(--wash-mono);
    font-size: 0.92rem;
    font-weight: 600;
    color: var(--wash-text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
    min-width: 0;
  }

  .ws-preview-meta {
    font-size: 0.72rem;
    color: var(--wash-muted);
    font-family: var(--wash-mono);
  }

  .ws-preview-trunc,
  .ws-preview-note {
    margin: 0.5rem 0 0;
    font-size: 0.75rem;
    color: var(--wash-muted);
    font-style: italic;
  }

  .ws-preview-md {
    font-size: 0.88rem;
    line-height: 1.55;
    color: var(--wash-text);
    overflow-wrap: anywhere;
  }
  .ws-preview-md :global(h1),
  .ws-preview-md :global(h2),
  .ws-preview-md :global(h3) {
    margin-top: 0.85rem;
    margin-bottom: 0.4rem;
  }
  .ws-preview-md :global(pre) {
    background: var(--wash-code-bg);
    padding: 0.55rem 0.65rem;
    border-radius: 6px;
    overflow-x: auto;
    font-size: 0.78rem;
  }
  .ws-preview-md :global(code) {
    background: var(--wash-code-bg);
    padding: 0.05rem 0.3rem;
    border-radius: 4px;
    font-size: 0.85em;
  }
  .ws-preview-md :global(table) {
    border-collapse: collapse;
    margin: 0.45rem 0;
  }
  .ws-preview-md :global(th),
  .ws-preview-md :global(td) {
    border: 1px solid var(--wash-border);
    padding: 0.3rem 0.55rem;
  }

  .ws-preview-pre {
    margin: 0;
    padding: 0.6rem 0.7rem;
    background: var(--wash-code-bg);
    border: 1px solid var(--wash-border);
    border-radius: 6px;
    font-family: var(--wash-mono);
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--wash-text);
    white-space: pre-wrap;
    word-break: break-word;
    overflow-y: auto;
    max-height: calc(100vh - 220px);
  }

  .ws-preview-img-wrap {
    display: flex;
    align-items: center;
    justify-content: center;
    background: color-mix(in srgb, var(--wash-bg) 80%, transparent);
    border: 1px solid var(--wash-border);
    border-radius: 6px;
    padding: 0.5rem;
    overflow: auto;
    max-height: calc(100vh - 220px);
  }

  .ws-preview-img {
    max-width: 100%;
    max-height: calc(100vh - 240px);
    object-fit: contain;
    image-rendering: -webkit-optimize-contrast;
  }

  .ws-preview-table-wrap {
    overflow: auto;
    border: 1px solid var(--wash-border);
    border-radius: 6px;
    max-height: calc(100vh - 250px);
  }

  .ws-preview-table {
    border-collapse: collapse;
    font-family: var(--wash-mono);
    font-size: 0.74rem;
    width: 100%;
  }

  .ws-preview-table th,
  .ws-preview-table td {
    border: 1px solid var(--wash-border);
    padding: 0.3rem 0.55rem;
    text-align: left;
    white-space: nowrap;
    color: var(--wash-text);
  }

  .ws-preview-table thead th {
    position: sticky;
    top: 0;
    background: var(--wash-panel);
    font-weight: 600;
    z-index: 1;
  }

  .ws-preview-table tbody tr:nth-child(even) td {
    background: color-mix(in srgb, var(--wash-row-hover) 50%, transparent);
  }

  .ws-preview-sheet {
    border: 1px solid var(--wash-border);
    border-radius: 6px;
    padding: 0.45rem 0.6rem;
    background: var(--wash-panel);
  }

  .ws-preview-sheet > summary {
    cursor: pointer;
    font-size: 0.82rem;
    font-family: var(--wash-mono);
    color: var(--wash-text);
    display: flex;
    align-items: baseline;
    gap: 0.4rem;
  }

  .activity {
    border-left: 1px solid var(--wash-border);
    background: var(--wash-panel);
    display: flex;
    flex-direction: column;
    min-height: 0;
    font-family: var(--wash-mono);
    position: relative;
  }

  .act-head {
    padding: 0.85rem 1rem;
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.12em;
    color: var(--wash-brand);
    border-bottom: 1px solid var(--wash-border);
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }

  .act-paused {
    margin-left: auto;
    font-size: 0.6rem;
    letter-spacing: 0.08em;
    color: var(--wash-muted);
    background: color-mix(in srgb, var(--wash-muted) 18%, transparent);
    padding: 0.12rem 0.5rem;
    border-radius: 999px;
    text-transform: uppercase;
  }

  .act-jump {
    position: absolute;
    bottom: 0.85rem;
    right: 0.85rem;
    padding: 0.4rem 0.75rem;
    font-size: 0.7rem;
    font-family: var(--wash-mono);
    font-weight: 600;
    letter-spacing: 0.04em;
    color: var(--wash-bg);
    background: var(--wash-brand);
    border: 0;
    border-radius: 999px;
    cursor: pointer;
    box-shadow: var(--wash-shadow);
    transition: transform 0.12s ease, opacity 0.12s ease;
    z-index: 2;
  }

  .act-jump:hover {
    transform: translateY(-1px);
  }

  .act-list {
    flex: 1;
    overflow-y: auto;
    padding: 0.5rem 0.65rem 1rem;
    font-size: 0.68rem;
  }

  .act-row {
    display: grid;
    grid-template-columns: 3.2rem 1fr;
    gap: 0.35rem;
    padding: 0.35rem 0;
    border-bottom: 1px solid var(--wash-act-divider);
    color: var(--wash-muted);
    word-break: break-word;
  }

  .act-row.orch {
    border-left: 2px solid color-mix(in srgb, var(--wash-brand) 55%, transparent);
    padding-left: 0.4rem;
    margin-left: -0.15rem;
  }

  .act-text {
    white-space: pre-wrap;
    line-height: 1.38;
  }

  .act-row.ok .act-text {
    color: color-mix(in srgb, var(--wash-brand) 88%, var(--wash-text));
  }

  .act-row.err .act-text {
    color: #e85d5d;
  }

  .act-time {
    opacity: 0.65;
  }

  .act-empty {
    color: var(--wash-muted);
    opacity: 0.7;
    font-size: 0.72rem;
    padding: 1rem 0.25rem;
  }
</style>
