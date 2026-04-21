# Interaction roadmap (chat + voice)

## v1 (current)

- **Chat**: primary UI — sessions, composer, agent replies, Socket.IO tool timeline.
- **Voice**: shell panel in-app (placeholder); same session concept as chat.
- **Transport**: Socket.IO + HTTP on a single agent port (`8787` by default).

## v1.5

- Local **STT** (e.g. Whisper-class) and **TTS** (e.g. Piper-class) behind a stable audio adapter API.
- Microphone permission strings on macOS (`NSMicrophoneUsageDescription`).

## v2

- Optional **LiveKit** for realtime voice rooms (aligns with `eson.md` Transport plane).
- Document split: **LiveKit** owns bidirectional audio media; **Socket.IO** remains for tool/status/memory events unless unified.

## Cloud fallback (optional)

- Disabled by default. When enabled, route only explicit “escalation” turns through configured remote providers; keep audit logs in local SQLite.
