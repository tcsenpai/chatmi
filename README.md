# chatmi

A desktop chat app for [MiMo](https://platform.xiaomimimo.com), Xiaomi's LLM.
Built with Tauri, so it's a small native binary for macOS and Linux, not an
Electron install.

It looks and behaves like a typical chat client: a sidebar of conversations, a
message thread, streaming replies with the model's reasoning shown in a
collapsible block. Conversations are stored locally in SQLite. You can attach
images and audio and the app switches to the multimodal model automatically.

## Why it's built this way

The API key must never end up in the webview, where it would be trivial to read.
So the app runs a small Node sidecar that owns the [`mimo-api`](https://www.npmjs.com/package/mimo-api)
SDK and does the actual streaming. The Rust core spawns it, passes the key
through the environment, and bridges its output to the UI as events. The webview
only ever sees text chunks.

```
Svelte webview  ──IPC──►  Rust core  ──stdin──►  Node sidecar (mimo-api)
      ▲                       │                        │
      └───── events ──────────┘◄──── stdout ───────────┘
                              │
                          SQLite
```

The sidecar speaks JSON-lines: one request per line in, one chunk per line out
(`reasoning`, `content`, `done`, `error`). Multimodal messages are the same
shape with image/audio parts added, so nothing structural changes as features
grow.

## Requirements

- [Bun](https://bun.sh)
- Rust (stable) and the [Tauri prerequisites](https://tauri.app/start/prerequisites/)
  for your platform (on Linux: `webkit2gtk`, `librsvg`, etc.)

## Setup

```sh
bun install
cp .env.example .env      # then fill in MIMO_API_KEY (and MIMO_URL if needed)
```

`MIMO_URL` is optional — it defaults to the SDK's endpoint. If you set it, don't
include the `/v1` suffix; the SDK adds the path itself.

## Run

```sh
bun run tauri:dev     # builds the sidecar, then starts Tauri in dev mode
```

`tauri:dev`/`tauri:build` run `sidecar:build` first, which produces
`src-tauri/binaries/chatmi-sidecar-<target-triple>` — Tauri picks that up as a
bundled binary. **Rerun `bun run sidecar:build` (and restart) whenever you
change anything under `sidecar/`** — the running app uses the compiled binary,
not the TypeScript source.

## Build

```sh
bun run tauri:build
```

Bundles land in `src-tauri/target/release/bundle/`.

## Project layout

```
src/            Svelte 5 frontend (chat UI)
sidecar/        Node sidecar — wraps the mimo-api SDK, streams over stdio
src-tauri/      Rust core — window, SQLite persistence, sidecar bridge
```

## Status

Chat with reasoning, local persistence, and image/audio input all work. Text-to-
speech and voice, which the SDK supports, aren't wired up yet.

## License

MIT
