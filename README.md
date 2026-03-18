# redis-tui

A terminal UI client for Redis inspired by Redis Insight, built with Rust and [ratatui](https://github.com/ratatui/ratatui).

## Features

- Browse keys across multiple Redis databases (0-9)
- View and edit values for all Redis data types: strings, hashes, lists, sets, sorted sets, and streams
- Filter keys with glob patterns
- Create, rename, and delete keys
- Set TTL on keys
- Binary data visualization with configurable data types and endianness
- **Multi-key plotting** — overlay up to 4 keys on the same chart with individual Y-axis scales
- FFT analysis with per-key traces (linear/log scale)
- Live stream listening on up to 4 keys simultaneously
- Up to 4 concurrent signal generators for writing waveform data to streams
- Key list indicators showing plot (P), listen (L), and signal gen (W) status
- Mouse support for plot interaction (drag to pan, scroll to zoom)
- Multi-host support — connect to multiple Redis instances and aggregate keys
- Panic-safe terminal restoration

## Installation

### From source

```bash
cargo build --release
```

The binary will be at `target/release/redis-tui`.

### Docker

```bash
docker build -t redis-tui .
docker run -it --rm redis-tui --host <redis-host>
```

To connect to Redis running on the host machine:

```bash
docker run -it --rm --network host redis-tui
```

### Dev environment

The included `start-dev.sh` script starts two local Redis instances, loads test data (strings, hashes, lists, sets, sorted sets, streams with binary waveforms), and launches the TUI in multi-host mode:

```bash
./start-dev.sh
```

Requires `redis-server`, `redis-cli`, and `python3` to be installed.

## Usage

```
redis-tui [OPTIONS]
```

### Options

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
# Connect to localhost
redis-tui

# Connect to a remote host
redis-tui --host 10.0.0.5 --port 6380

# Connect with a password
redis-tui --host myredis --password secret

# Connect with a full URL
redis-tui --url redis://:password@host:6379/2

# Connect to multiple hosts
redis-tui --hosts-file hosts.txt
```

### Hosts file format

```
# One Redis URL per line, # for comments
redis://127.0.0.1:6379/0
redis://127.0.0.1:6380/0
redis://:password@10.0.0.5:6379/0
```

## Keybindings

### Navigation

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Cycle between panels (Key List, Value View, Data Plot) |
| `Up` / `Down` / `j` / `k` | Navigate keys, scroll values |
| `Enter` | Load selected key's value |
| `0-9` | Switch Redis database |
| `?` | Show help |
| `q` / `Esc` | Quit |

### Key Operations

| Key | Action |
|-----|--------|
| `/` | Filter keys by glob pattern |
| `r` | Refresh key list |
| `s` | Edit selected key's value |
| `n` | Create new key |
| `d` | Delete selected key (with confirmation) |
| `z` | Set TTL on selected key |
| `R` | Rename selected key |

### Multi-Key Plotting

| Key | Action |
|-----|--------|
| `p` | Toggle selected key in/out of plot (FIFO, max 4) |
| `t` / `T` | Cycle data type forward/backward (Int8, Int16, Int32, UInt8, UInt16, UInt32, Float32, Float64, String, Blob) |
| `e` | Toggle endianness (little/big) |
| `a` | Auto-fit all plot limits |
| `x` | Open plot settings popup (X limits + per-key Y limits) |
| `f` | Toggle FFT frequency analysis (split view) |
| `g` | Toggle FFT Y-axis scale (linear/log) |
| Mouse drag | Pan |
| Mouse scroll | Zoom |

When multiple keys are plotted, each gets its own colored trace (Cyan, Yellow, Green, Magenta) with individual Y-axis scales displayed on the left side of the chart. The legend in the top-right shows key names with their value ranges.

### Streams

| Key | Action |
|-----|--------|
| `l` | Toggle live stream listener for selected key (FIFO, max 4) |
| `w` | Toggle signal generator for selected key / stop if running (FIFO, max 4) |

### Key List Indicators

The key list shows status indicators next to each key:

| Indicator | Meaning |
|-----------|---------|
| `P` (colored) | Key is being plotted (color matches trace) |
| `L` (green) | Stream listener active |
| `W` (red) | Signal generator running |

### Edit Mode

| Key | Action |
|-----|--------|
| `Ctrl+B` | Toggle binary encoding mode |
| `Ctrl+T` | Cycle binary data type |
| `Ctrl+E` | Toggle endianness |
| `Tab` / `Shift+Tab` | Navigate between fields |
| `Enter` | Submit/apply changes |
| `Esc` | Cancel/close popup |

### Plot Settings Popup (x)

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Navigate between fields |
| `Enter` | Apply limits |
| `Esc` | Cancel |

Fields shown: X Min, X Max (shared), plus per-key Y Min / Y Max when multiple keys are plotted.

## Architecture

The TUI is built with three main panels:

- **Key List** (left) — browse and filter Redis keys with type badges
- **Value View** (right) — display key metadata and formatted values
- **Data Plot** (bottom) — visualize binary data as waveforms with optional FFT

Background threads handle stream listening (XREAD) and signal generation (XADD + XTRIM) independently per key.
