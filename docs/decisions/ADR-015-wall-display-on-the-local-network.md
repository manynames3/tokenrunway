# ADR-015: The wall display is a page the app serves on the local network

- **Status**: Accepted
- **Date**: 2026-11-26
- **Related**: [design/02-features.md](../design/02-features.md#wall-display), `native/src/web.rs`

## Context

People who run out of usage want the limits visible at a glance, all day, on a spare screen such as an old iPad mini. The readings live in the app on the Mac (account limits, local logs), and old tablets can't run a current app or a current browser engine.

## Decision

The app serves the wall display itself, from a small HTTP server on port 8765 (std only, one thread), only while **Wall display** is switched on in the menu. It answers only at a random eight-character path (`/<key>` for the page, `/<key>/pet/<name>.png` for the pets) and 404s everything else; the key and the switch live in `~/.claude-usage-monitor/wall-display.json` (mode 600). The page holds percentages, reset times, the API-rate value and pets, never an account, an email or a token. It is server-rendered HTML and CSS that reloads itself every minute, with no JavaScript, and the pets are strips cut from the app's own sprite sheets at 128 px and cached.

## Alternatives considered

- **A native iPad app**: current Xcode can't build for old iPads, and it would need its own copy of the readings.
- **The iPad as a second display (Sidecar)**: not available on older models, and it keeps the Mac's screen busy.
- **A hosted page**: the readings would leave the machine.

## Consequences

- The Mac must be awake and on the same network; when it sleeps the page shows the browser's own error.
- Plain HTTP on the local network: anyone on it who learns the key can read the page. The key is the only gate, which is why the page carries no account data.
- macOS may ask once whether the app can accept incoming connections.
