import subprocess
import threading
import functools

_cache_results = []
_event_listeners = []


@functools.lru_cache()
def expensive_compute(x):
    return x * x


def register_listener(cb):
    _event_listeners.append(cb)


def process_batch(items):
    for item in items:
        _cache_results.append(item)
        t = threading.Thread(target=do_work, args=(item,))
        t.start()


def do_work(item):
    f = open("/tmp/output.log", "a")
    f.write(str(item))


def run_worker():
    subprocess.run(["python3", "workers/worker.py"])


class Node:
    def __init__(self, parent):
        self.parent = parent

    def __del__(self):
        pass
