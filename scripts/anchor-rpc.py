#!/usr/bin/env python3
"""Tiny smoke-test client for the anchor RPC unix socket.

Usage:
    scripts/anchor-rpc.py hello
    scripts/anchor-rpc.py list
    scripts/anchor-rpc.py dismiss <notification-id>
    scripts/anchor-rpc.py subscribe        # streams events until Ctrl-C

Speaks newline-delimited JSON over the socket at
~/Library/Application Support/Helm/anchor.sock (override with $HELM_ANCHOR_SOCKET).
"""
import json
import os
import select
import socket
import sys


SOCKET_PATH = os.environ.get(
    "HELM_ANCHOR_SOCKET",
    os.path.expanduser("~/Library/Application Support/Helm/anchor.sock"),
)


def connect():
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCKET_PATH)
    return s


def send(s, op_dict, req_id=1):
    msg = {"kind": "request", "id": req_id, **op_dict}
    s.sendall((json.dumps(msg) + "\n").encode())


def recv_one(s, timeout=2.0):
    r, _, _ = select.select([s], [], [], timeout)
    if not r:
        return None
    data = s.recv(65536)
    if not data:
        return None
    # Strip trailing newline so the printed JSON is clean.
    return data.decode().rstrip("\n")


def stream(s):
    """Print every server message as it arrives. Used for subscribe."""
    buf = b""
    while True:
        chunk = s.recv(65536)
        if not chunk:
            return
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if line.strip():
                # Pretty-print so events are readable on a terminal.
                try:
                    obj = json.loads(line.decode())
                    print(json.dumps(obj, indent=2), flush=True)
                except json.JSONDecodeError:
                    print(line.decode(), flush=True)


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        sys.exit(1)
    cmd = argv[1]
    s = connect()
    if cmd == "hello":
        send(s, {"op": "hello"})
        print(recv_one(s))
    elif cmd == "list":
        send(s, {"op": "list_notifications"})
        print(recv_one(s))
    elif cmd == "dismiss":
        if len(argv) < 3:
            print("dismiss requires a notification id")
            sys.exit(1)
        send(s, {"op": "dismiss_notification", "notification_id": argv[2]})
        print(recv_one(s))
    elif cmd == "subscribe":
        send(s, {"op": "subscribe"})
        try:
            stream(s)
        except KeyboardInterrupt:
            pass
    else:
        print(f"unknown command: {cmd}")
        print(__doc__)
        sys.exit(1)


if __name__ == "__main__":
    main(sys.argv)
