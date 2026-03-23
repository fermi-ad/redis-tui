---
name: stream-rate-feature
description: Implemented feature to measure and display stream entries-per-second for stream keys — shipped in v1.3.0, PR #63
type: project
---

Feature: stream entries-per-second measurement and display
Branch: `62-add-a-feature-to-measure-how-many-stream-entries-you-get-per-second`
PR: https://github.com/fermi-ad/redis-tui/pull/63 (reviewer: fermi-ad/instrumentation)
Status: **SHIPPED** — all commits merged into branch, PR open for review

**Why:** User wants to monitor write throughput per stream key, including detecting rare trimmed entries over long runs (e.g. weekend-long monitoring).

**How to apply:** Feature is fully implemented. The decisions and architecture below reflect what was actually built.

## Key Decisions (as implemented)

- **Rate source**: Write rate derived from stream entry ID timestamps (unix_ms prefix), not receive time. XREAD with last_id tracking is reliable (no drops), so entry ID timestamps give true write rate.
- **Toggle key**: `i` — toggles the entire right panel between normal value view and rate view. Only active when listening (`l`) on a stream key. Otherwise shows status message.
- **Rate chart**: Rolling window configurable via `--rate-history <minutes>` (default 20). Chart line uses a sliding avg window configurable via `--rate-avg-window <seconds>` (default 2 for responsiveness). Both flags are dynamic — changing them changes the chart.
- **Multi-window header**: Rate view and value view both show 1s / 5s / 10s / 20s / 30s window averages in header row. Uses 60s of `entry_timestamps` in ring buffer (always kept, independent of avg window).
- **Gap detection**: Flag when first new entry timestamp jumps > 1.85× the expected inter-arrival interval. Reference rate always uses 30s window (stable regardless of `--rate-avg-window`). Floor threshold ~500ms. Gaps stored as unix_ms timestamps.
- **Five-number summary**: Min / Q1 / Median / Q3 / Max over all `rate_history` samples. Recomputed at most once per second. `None` during warmup period.
- **Warmup handling**: Chart history recording is delayed until one full `rate_avg_window_secs` has elapsed after listener start (`tracking_start_ms` field tracks this). Prevents the first partial-window sample from permanently anchoring the minimum low.
- **Rate/Stats lines in value view**: Always shown when listening a stream key — not gated on data availability. During warmup, Stats line shows countdown ("warming up — Xs until data available"). After warmup, shows five-number summary.
- **`RateTracker` location**: `App.rate_trackers: HashMap<String, RateTracker>` (keyed by stream key name), not inside `PlotSlot`.
- **Clear on stop**: `clear_rate_tracker()` called when listener stops (`l` toggle) and `rate_view` forced to false.

## RateTracker Fields (as built)

```rust
pub struct RateTracker {
    pub entry_timestamps: VecDeque<u64>,   // unix_ms of all entries in last 60s
    pub rate_history: VecDeque<(u64, f64)>,// (unix_ms, rate) chart samples, pruned to rate_history_secs
    pub gaps: Vec<u64>,                    // unix_ms where gap was detected
    pub last_entry_ms: Option<u64>,        // last seen entry timestamp
    pub total_entries: u64,                // cumulative count
    pub five_num: Option<[f64; 5]>,        // cached [min, Q1, med, Q3, max]
    pub last_five_num_ms: u64,             // ms when five_num was last recomputed
    pub tracking_start_ms: Option<u64>,    // set on first update_rate_tracker call
}
```

## Value View Layout (listening stream)

```
Rate:  1s:X  5s:X  10s:X  20s:X  30s:X  /s   [⚠ N gaps]
Stats: Min:X  Q1:X  Med:X  Q3:X  Max:X  /s    (or: warming up — Xs until data available)
(N older entries hidden)
```

## Rate View Layout

```
┌─ Ingestion Rate [i]  |  N entries  (chart: 2s avg) ──┐
│ Avg: 1s:X  5s:X  10s:X  20s:X  30s:X  /s            │
│ Stats: Min:X  Q1:X  Med:X  Q3:X  Max:X  /s           │
│ [chart — sliding avg rate over --rate-history window] │
└───────────────────────────────────────────────────────┘
```

## Commits (in order)

1. `3282878` — Add stream ingestion rate view and value view rate display
2. `23850d2` — Update CLAUDE.md to document ingestion rate tracking feature
3. `400ed73` — Add five-number summary to rate display with warmup handling
4. `b025f90` — Upgrade ratatui to 0.30.0 to resolve lru IterMut soundness issue
5. `edfa309` — Bump version to 1.3.0 and update CLAUDE.md documentation

## Notable Issues Encountered

- **Startup transient skewing minimum**: First chart sample always low (partial window). Fixed with warmup guard in `update_rate_tracker` — don't push to `rate_history` until `now_ms - tracking_start_ms >= chart_window * 1000`.
- **`--rate-avg-window` no-op bug**: After refactor, flag was no longer wired into chart computation. Fixed by using `rate_avg_window_secs` as `chart_window` in `update_rate_tracker`.
- **lru security issue**: Dependabot flagged `lru < 0.16.3` (IterMut soundness). Could not fix by adding explicit `lru = "0.16.3"` dep because ratatui 0.29 uses semver-incompatible `lru 0.12.x`. Fixed by upgrading to `ratatui = "0.30.0"` which ships `lru 0.16.3`.
- **Copilot reviewer**: User requested Copilot as PR reviewer. `gh pr create --reviewer Copilot` and `gh pr edit --add-reviewer "github-copilot[bot]"` both failed with "Could not resolve user". Must be added manually via GitHub web UI, or requires Copilot code review to be enabled in the fermi-ad org.

## CLI Flags Added

- `--rate-history <minutes>` — rolling chart window (default 20)
- `--rate-avg-window <seconds>` — sliding window for chart line (default 2)
