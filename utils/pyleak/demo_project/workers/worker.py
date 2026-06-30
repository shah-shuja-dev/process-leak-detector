import os

_seen = {}


def remember(key, value):
    _seen[key] = value


def shell_out(cmd):
    os.system(cmd)
