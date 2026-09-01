# redis-tui

[![CI](https://github.com/fermi-ad/redis-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/fermi-ad/redis-tui/actions/workflows/ci.yml)
[![Build and Push Docker Image](https://github.com/fermi-ad/redis-tui/actions/workflows/docker-publish.yml/badge.svg)](https://github.com/fermi-ad/redis-tui/actions/workflows/docker-publish.yml)

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

Every push to `main` publishes an image to Fermilab's Harbor registry, tagged with
both `latest` and the `Cargo.toml` version:

```bash
docker pull adregistry.fnal.gov/instrumentation/redis-tui:latest
docker run -it --rm --network host adregistry.fnal.gov/instrumentation/redis-tui
```

To build it yourself instead:

```bash
docker build -t redis-tui .
docker run -it --rm redis-tui --hosts <redis-host>
```

To connect to Redis running on the host machine:

```bash
docker run -it --rm --network host redis-tui
```

Passing the connection through the environment avoids a long argument list:

```bash
docker run -it --rm --network host \
  -e REDIS_TUI_HOSTS="db1 db2:6380" \
  -e REDIS_TUI_DB=1 \
  adregistry.fnal.gov/instrumentation/redis-tui
```

### Dev environment

The included `start-dev.sh` script starts two local Redis instances, loads test data (strings, hashes, lists, sets, sorted sets, streams with binary waveforms), starts three simulated instrumentation devices streaming live waveforms, and launches the TUI in multi-host mode:

```bash
./start-dev.sh
```

The devices write continuously to `device:slow` (10 entries/s), `device:medium`
(200/s) and `device:fast` (1200/s), so the stream listener (`l`), ingestion rate
view (`i`) and live plotting have something moving to show. They are stopped
when the TUI exits. `stream-device.py` also runs standalone:

```bash
./stream-device.py --port 6379 --stream device:test --rate 500 --samples 1024
```

Requires `redis-server`, `redis-cli`, and `python3` to be installed.

## Quick Start

```bash
# Connect to localhost
redis-tui

# Connect to a remote host
redis-tui --hosts 10.0.0.5:6380

# Connect with a full URL
redis-tui --hosts redis://:password@host:6379/2

# Connect to several hosts at once, mixing the forms
redis-tui --hosts db1 db2:6380 redis://svc:pw@db3/2

# Load more of each large collection (default 1000 elements)
redis-tui --max-value-items 5000

# Widen the ingestion rate chart to an hour, averaged over 5s
redis-tui --rate-history 60 --rate-avg-window 5

# Wait longer for hosts that are still booting
redis-tui --hosts db1 db2 --connect-retries 10 --connect-timeout 5
```

Every option can be set from the environment instead, with the flag winning if
both are given:

```bash
REDIS_TUI_HOSTS="db1 db2:6380" REDIS_TUI_DB=1 redis-tui
```

See [MANUAL.md](MANUAL.md#command-line-options) for the full flag reference.

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
| `R` | Rename key |
| `z` | Set TTL |
| `d` | Delete key |
| `t` / `T` | Cycle data type forward / back |
| `e` | Toggle endianness |
| `g` | Toggle FFT log scale |
| `a` | Reset plot to auto limits |
| `x` | Plot settings |
| `?` | Help |
| `q` / `Esc` | Quit |

See [MANUAL.md](MANUAL.md) for full keybinding reference and detailed usage guide.

## Architecture

The TUI is built with three main panels:

- **Key List** (left) — browse and filter Redis keys with type badges
- **Value View** (right) — display key metadata and formatted values
- **Data Plot** (bottom) — visualize binary data as waveforms with optional FFT

Background threads handle stream listening (XREAD) and signal generation (XADD + XTRIM) independently per key. Memory is bounded: only the 10 newest entries of a stream are held per key (`MAX_STREAM_ENTRIES`), of which the last 5 are displayed and the newest is plotted. Ingestion rate statistics are computed from a separate 60-second ring of entry timestamps, so the rate view stays accurate even though the entries themselves are discarded.

## License

BSD 3-Clause. See [LICENSE](LICENSE).
