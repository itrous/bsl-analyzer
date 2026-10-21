"""Clocks for the harness: every wait ends on an event or on its own timeout.

The harness never sleeps blindly. A wait is `clock.wait(ready, timeout, cond,
key)`: it returns as soon as `ready()` holds (re-checked whenever `cond` is
notified) or reports a timeout. Tests swap `RealClock` for a fake one whose
timeouts are decided by the scripted server, so no test depends on CPU speed.
"""

from __future__ import annotations

import select
import threading
import time


class RealClock:
    def now(self) -> float:
        return time.monotonic()

    def sleep(self, seconds: float) -> None:
        time.sleep(seconds)

    def wait(self, ready, timeout: float, cond: threading.Condition, key=None) -> bool:
        """True once `ready()` holds; False when `timeout` seconds pass first."""
        deadline = time.monotonic() + timeout
        with cond:
            while not ready():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                cond.wait(remaining)
        return True

    def wait_exit(self, pidfds: dict, timeout: float) -> bool:
        """True once every process in `pidfds` ({pid: pidfd}) has exited."""
        deadline = time.monotonic() + timeout
        pending = list(pidfds.values())
        while pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            poller = select.poll()
            for fd in pending:
                poller.register(fd, select.POLLIN)
            done = {fd for fd, _ in poller.poll(remaining * 1000)}
            pending = [fd for fd in pending if fd not in done]
        return True
