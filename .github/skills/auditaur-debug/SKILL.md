---
name: auditaur-debug
description: Debug Auditaur-enabled Tauri apps. Use when asked to inspect app startup, telemetry readiness, frontend errors, IPC, events, traces, WebView/CDP targets, or dogfood/smoke-test an Auditaur integration.
license: MIT
---

# Auditaur debug workflow

Use Auditaur as the first diagnostic surface for Tauri app debugging. Prefer machine-readable JSON when acting as an agent.

## Readiness first

Before reading logs or driving the UI, establish what is ready:

```bash
auditaur debug --app <app-name> --json status
```

For a running app, watch until core telemetry is ready:

```bash
auditaur debug --app <app-name> --active --json watch --until-ready --timeout-seconds 120
```

If the task requires frontend telemetry, add `--require-frontend`. If the task requires WebView selector actions on Windows/WebView2, launch the app with a remote debugging port and add `--cdp-port <port>`.

```bash
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222" npm run tauri dev
auditaur debug --app <app-name> --active --cdp-port 9222 --require-frontend --json watch --until-ready --timeout-seconds 120
```

On Windows PowerShell:

```powershell
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS='--remote-debugging-port=9222'
npm run tauri dev
auditaur debug --app <app-name> --active --cdp-port 9222 --require-frontend --json watch --until-ready --timeout-seconds 120
```

On macOS, Tauri uses WKWebView, which does not expose Chrome DevTools Protocol/WebView2 targets. Use the Auditaur in-app drive bridge for selector actions: the app must explicitly enable `initAuditaur({ driveBridge: true })` in exactly one debug/test WebView per Auditaur session, then `auditaur drive` commands should run without `--cdp-port`.

## Starting the app

Prefer attach mode by default: let the developer, IDE, Tauri dev server, or existing terminal own app startup, then use `auditaur debug watch` to observe readiness. This preserves the user's normal environment, debugger, hot reload, and terminal output.

Use wrapper mode only when the agent or a smoke script needs to own a repeatable run. Wrapper mode should still start the app through its normal command; Auditaur observes that process instead of replacing the app startup system.

| Scenario | Preferred mode |
| --- | --- |
| Human local debugging | Attach to the already-running app |
| IDE/debugger/Tauri dev workflow is already running | Attach |
| Agent needs an end-to-end validation run | Wrapper |
| Dogfood or CI-like local smoke pass | Wrapper |

If the agent should start the app, wrap the existing command:

```bash
auditaur debug --app <app-name> --active --cdp-port 9222 --json run --timeout-seconds 180 -- npm run tauri dev
```

`debug run` reports readiness and intentionally leaves the app running after it becomes ready. Clean up the spawned app process when the validation is done.

## Interpreting readiness

Inspect the `stages` array:

- `app_discovery`: Auditaur discovery file exists.
- `heartbeat`: app heartbeat is fresh.
- `telemetry_database`: SQLite database exists and schema validates.
- `session`: a session row is queryable.
- `window`: Tauri window telemetry exists.
- `backend_telemetry`: backend/plugin logs, spans, or window rows exist.
- `frontend_telemetry`: frontend logs/errors/IPC/events exist; required only with `--require-frontend`.
- `cdp_endpoint`: Chrome DevTools Protocol is reachable; checked only with `--cdp-port`.

If readiness is false, follow the stage messages and `hints` instead of guessing from terminal output.

## Driving the WebView

When CDP readiness is ok, inspect targets before mutating the app if there is more than one target:

```bash
auditaur drive --app <app-name> --active --cdp-port 9222 --json inspect
```

If CDP target selection is ambiguous, pass the specific target id:

```bash
auditaur drive --app <app-name> --active --cdp-port 9222 --json click --target <target-id> --selector '<css>' --allow-unproven-target
```

Use read-only actions first when possible:

```bash
auditaur drive --app <app-name> --active --json exists --selector '<css>'
auditaur drive --app <app-name> --active --json text --selector '<css>'
auditaur drive --app <app-name> --active --json snapshot --selector body --output failure.json
auditaur drive --app <app-name> --active --json screenshot --selector body --output failure.png --snapshot-output failure.json
auditaur drive --app <app-name> --active --cdp-port 9222 --json screenshot --output failure.png --snapshot-output failure.json --selector body
```

Review snapshot artifacts before sharing them; they may contain DOM text, URLs, or other sensitive content.

For text entry, use `fill` when a direct DOM value setter plus `input`/`change` events is enough. Use `type` when a framework-controlled input or textarea needs CDP text insertion after focus:

```bash
auditaur drive --app <app-name> --active --cdp-port 9222 --json type --selector 'textarea' --value 'hello' --visible-only --allow-unproven-target
```

Prefer `--visible-only` (or `--visible`) with selector actions (`wait`, `exists`, `text`, `click`, `fill`, and `type`) when validating modals, focus overlays, or fullscreen shells that leave duplicate hidden DOM behind.

If `drive inspect` on macOS reports CDP unavailable for a WKWebView app, do not keep retrying WebView2 flags. Check `bridge.status`; when it is `active`, use bridge-backed `wait`, `exists`, `text`, `click`, `fill`, `type`, `press`, `snapshot`, and `screenshot` without `--cdp-port`. Bridge screenshots are PNG summary artifacts rendered from DOM text inside the page, not compositor/browser chrome captures. If the bridge is inactive, use Auditaur telemetry (`timeline`, `ipc`, `traces`, `explain`) for truth and pair it with Accessibility automation only when manual UI input must be simulated.

## Inspecting telemetry

After readiness, use structured read commands:

```bash
auditaur apps --json
auditaur sessions --json
auditaur logs --json
auditaur errors --json
auditaur ipc --json
auditaur events --json
auditaur traces --json
auditaur timeline --json
auditaur explain --json
```

Prefer `--session <id>`, `--db <path>`, `--active`, `--latest`, `--pid`, or `--instance-id` when there are stale or multiple sessions.

## Common failure patterns

- No discovery file: app is still compiling, has not launched, or the Auditaur plugin is not registered.
- Database not readable/schema invalid: app initialized partially or data directory is wrong.
- No frontend telemetry: frontend API did not initialize, no frontend action has fired, or export failed in the UI.
- CDP unavailable on macOS/WKWebView: expected; use the in-app bridge when enabled, otherwise use telemetry plus Accessibility fallback.
- CDP target ambiguity: stale WebView targets or multiple app instances are sharing a remote debugging port; run `drive inspect`, clean stale processes, use a fresh port, or pass `--target`.
- JSON output contamination: use `debug run --json` from a current Auditaur CLI; child startup output should not appear in JSON lines.

## Validation loop

For high-confidence changes:

1. Run the relevant tests/builds.
2. Launch or attach with `auditaur debug`.
3. Drive a representative UI path if CDP is needed.
4. Re-check `debug status --require-frontend` when frontend telemetry matters.
5. Inspect `timeline` and `explain`.
6. Clean up spawned app processes and temporary telemetry directories.
