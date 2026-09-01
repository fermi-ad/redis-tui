#!/usr/bin/env python3
"""A simulated instrumentation device that streams a waveform into Redis.

Writes one XADD per entry at a target rate, each carrying a float32 waveform
blob in a `_`-prefixed field, which is what redis-tui decodes as binary plot
data. The stream is trimmed as it goes, so it can run indefinitely.

Speaks RESP over a plain socket rather than using the `redis` package: it is
not guaranteed to be installed, and shelling out to redis-cli once per entry
cannot hold the rates this needs to produce. That keeps the dependency list
exactly what the README already claims - redis-server, redis-cli, python3.

Usage:
    ./stream-device.py --stream device:fast --rate 1200 --samples 1024
"""

import argparse
import math
import random
import signal
import socket
import struct
import sys
import time

CRLF = b"\r\n"


class Resp:
    """The little of RESP this needs: send commands, read replies, report errors."""

    def __init__(self, host, port, timeout=5.0):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = b""

    @staticmethod
    def encode(*args):
        out = [b"*%d\r\n" % len(args)]
        for a in args:
            if isinstance(a, str):
                a = a.encode()
            elif not isinstance(a, bytes):
                a = str(a).encode()
            out.append(b"$%d\r\n" % len(a))
            out.append(a)
            out.append(CRLF)
        return b"".join(out)

    def send(self, payload):
        self.sock.sendall(payload)

    def _line(self):
        while CRLF not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("redis closed the connection")
            self.buf += chunk
        line, self.buf = self.buf.split(CRLF, 1)
        return line

    def _read_exactly(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("redis closed the connection")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def reply(self):
        line = self._line()
        kind, rest = line[:1], line[1:]
        if kind in (b"+", b":"):
            return rest
        if kind == b"-":
            raise RuntimeError(rest.decode(errors="replace"))
        if kind == b"$":
            n = int(rest)
            if n == -1:
                return None
            out = self._read_exactly(n)
            self._read_exactly(2)  # trailing CRLF
            return out
        if kind == b"*":
            n = int(rest)
            return None if n == -1 else [self.reply() for _ in range(n)]
        raise RuntimeError("unexpected RESP reply: %r" % line[:40])


def waveform(phase, step, samples, amp, noise):
    """`samples` float32s of a multi-frequency sine, continuing from `phase`.

    Phase carries across entries so the signal is continuous rather than
    restarting each entry, which would put a discontinuity into every window
    and show up in the FFT view as an artifact rather than a signal.
    """
    vals = []
    for _ in range(samples):
        v = amp * (
            math.sin(phase) + 0.5 * math.sin(3.0 * phase) + 0.25 * math.sin(7.0 * phase)
        )
        if noise:
            v += random.gauss(0.0, noise)
        vals.append(v)
        phase += step
    return struct.pack("<%df" % samples, *vals), phase % (2.0 * math.pi)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=6379)
    p.add_argument("--stream", default="device:waveform")
    p.add_argument("--rate", type=float, default=100.0, help="entries per second")
    p.add_argument("--samples", type=int, default=512, help="float32s per entry")
    p.add_argument("--freq", type=float, default=3.0, help="cycles per entry")
    p.add_argument("--amp", type=float, default=1.0)
    p.add_argument("--noise", type=float, default=0.02)
    p.add_argument("--maxlen", type=int, default=1000, help="XTRIM MAXLEN ~ bound")
    p.add_argument("--source", default="device")
    args = p.parse_args()

    if args.rate <= 0 or args.samples <= 0:
        p.error("--rate and --samples must be positive")

    running = [True]

    def stop(_sig, _frm):
        running[0] = False

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)

    conn = Resp(args.host, args.port)
    interval = 1.0 / args.rate
    # Flush roughly every 10ms. One syscall per entry cannot keep up at the top
    # of the range, and a whole second of entries per flush would arrive as one
    # burst and misrepresent the ingestion rate it exists to exercise.
    batch = max(1, min(64, int(round(args.rate / 100.0))))
    phase = 0.0
    step = 2.0 * math.pi * args.freq / args.samples

    sys.stderr.write(
        "device %s -> %s:%d  %.0f entries/s  %d samples  maxlen %d (batch %d)\n"
        % (args.stream, args.host, args.port, args.rate, args.samples, args.maxlen, batch)
    )
    sys.stderr.flush()

    sent = 0
    started = time.monotonic()
    try:
        while running[0]:
            payload = []
            for _ in range(batch):
                blob, phase = waveform(phase, step, args.samples, args.amp, args.noise)
                payload.append(
                    Resp.encode("XADD", args.stream, "*", "source", args.source, "_", blob)
                )
            payload.append(
                Resp.encode("XTRIM", args.stream, "MAXLEN", "~", str(args.maxlen))
            )
            conn.send(b"".join(payload))
            for _ in range(batch + 1):
                conn.reply()

            sent += batch
            # Schedule against the start, not the last send, so a slow batch is
            # made up rather than compounding into permanent drift.
            due = started + sent * interval
            slack = due - time.monotonic()
            if slack > 0:
                time.sleep(slack)
    except (ConnectionError, RuntimeError, OSError) as e:
        sys.stderr.write("device %s stopped: %s\n" % (args.stream, e))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
