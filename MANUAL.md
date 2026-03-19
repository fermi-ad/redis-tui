# Redis TUI Manual

## Table of Contents

1. [Command Line Options](#command-line-options)
2. [Connecting to Redis](#connecting-to-redis)
3. [Interface Layout](#interface-layout)
4. [Navigation](#navigation)
5. [Key Management](#key-management)
6. [Editing Values](#editing-values)
7. [Data Plotting](#data-plotting)
8. [FFT Analysis](#fft-analysis)
9. [Stream Monitoring](#stream-monitoring)
10. [Signal Generator](#signal-generator)
11. [Mouse Controls](#mouse-controls)
12. [Multi-Host Mode](#multi-host-mode)

---

## Command Line Options

```
redis-tui [OPTIONS]
```

| Flag | Description | Default |
|------|-------------|---------|
| `--host <HOST>` | Redis host | `127.0.0.1` |
| `-p, --port <PORT>` | Redis port | `6379` |
| `--password <PASSWORD>` | Redis password | None |
| `-d, --db <DB>` | Redis database number | `0` |
| `-u, --url <URL>` | Full Redis URL (overrides host/port/password/db) | None |
| `--hosts-file <PATH>` | Path to hosts file for multi-host mode | None |

### Examples

```bash
# Default localhost connection
redis-tui

# Remote host with custom port
redis-tui --host 10.0.0.5 --port 6380

# Authenticated connection
redis-tui --host myredis --password secret

# Full URL with database selection
redis-tui --url redis://:password@host:6379/2

# Multi-host aggregation
redis-tui --hosts-file hosts.txt
```

---

## Connecting to Redis

### Single Host

By default, redis-tui connects to `127.0.0.1:6379`. Use `--host`, `--port`, and `--password` flags, or provide a full URL with `--url`.

The URL format is: `redis://[:password@]host:port[/db]`

### Multi-Host Mode

Create a hosts file with one Redis URL per line. Lines starting with `#` are comments. Blank lines are skipped.

```
# hosts.txt
redis://127.0.0.1:6379/0
redis://127.0.0.1:6380/0
redis://:password@10.0.0.5:6379/0
```

Run with: `redis-tui --hosts-file hosts.txt`

In multi-host mode, keys from all hosts are aggregated into a single list. If the same key exists on multiple hosts, a collision warning is displayed when selecting it. The status bar shows which host each key belongs to.

---

## Interface Layout

The interface consists of three panels:

```
+------------------+----------------------------------+
|                  |                                  |
|    Key List      |         Value View               |
|                  |                                  |
|                  |                                  |
+------------------+----------------------------------+
|                                                     |
|                    Data Plot                        |
|                                                     |
+-----------------------------------------------------+
```

- **Key List** (left): Shows all keys with type badges (`str`, `hash`, `list`, `set`, `zset`, `stream`). Status indicators appear next to keys: colored `P` for plotted keys, green `L` for active listeners, red `W` for running signal generators.
- **Value View** (right): Displays key metadata (type, TTL, encoding, size) and the formatted value.
- **Data Plot** (bottom): Visualizes data as waveforms. When FFT is enabled, the plot splits into a signal view (top) and frequency view (bottom).
- **Status Bar** (bottom line): Shows the current operation status, active database, and host info.
- **Title Bar** (top): Shows the app name, help/quit hints, and version.

---

## Navigation

| Key | Action |
|-----|--------|
| `Tab` | Cycle to next panel (Key List -> Value View -> Data Plot -> Key List) |
| `Shift+Tab` | Cycle to previous panel |
| `Up` / `Down` | Navigate keys (Key List panel) or scroll value (Value View panel) |
| `Enter` | Load the selected key's value |
| `0-9` | Switch to Redis database 0-9 |
| `?` | Open help overlay (scroll with Up/Down, press any key to close) |
| `q` / `Esc` | Quit the application |

### Data Plot Navigation (when Data Plot panel is focused)

| Key | Action |
|-----|--------|
| `Up` | Select signal sub-plot (when FFT is active) |
| `Down` | Select FFT sub-plot (when FFT is active) |

---

## Key Management

### Filtering Keys

| Key | Action |
|-----|--------|
| `/` | Enter filter mode |

Type a glob pattern (e.g., `user:*`, `stream:*`) and press `Enter` to apply. Press `Esc` to cancel. The filter uses Redis SCAN MATCH syntax.

### Refreshing

| Key | Action |
|-----|--------|
| `r` | Refresh the key list from Redis |

### Creating Keys

| Key | Action |
|-----|--------|
| `n` | Open new key popup |

In the popup:
- Use `Left`/`Right` to select the key type: string, hash, list, set, zset, or stream
- `Tab`/`Shift+Tab` to navigate between fields
- `Enter` to create the key
- `Esc` to cancel

### Editing Values

| Key | Action |
|-----|--------|
| `s` | Edit the selected key's value |

Opens an edit popup with fields appropriate to the key type. For multi-entry types (lists, sets, sorted sets, streams, hashes), you can submit multiple entries — the popup stays open after each `Enter`. Press `Esc` to close.

### Renaming Keys

| Key | Action |
|-----|--------|
| `R` (Shift+R) | Rename the selected key |

### Deleting Keys

| Key | Action |
|-----|--------|
| `d` | Delete the selected key |

A confirmation prompt appears. Press `y` to confirm or `n`/`Esc` to cancel.

### Setting TTL

| Key | Action |
|-----|--------|
| `z` | Set TTL on the selected key |

Enter the TTL in seconds. Use `-1` to remove the TTL (persist the key).

---

## Editing Values

### Binary Encoding Mode

When editing values, you can encode numeric input as binary data:

| Key | Action |
|-----|--------|
| `Ctrl+B` | Toggle binary encoding mode on/off |
| `Ctrl+T` | Cycle binary data type (Int8 through Float64) |
| `Ctrl+E` | Toggle endianness (little/big) |

When binary mode is on, your numeric input is encoded into raw bytes using the selected data type and endianness before being stored in Redis.

### Edit Popup Controls

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Navigate between fields |
| `Enter` | Submit the edit |
| `Esc` | Cancel / close (for multi-entry types, closes after all entries are submitted) |
| `Backspace` | Delete character |

---

## Data Plotting

The data plot panel visualizes Redis values as waveforms. It supports all value types:

- **Strings**: Decoded as binary blobs using the selected data type
- **Streams**: Plots the binary data from the last entry's `_`-prefixed field
- **Lists**: Parses items as numbers, or decodes as binary blobs
- **Sorted Sets**: Plots the scores
- **Hashes**: Parses values as numbers, or decodes as binary blobs

### Plot Controls

| Key | Action |
|-----|--------|
| `p` | Toggle the selected key in/out of the plot (FIFO, max 4 keys) |
| `t` | Cycle data type forward (Int8, Int16, Int32, UInt8, UInt16, UInt32, Float32, Float64, String, Blob) |
| `T` (Shift+T) | Cycle data type backward |
| `e` | Toggle endianness (little/big) |
| `a` | Auto-fit all plot axis limits |
| `x` | Open plot settings popup |

### Multi-Key Plotting

Up to 4 keys can be plotted simultaneously. Each key gets a unique color:

| Slot | Color |
|------|-------|
| 1 | Cyan |
| 2 | Yellow |
| 3 | Green |
| 4 | Magenta |

When a 5th key is added, the oldest is evicted (FIFO). Colors are reassigned based on slot position. The legend in the top-right of the chart shows each key's name and current value range.

Per-key Y-axis scales are displayed on the left side of the chart, color-coded to match each trace.

### Plot Settings Popup (x)

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Navigate between fields |
| `Enter` | Apply settings |
| `Esc` | Cancel |

Fields: X Min, X Max (shared across all keys), plus per-key Y Min / Y Max when multiple keys are plotted. Leave blank or set both to 0 for auto-scaling.

### Plot Viewport

The plot shows the most recent 2,000 data points by default (auto-scrolling window). Use the plot settings popup or mouse controls to customize the viewport.

---

## FFT Analysis

| Key | Action |
|-----|--------|
| `f` | Toggle FFT frequency analysis on/off |
| `g` | Toggle FFT Y-axis scale between linear and log |

When FFT is enabled, the plot splits horizontally:
- **Top**: Signal waveform (time domain)
- **Bottom**: Frequency spectrum (FFT magnitude)

Use `Up`/`Down` arrows (with Data Plot panel focused) to select which sub-plot is active for axis controls.

FFT is computed in a background thread and updates automatically as new data arrives. The DC offset (mean) is removed before computation for cleaner visualization.

---

## Stream Monitoring

### Live Listening

| Key | Action |
|-----|--------|
| `l` | Toggle live stream listener for the selected key |

When a listener is active:
- New entries are received via background XREAD polling (1-second timeout)
- The plot updates in real-time with incoming data
- The status bar shows the entry count per update
- A green `L` indicator appears next to the key in the list
- Up to 4 listeners can run simultaneously (FIFO eviction if exceeded)
- Stream entries in memory are capped at 10,000 per key to prevent OOM

Press `l` again on the same key to stop its listener.

---

## Signal Generator

The signal generator writes synthetic waveform data to Redis streams, useful for testing and demonstration.

| Key | Action |
|-----|--------|
| `w` | Open signal generator popup (if on a stream key) / stop if already running |

### Signal Generator Popup

| Field | Description | Default |
|-------|-------------|---------|
| Wave Type | sine, square, sawtooth, triangle (use Left/Right to select) | sine |
| Data Type | Numeric encoding type (use Left/Right to select) | Float32 |
| Cycles/Entry | Number of wave cycles per stream entry | 1.0 |
| Amplitude | Wave amplitude | 1.0 |
| Noise | Random noise amplitude (0 = none) | 0.0 |
| Samples/Entry | Number of samples per stream entry | 100 |
| Entries/Sec | Rate of stream entry generation | 10.0 |

Navigation:
- `Tab`/`Shift+Tab` to move between fields
- `Left`/`Right` to change Wave Type and Data Type selections
- Type numeric values into text fields
- `Enter` to start the generator
- `Esc` to cancel

When running:
- A red `W` indicator appears next to the key in the list
- The stream is trimmed to the last 100 entries to prevent unbounded growth
- Up to 4 generators can run simultaneously (FIFO eviction if exceeded)
- Press `w` on the same key to stop its generator

### Typical Workflow

1. Create or select a stream key
2. Press `w` to open the signal generator popup
3. Configure the wave parameters and press `Enter`
4. Press `l` to start listening to the same key
5. Press `p` to add the key to the plot
6. Press `f` to enable FFT and see the frequency spectrum

---

## Mouse Controls

Mouse interaction works within the Data Plot panel:

| Action | Effect |
|--------|--------|
| Click + drag | Pan the plot viewport |
| Scroll up | Zoom in (centered on cursor position) |
| Scroll down | Zoom out (centered on cursor position) |
| Hover | Shows crosshair with data coordinates |

When FFT is active, mouse actions apply to whichever sub-plot (signal or FFT) the cursor is hovering over.

Press `a` to reset to auto-fit limits after manual panning/zooming.

---

## Multi-Host Mode

When using `--hosts-file`, redis-tui connects to all listed hosts and aggregates their keys into a single view.

### Key Collision Handling

If the same key name exists on multiple hosts:
- A warning is shown in the status bar when selecting the key
- The host label is displayed in the value view header

### Host Labels

Host labels are derived from the URL (the `host:port` portion, with auth stripped). These appear in the status bar and collision warnings to identify which host a key belongs to.
