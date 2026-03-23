# redis-tui

A terminal UI client for Redis inspired by Redis Insight, built with Rust and [ratatui](https://github.com/ratatui/ratatui).

## Features

- Browse keys across multiple Redis databases (0-9)
- View and edit values for all Redis data types: strings, hashes, lists, sets, sorted sets, and streams
- Filter keys with glob patterns
- Create, rename, and delete keys
- Set TTL on keys
- Binary data visualization with configurable data types and endianness
- Multi-key plotting — overlay up to 4 keys on the same chart with individual Y-axis scales
- FFT analysis with per-key traces (linear/log scale)
- Live stream listening on up to 4 keys simultaneously with ingestion rate monitoring (rolling averages, five-number summary, gap detection)
- Up to 4 concurrent signal generators for writing waveform data to streams
- Mouse support for plot interaction (drag to pan, scroll to zoom)
- Multi-host support — connect to multiple Redis instances and aggregate keys
- Panic-safe terminal restoration

For detailed usage instructions, see [MANUAL.md](MANUAL.md).

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

## Quick Start

```bash
# Connect to localhost
redis-tui

# Connect to a remote host
redis-tui --host 10.0.0.5 --port 6380

# Connect with a full URL
redis-tui --url redis://:password@host:6379/2

# Connect to multiple hosts
redis-tui --hosts-file hosts.txt
```

## Keybindings at a Glance

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Cycle panels |
| `Up` / `Down` | Navigate / scroll |
| `Enter` | Load key value |
| `0-9` | Switch database |
| `/` | Filter keys |
| `r` | Refresh keys |
| `p` | Toggle key in plot |
| `l` | Toggle stream listener |
| `i` | Toggle ingestion rate view |
| `w` | Toggle signal generator |
| `f` | Toggle FFT |
| `s` | Edit value |
| `n` | New key |
| `d` | Delete key |
| `?` | Help |
| `q` / `Esc` | Quit |

See [MANUAL.md](MANUAL.md) for full keybinding reference and detailed usage guide.

## Architecture

The TUI is built with three main panels:

- **Key List** (left) — browse and filter Redis keys with type badges
- **Value View** (right) — display key metadata and formatted values
- **Data Plot** (bottom) — visualize binary data as waveforms with optional FFT

Background threads handle stream listening (XREAD) and signal generation (XADD + XTRIM) independently per key, with bounded memory usage (max 10,000 stream entries per key in memory).
