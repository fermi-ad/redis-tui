#!/usr/bin/env bash
set -euo pipefail

# Both nodes run with persistence off (--save '' --appendonly no).
# Without it redis writes dump.rdb into whatever directory it was launched
# from, which is the repo root, and the live tests then adopt that file as
# their starting dataset - `dir` defaults to the working directory and an
# existing dump.rdb is loaded at boot whether or not saving is enabled.
# This is throwaway fixture data that FLUSHALL rebuilds on every run, so
# there is nothing here worth persisting.
REDIS_PORT_1=6379
REDIS_PORT_2=6380

echo "=== Redis TUI Dev Environment (Multi-Host) ==="

# Check for redis-server
if ! command -v redis-server &>/dev/null; then
    echo "ERROR: redis-server not found. Install redis first."
    echo "  Ubuntu/Debian: sudo apt install redis-server"
    echo "  macOS:         brew install redis"
    exit 1
fi

# Check for redis-cli
if ! command -v redis-cli &>/dev/null; then
    echo "ERROR: redis-cli not found."
    exit 1
fi

# ─── Start Redis Node 1 ──────────────────────────────────
if redis-cli -p "$REDIS_PORT_1" ping &>/dev/null; then
    echo "[*] Redis node 1 already running on port $REDIS_PORT_1"
else
    echo "[*] Starting redis node 1 on port $REDIS_PORT_1..."
    redis-server --port "$REDIS_PORT_1" --daemonize yes --loglevel warning \
        --save '' --appendonly no
    sleep 1
    if ! redis-cli -p "$REDIS_PORT_1" ping &>/dev/null; then
        echo "ERROR: Failed to start redis node 1"
        exit 1
    fi
    echo "[*] Node 1 started (PID $(redis-cli -p "$REDIS_PORT_1" INFO server | grep process_id | tr -d '\r' | cut -d: -f2))"
fi

# ─── Start Redis Node 2 ──────────────────────────────────
if redis-cli -p "$REDIS_PORT_2" ping &>/dev/null; then
    echo "[*] Redis node 2 already running on port $REDIS_PORT_2"
else
    echo "[*] Starting redis node 2 on port $REDIS_PORT_2..."
    redis-server --port "$REDIS_PORT_2" --daemonize yes --loglevel warning \
        --save '' --appendonly no
    sleep 1
    if ! redis-cli -p "$REDIS_PORT_2" ping &>/dev/null; then
        echo "ERROR: Failed to start redis node 2"
        exit 1
    fi
    echo "[*] Node 2 started (PID $(redis-cli -p "$REDIS_PORT_2" INFO server | grep process_id | tr -d '\r' | cut -d: -f2))"
fi

CLI1="redis-cli -p $REDIS_PORT_1"
CLI2="redis-cli -p $REDIS_PORT_2"

echo "[*] Flushing existing data on both nodes..."
$CLI1 FLUSHALL >/dev/null
$CLI2 FLUSHALL >/dev/null

# ═══════════════════════════════════════════════════════════
# NODE 1 DATA
# ═══════════════════════════════════════════════════════════
echo ""
echo "=== Loading data on Node 1 (port $REDIS_PORT_1) ==="

# ─── Strings ───────────────────────────────────────────────
$CLI1 SET "string:greeting" "Hello, Redis TUI!" >/dev/null
$CLI1 SET "string:json_config" '{"debug":true,"log_level":"info","max_connections":100,"features":["auth","caching","streams"]}' >/dev/null
$CLI1 SET "string:counter" "42" >/dev/null

$CLI1 SET "string:ephemeral" "I expire in 300 seconds" >/dev/null
$CLI1 EXPIRE "string:ephemeral" 300 >/dev/null

# ─── Blobs ─────────────────────────────────────────────────
echo "[*] Generating blobs..."

# float32 blob - 1k elements, multi-freq sine
python3 -c "
import struct, math, sys
n = 1000
vals = [math.sin(i*0.01) + 0.5*math.sin(i*0.05) + 0.25*math.sin(i*0.13) for i in range(n)]
sys.stdout.buffer.write(struct.pack(f'<{n}f', *vals))
" | $CLI1 -x SET "blob:float32_1k" >/dev/null
echo "  blob:float32_1k"

# random bytes blob for hex view
python3 -c "
import os, sys
sys.stdout.buffer.write(os.urandom(256))
" | $CLI1 -x SET "blob:random_256b" >/dev/null
echo "  blob:random_256b"

# ─── Hashes ───────────────────────────────────────────────
$CLI1 HSET "hash:user:1001" name "Alice" email "alice@example.com" age 30 role "admin" active "true" >/dev/null
$CLI1 HSET "hash:server:config" host "0.0.0.0" port "8080" workers "4" timeout "30" tls "enabled" >/dev/null

# ─── Lists ─────────────────────────────────────────────────
$CLI1 RPUSH "list:task_queue" "send_email:user@test.com" "resize_image:photo_001.jpg" "generate_report:Q4_2025" >/dev/null
$CLI1 RPUSH "list:numbers" 10 20 30 40 50 60 70 80 90 100 >/dev/null

# ─── Sets ──────────────────────────────────────────────────
$CLI1 SADD "set:tags" "rust" "redis" "tui" "ratatui" "cli" "database" "visualization" >/dev/null

# ─── Sorted Sets ──────────────────────────────────────────
$CLI1 ZADD "zset:leaderboard" 9500 "alice" 8700 "bob" 8200 "charlie" 7100 "diana" 6500 "eve" 5900 "frank" >/dev/null

# ─── Streams ──────────────────────────────────────────────
$CLI1 XADD "stream:app_log" "*" level INFO msg "Application started" service "api" >/dev/null
$CLI1 XADD "stream:app_log" "*" level WARN msg "High memory usage detected" service "worker" >/dev/null
$CLI1 XADD "stream:app_log" "*" level ERROR msg "Database connection lost" service "api" >/dev/null

# Small sensor stream (20 entries, float32 _data)
echo "[*] Generating small sensor stream..."
for i in $(seq 0 19); do
    python3 -c "
import struct, math, sys
t = $i * 0.5
temp = 20.0 + 5.0 * math.sin(t) + 0.5 * (($i * 7) % 3 - 1)
humidity = 60.0 + 10.0 * math.cos(t * 0.7)
pressure = 1013.25 + 2.0 * math.sin(t * 0.3)
accel_x = 0.01 * math.sin(t * 2.0)
accel_y = 0.01 * math.cos(t * 2.0)
accel_z = 9.81 + 0.005 * math.sin(t * 5.0)
sys.stdout.buffer.write(struct.pack('<6f', temp, humidity, pressure, accel_x, accel_y, accel_z))
" | $CLI1 -x XADD "stream:sensor_data" "*" sensor_id "env-001" _ >/dev/null
done

# ─── Big streams ──────────────────────────────────────────
echo "[*] Generating big streams on node 1..."

generate_big_stream() {
    local cli=$1
    local key=$2
    local count=$3
    local dtype=$4
    local fmt=$5
    local values_per_entry=$6

    echo "  ${key} (${count} entries, ${dtype})..."

    python3 -c "
import struct, math, sys

key = '${key}'
count = ${count}
fmt = '${fmt}'
vpe = ${values_per_entry}
dtype = '${dtype}'

for i in range(count):
    # Each entry is a waveform: multi-freq sine, shifted by entry index
    phase_offset = i * 0.3
    vals = []
    for j in range(vpe):
        t = j / vpe * 2 * math.pi + phase_offset
        v = math.sin(t) + 0.5 * math.sin(3 * t) + 0.25 * math.sin(7 * t)
        if dtype == 'float32' or dtype == 'float64':
            vals.append(v)
        elif dtype == 'int16':
            vals.append(int(max(-32768, min(32767, 16000 * v))))
        elif dtype == 'uint16':
            vals.append(int(max(0, min(65535, 32768 + 18000 * v))))
        elif dtype == 'int32':
            vals.append(int(max(-2**31, min(2**31-1, int(1e8 * v)))))
        elif dtype == 'uint32':
            vals.append(int(max(0, min(2**32-1, 2**31 + int(5e8 * v)))))
        elif dtype == 'int8':
            vals.append(int(max(-128, min(127, 72 * v))))
        elif dtype == 'uint8':
            vals.append(int(max(0, min(255, 128 + 72 * v))))
        else:
            vals.append(v)

    blob = struct.pack(fmt, *vals)

    parts = ['XADD', key, '*', 'source', 'gen', '_']
    resp = f'*{len(parts) + 1}\r\n'
    for p in parts:
        b = p.encode()
        resp += f'\${len(b)}\r\n'
        sys.stdout.buffer.write(resp.encode())
        sys.stdout.buffer.write(b)
        sys.stdout.buffer.write(b'\r\n')
        resp = ''
    sys.stdout.buffer.write(f'\${len(blob)}\r\n'.encode())
    sys.stdout.buffer.write(blob)
    sys.stdout.buffer.write(b'\r\n')
" | $cli --pipe >/dev/null 2>&1
}

generate_big_stream "$CLI1" "stream:float32_500"  100  "float32" "<500f" 500
generate_big_stream "$CLI1" "stream:int16_1000"   100  "int16"   "<1000h" 1000
generate_big_stream "$CLI1" "stream:uint8_2000"   100  "uint8"   "<2000B" 2000

# Large streams for FFT stress testing
echo "[*] Generating large streams on node 1..."
generate_big_stream "$CLI1" "stream:large_f32_10k"  50  "float32" "<10000f" 10000

# ─── Some keys in DB 1 ────────────────────────────────────
$CLI1 -n 1 SET "db1:test_key" "This is in database 1 on node 1" >/dev/null
$CLI1 -n 1 HSET "db1:info" description "DB 1 on node 1" purpose "testing" >/dev/null


# ═══════════════════════════════════════════════════════════
# NODE 2 DATA
# ═══════════════════════════════════════════════════════════
echo ""
echo "=== Loading data on Node 2 (port $REDIS_PORT_2) ==="

# ─── Strings (unique to node 2) ──────────────────────────
$CLI2 SET "string:node2_greeting" "Hello from Node 2!" >/dev/null
$CLI2 SET "string:node2_config" '{"region":"us-east","replica":true}' >/dev/null

# ─── Hashes (unique to node 2) ──────────────────────────
$CLI2 HSET "hash:user:2001" name "Bob" email "bob@example.com" age 25 role "user" active "true" >/dev/null
$CLI2 HSET "hash:metrics" cpu_pct "55.2" mem_mb "1024" disk_gb "80.1" uptime_hrs "720" requests "2541098" >/dev/null

# ─── Lists (unique to node 2) ────────────────────────────
$CLI2 RPUSH "list:events" "deploy:v2.1.0" "rollback:v2.0.9" "scale_up:workers=8" >/dev/null

# ─── Sets (unique to node 2) ─────────────────────────────
$CLI2 SADD "set:blocked_ips" "192.168.1.100" "10.0.0.55" "172.16.0.99" >/dev/null

# ─── Sorted Sets (unique to node 2) ─────────────────────
$CLI2 ZADD "zset:temperatures" -10.5 "jan" -2.3 "feb" 5.0 "mar" 12.8 "apr" 20.1 "may" 26.5 "jun" 30.2 "jul" 29.0 "aug" 22.4 "sep" 14.1 "oct" 5.5 "nov" -5.2 "dec" >/dev/null

# ─── Streams (unique to node 2) ─────────────────────────
$CLI2 XADD "stream:node2_log" "*" level INFO msg "Node 2 started" service "replica" >/dev/null
$CLI2 XADD "stream:node2_log" "*" level INFO msg "Sync complete" service "replica" >/dev/null

echo "[*] Generating big streams on node 2..."
generate_big_stream "$CLI2" "stream:float64_200"  100  "float64" "<200d" 200
generate_big_stream "$CLI2" "stream:uint16_500"   100  "uint16"  "<500H" 500
generate_big_stream "$CLI2" "stream:int8_1000"    100  "int8"    "<1000b" 1000
generate_big_stream "$CLI2" "stream:int32_200"    100  "int32"   "<200i" 200
generate_big_stream "$CLI2" "stream:uint32_200"   100  "uint32"  "<200I" 200

echo "[*] Generating large streams on node 2..."
generate_big_stream "$CLI2" "stream:large_i16_20k"  50  "int16"   "<20000h" 20000

# ─── COLLISION KEYS (exist on BOTH nodes) ────────────────
echo ""
echo "=== Creating collision keys (on both nodes) ==="
$CLI1 SET "shared:collision_test" "Value from Node 1 (port $REDIS_PORT_1)" >/dev/null
$CLI2 SET "shared:collision_test" "Value from Node 2 (port $REDIS_PORT_2)" >/dev/null
echo "  shared:collision_test"

$CLI1 HSET "shared:status" node "node1" status "primary" uptime "168h" >/dev/null
$CLI2 HSET "shared:status" node "node2" status "replica" uptime "720h" >/dev/null
echo "  shared:status"

$CLI1 SADD "shared:active_users" "alice" "charlie" "eve" >/dev/null
$CLI2 SADD "shared:active_users" "bob" "diana" "frank" >/dev/null
echo "  shared:active_users"

# ─── DB 1 on node 2 ──────────────────────────────────────
$CLI2 -n 1 SET "db1:node2_key" "This is in database 1 on node 2" >/dev/null
$CLI2 -n 1 HSET "db1:info" description "DB 1 on node 2" purpose "collision test" >/dev/null

# ─── Live stream devices ─────────────────────────────────
# Static fixtures cannot exercise the parts of the TUI that only exist while
# data is arriving: the [l] stream listener, the [i] ingestion rate view, and a
# plot that moves. These three simulate instrumentation writing waveforms
# continuously, at rates chosen to straddle what the main loop drains per tick
# (~400 entries/s), so the fast channel is deliberately past it.
DEVICE_SCRIPT="$(dirname "$0")/stream-device.py"
DEVICE_LOG="$(mktemp -t redis-tui-devices.XXXXXX.log)"
DEVICE_PIDS=()

stop_devices() {
    if [ ${#DEVICE_PIDS[@]} -gt 0 ]; then
        kill "${DEVICE_PIDS[@]}" 2>/dev/null || true
        wait "${DEVICE_PIDS[@]}" 2>/dev/null || true
    fi
}
# The TUI runs in the foreground, so this fires however it ends - quit, Ctrl-C
# or a crash. Without it the devices outlive the script and keep writing.
trap stop_devices EXIT INT TERM

start_device() { # name rate samples
    "$DEVICE_SCRIPT" --port "$REDIS_PORT_1" --stream "device:$1" \
        --rate "$2" --samples "$3" --maxlen 1000 >>"$DEVICE_LOG" 2>&1 &
    DEVICE_PIDS+=($!)
    echo "  device:$1  $2 entries/s  $3 float32 samples"
}

echo ""
echo "[*] Starting live stream devices on node 1..."
start_device slow 10 256
start_device medium 200 512
start_device fast 1200 1024
echo "  log: $DEVICE_LOG"

# ─── Summary ──────────────────────────────────────────────
echo ""
echo "=== Test Data Loaded ==="
echo "Node 1 (port $REDIS_PORT_1) keys in DB 0: $($CLI1 DBSIZE | tr -d '\r')"
echo "Node 2 (port $REDIS_PORT_2) keys in DB 0: $($CLI2 DBSIZE | tr -d '\r')"
echo "Collision keys: shared:collision_test, shared:status, shared:active_users"
echo "Live devices: device:slow, device:medium, device:fast (press [l] to listen, [i] for rate)"
echo ""
echo "=== Starting Redis TUI (multi-host) ==="
echo ""

cargo run -- --hosts "127.0.0.1:${REDIS_PORT_1}" "127.0.0.1:${REDIS_PORT_2}"
