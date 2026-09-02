# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build                    # Dev build
cargo build --release          # Release build (binary at target/release/redis-tui)
cargo test                     # Run all tests
cargo test app::tests          # Run only app module tests
cargo test data::tests         # Run only data module tests
cargo test ui::tests           # Run only UI tests
cargo test <test_name>         # Run a single test by name
cargo test -- --ignored        # Live-Redis tests (spawns throwaway redis-server)
cargo clippy                   # Lint
cargo fmt                      # Format
```

**Docker:**
```bash
docker build -t redis-tui .
docker run -it --rm --network host redis-tui
```

**Dev environment** (requires `redis-server`, `redis-cli`, `python3`):
```bash
./start-dev.sh    # 2 Redis instances + test data + 3 live stream devices, launches TUI in multi-host mode

# Ports are OS-assigned and printed on startup; pin them only if you need to:
REDIS_PORT_1=7001 REDIS_PORT_2=7002 ./start-dev.sh

# The devices alone (start-dev.sh runs three of these and reaps them on exit):
./stream-device.py --port 6379 --stream device:test --rate 500 --samples 1024
```

## Architecture

Five source modules in `src/`, single binary:

- **`main.rs`** — CLI parsing, terminal setup/teardown (raw mode, mouse capture, bracketed paste), event loop (50ms tick), background thread lifecycle (`StreamListener`, `SignalGenerator`), input routing by `InputMode`, mouse event handling (click-to-select, scroll per-pane, shift-bypass)
- **`app.rs`** — All application state (`App` struct), Redis data loading, plot/FFT computation, edit operations, multi-key slot management with per-key data types, ingestion rate tracking (`RateTracker`)
- **`data.rs`** — Binary data encoding/decoding (`DataType` × `Endianness` → `Vec<f64>`), hex formatting, value parsing. `is_binary()` only checks for control chars — do NOT use it to gate binary display for `_`-prefixed stream fields or when a numeric DataType is selected
- **`hosts.rs`** — Turns each `--hosts` entry (`host`, `host:port`, or a full `redis://` URL) into a `redis::ConnectionInfo` plus a credential-free display label. Reports every bad entry at once
- **`redis_client.rs`** — `RedisClient` (one host, carrying its `ConnectionInfo` and label) and `MultiRedisClient` (aggregated multi-host with collision detection), all Redis commands
- **`ui.rs`** — ratatui rendering: layout (key list / value view / data plot), charts (signal + FFT + ingestion rate), popups (filter, edit, help, confirm, signal gen, plot settings), crosshair drawing, per-key Y-axis labels and hover values

### Data Flow

```
Redis → MultiRedisClient → App state → ui::draw()
                              ↑
         StreamListener ──────┘ (mpsc channel, coalesced drain, 10ms budget/tick)
                          also drives RateTracker per key (update_rate_tracker)
         SignalGenerator ──────→ Redis (XADD + XTRIM 100)
         FFT thread ───────────→ App.fft_rx (mpsc, polled via try_recv)
```

### Threading Model

The main thread owns all `App` state and renders UI. Background threads communicate only via `mpsc::channel` (stream data, FFT results) and `Arc<AtomicBool>` (stop flags). No shared mutable state.

- **StreamListener**: per-key XREAD loop (1s block timeout), up to 4 concurrent
- **SignalGenerator**: per-key waveform writer, up to 4 concurrent
- **FFT**: one-shot computation, at most 1 in-flight (`fft_computing` guard + `fft_dirty` flag for re-trigger)
- All stops are non-blocking during normal operation (detached join threads); only blocking on app exit

### Key Constants (app.rs)

| Constant | Value | Purpose |
|----------|-------|---------|
| `MAX_PLOT_SLOTS` | 4 | Simultaneous plotted keys |
| `MAX_STREAM_ENTRIES` | 10 | In-memory stream entries (only last 5 displayed, last entry plotted) |
| `DEFAULT_MAX_VALUE_ITEMS` | 1,000 | Collection elements fetched per value load (`--max-value-items`) |
| `PLOT_WINDOW` | 2,000 | Auto-range visible data points |
| `RATE_WINDOWS` | `[(1,"1s"),(5,"5s"),(10,"10s"),(20,"20s"),(30,"30s")]` | Multi-window averages shown in rate view header and value view |

### Ingestion Rate Tracking

`RateTracker` (in `app.rs`) is stored per stream key in `App.rate_trackers: HashMap<String, RateTracker>`. It is populated by `update_rate_tracker()` called from the main loop drain for every active `StreamListener`.

- **`entry_timestamps`**: ring buffer of entry ID unix_ms timestamps, always pruned to 60s. Used to compute multi-window averages at render time via `rate_for_window(window_secs, now_ms)`, which windows against the **newest entry**, not `now_ms` — windowing on the wall clock made any backlog read as 0/s. `lag_ms`/`is_lagging` report how far behind that stream clock is, and the UI labels a rate older than `RATE_LAG_WARN_MS` (2s) as lagging.
- **`rate_history`**: `(unix_ms, rate)` samples for the chart, computed using `--rate-avg-window` (default 2s) sliding window. Pruned to `--rate-history` (default 20 min). Recording is delayed by one full avg-window after listener start to avoid warmup transients skewing the minimum.
- **`gaps`**: unix_ms timestamps where a jump of >1.85× the expected inter-arrival interval was detected — indicates entries may have been written and trimmed before XREAD could read them.
- **`five_num`**: cached `Option<[f64; 5]>` five-number summary [min, Q1, median, Q3, max] of all `rate_history` values. Recomputed at most once per second. `None` during the warmup period.
- **`tracking_start_ms`**: `Option<u64>` set on first call to `update_rate_tracker`. Used to compute the warmup countdown shown in the value view stats line.

Key `i` toggles `app.rate_view`, which replaces the right panel with `draw_rate_view()`. Per-listener state is dropped by `App::stop_listening`, which both listener removal paths go through — `main.rs`'s `stop_listener` is the only place a listener is removed, because the eviction path once drifted from the explicit-stop path and skipped the cleanup. The rate and stats lines in the value view are always shown for any actively-listened stream key; during warmup the stats line shows a live countdown until data is available.

**CLI flags:**
- `--rate-history <minutes>` — rolling chart window (default 20)
- `--rate-avg-window <seconds>` — sliding window for the plotted chart line (default 2)

**Value view layout (listening stream):**
```
Rate:  1s:X  5s:X  10s:X  20s:X  30s:X  /s   [⚠ N gaps]
Stats: Min:X  Q1:X  Med:X  Q3:X  Max:X  /s    (or: warming up — Xs until data available)
(N older entries hidden)
```

**Rate view layout:**
```
┌─ Ingestion Rate [i]  |  N entries  (chart: 2s avg) ──┐
│ Avg: 1s:X  5s:X  10s:X  20s:X  30s:X  /s            │
│ Stats: Min:X  Q1:X  Med:X  Q3:X  Max:X  /s           │
│ [chart — 2s rolling rate over --rate-history window]  │
└───────────────────────────────────────────────────────┘
```

### Per-Key Data Types

Each `PlotSlot` stores its own `data_type` and `endianness`. Pressing `t`/`T`/`e` changes the type for the currently selected key's slot only. The global `app.data_type` syncs to the selected slot's type so the value view title stays accurate. When decoding slot data (`update_slot_data`, `append_slot_stream_entries`), always use the slot's own type, not the global.

### Input Modes

The event loop routes keyboard input based on `app.input_mode`: `Normal`, `Filter`, `Confirm`, `Help`, `Edit`, `PlotLimit`, `SignalGen`. Each has a dedicated handler function in `main.rs`.

### Mouse Handling

- **Click on key list**: Maps row to key index via `key_list_area` + `key_list_state.offset()`, sets `pending_click_load` flag (processed in main loop with client access)
- **Scroll**: Over key list = viewport scroll (offset only, no selection change). Over value view = content scroll. Over plot = zoom.
- **Shift held**: All mouse events bypassed — terminal handles native text selection. Redraws suppressed while `shift_selecting` is true. Cleared only by non-modifier keypress or non-shift click.
- **Bracketed paste**: Enabled on startup. `Event::Paste(data)` appends to active input field based on current InputMode.
- **Panel areas**: `key_list_area`, `value_view_area`, `signal_chart_area`, `fft_chart_area` stored during draw for mouse hit-testing.

### Chart Y-Axis Bounds

The chart Y bounds must match between `ui.rs` rendering and `app.rs` `mouse_to_data()`:
- Multi-key (>1 slot): normalized `(-0.05, 1.05)`
- Single slot with manual limits: `(slot.y_min, slot.y_max)`
- Single slot auto: computed from data bounds
- No slots: `app.plot_y_min/max` or auto

### Tests

Tests are `#[cfg(test)]` modules at the bottom of `data.rs`, `app.rs`, `ui.rs`, and `redis_client.rs`. Tests in `ui.rs` use `ratatui::backend::TestBackend` for rendering verification.

There is no `tests/` directory: this is a binary-only crate with no lib target, so external integration tests cannot import the modules. Tests needing a live Redis live in `redis_client.rs`'s test module instead, marked `#[ignore]` and run with `cargo test -- --ignored`. The `TestRedis` harness there spawns a throwaway `redis-server` per test and kills it on `Drop`. Ports come from the OS (bind `127.0.0.1:0`, read the assigned port, release it) rather than a fixed or sequential range, so parallel tests and concurrent `cargo test` runs cannot collide with each other or with a dev instance on 6379/6380; `start()` retries up to 5 times on a fresh port and detects early child exit via `try_wait`. They require `redis-server` on `PATH` and are not run by `cargo test` alone.

## Conventions

- Stream plot data uses `_`-prefixed field names in stream entries for binary waveform data
- `_`-prefixed stream fields are ALWAYS decoded as binary — never gate on `is_binary()`
- String values show decoded binary when a numeric DataType is selected (not String/Blob)
- `RedisValue::Stream` entries are capped via `cap_stream_entries()` on load, refresh, and append
- Arrow key navigation auto-loads the selected key's value (`load_selected_value`)
- `refresh_keys()` reloads the selected value itself — it preserves the selection *index*, not the selected key, so callers must not be left holding a value that describes a different key
- Mouse capture must be disabled before any blocking thread joins on exit (prevents terminal garbage)
- Panic hook only runs terminal cleanup on the main thread
- Version tests use `env!("CARGO_PKG_VERSION")` — never hardcode version strings
- Never push directly to main without explicit permission — use feature branches and PRs
- Never include Claude Code attribution in PRs or commits
- Never include Co-Authored-By lines in commits
- Confirmation popups carry their target in `ConfirmAction`; never re-read the live selection when the user answers, since mouse input is not gated by `InputMode`
- `apply_plot_settings()` handles all field counts (X limits + per-slot Y limits) — no field count checks
- One flag names where to connect. Connection info is built as `redis::ConnectionInfo`, never by formatting a URL string — a credential interpolated into a URL breaks on `/ # ? @`
- Never pre-fill the edit popup through `from_utf8_lossy` — it is one-way, and `execute_edit` writes the field straight back. A non-UTF-8 string key opens in binary mode with an empty field instead; gate on `std::str::from_utf8`, never `is_binary()`
- No value fetch may be unbounded: `get_value` caps collections at `max_value_items` and returns the true total in `LoadedValue`, so the pane can report what is hidden. Sets and hashes go through `SSCAN`/`HSCAN`, never `SMEMBERS`/`HGETALL`
- `scan_keys` drives the SCAN cursor by hand — never `Cmd::iter`. Its errors arrive through the same `Result` as a UTF-8 decode failure and do not advance the cursor, so treating every `Err` as a skippable key name re-issues the same SCAN forever
- Nothing reachable from `run_app` may print to stdout or stderr: raw mode owns the alternate screen. Per-host scan failures travel back as `MultiRedisClient::host_errors` and land on the status line. The `eprintln!`s in `from_entries` are fine — they run before raw mode
- `stream-device.py` simulates an instrumentation device: continuous float32 waveforms via `XADD <key> * source device _ <blob>` plus `XTRIM MAXLEN ~`. It speaks RESP over a stdlib socket — the `redis` python package is not a dependency, and one `redis-cli` per entry cannot hold 1200 entries/s. Phase carries across entries so the waveform is continuous and the FFT view shows a signal rather than a per-entry discontinuity
- `start-dev.sh` only ever talks to servers it started itself, on OS-assigned ports (the `TestRedis` trick: bind `127.0.0.1:0`, read the port, release it) in their own `--dir`, killed by one `trap cleanup EXIT INT TERM`. It once treated a `PING` answer on 6379/6380 as its own node and ran `FLUSHALL`, which emptied whatever system Redis or Valkey was there (#117). A `PING` reply is not proof of ownership — the readiness check compares `INFO server`'s `process_id` against the pid it spawned, because when the port is taken the child dies and the incumbent answers instead. Never reintroduce an adopt-if-running path, and never `FLUSHALL`
- The stream drain is bounded by time, never by message count: `drained += 1` per message capped throughput at 400 messages/s regardless of entries carried, and a producer writing entries individually above that grew the channel without bound. `drain_listener` coalesces a listener's whole queue into one batch under a 10ms budget
