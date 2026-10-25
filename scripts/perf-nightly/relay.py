#!/usr/bin/env python3
"""Latency relay for the positive control of latency (plan §13, slice 2).

`relay.py --delay-ms N --framing lsp|lines -- <server argv>` starts the server
and copies the client's stdin to it unchanged, while every message the server
writes reaches the client N ms late. A slower server is exactly what the
control must make the verdict catch; the relay carries the run marker like any
other process of the run, so the stop takes it down by PID.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import threading
import time


def read_lsp(stream):
    headers = b""
    while not headers.endswith(b"\r\n\r\n"):
        byte = stream.read(1)
        if not byte:
            return None
        headers += byte
    length = 0
    for line in headers.decode("ascii", "replace").split("\r\n"):
        name, _, value = line.partition(":")
        if name.strip().lower() == "content-length":
            length = int(value.strip())
    return headers + stream.read(length)


def read_line(stream):
    return stream.readline() or None


def main(argv=None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--delay-ms", type=int, required=True)
    parser.add_argument("--framing", choices=("lsp", "lines"), required=True)
    parser.add_argument("server", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    server = args.server[1:] if args.server[:1] == ["--"] else args.server
    proc = subprocess.Popen(server, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    read = read_lsp if args.framing == "lsp" else read_line
    delay = args.delay_ms / 1000.0

    def upstream():
        try:
            while True:
                chunk = os.read(sys.stdin.fileno(), 1 << 16)
                if not chunk:
                    break
                proc.stdin.write(chunk)
                proc.stdin.flush()
        except OSError:
            pass
        finally:
            try:
                proc.stdin.close()
            except OSError:
                pass

    threading.Thread(target=upstream, daemon=True).start()
    out = sys.stdout.buffer
    while True:
        message = read(proc.stdout)
        if message is None:
            break
        if delay:
            # The injected latency itself, not a wait for an event.
            time.sleep(delay)
        try:
            out.write(message)
            out.flush()
        except BrokenPipeError:
            break
    return proc.wait()


if __name__ == "__main__":
    sys.exit(main())
