#!/usr/bin/env python3
"""Loopback TCP passthrough that logs only HTTP request lines and response codes.

Never logs headers or bodies; for 4xx/5xx it records the public ApiError
`error` code and bounded `message`, or else only the body's top-level key names. Usage: status-tap.py LISTEN_PORT UPSTREAM_PORT LOG
"""
import json
import re
import socket
import sys
import threading
import time

REQUEST = re.compile(rb'(?:^|\r\n)(GET|POST|PUT|PATCH|DELETE) (/[!-~]{0,200}) HTTP/1\.1\r\n')
STATUS = re.compile(rb'(?:^|\r\n)HTTP/1\.1 ([1-5]\d\d) ')
ERROR = re.compile(rb'"error":"([A-Za-z_]{1,64})"')
# ApiError messages are public server strings; bounded and printable only.
MESSAGE = re.compile(rb'"message":"([ -!#-~]{1,200})')
KEY = re.compile(rb'"([a-z_]{1,40})":')
# Typed control refusals: only the enum-valued kind/code/reason fields.
ENUM = re.compile(rb'"(kind|code|reason)":"([a-z_.-]{1,80})"')
lock = threading.Lock()


def log(out, **record):
    record['t'] = round(time.time(), 3)
    with lock:
        out.write(json.dumps(record) + '\n')
        out.flush()


def pump(source, sink, out, connection, direction):
    pending = None
    try:
        while True:
            data = source.recv(65536)
            if not data:
                break
            if direction == 'request':
                for match in REQUEST.finditer(data):
                    log(out, c=connection, method=match[1].decode(), path=match[2].decode())
            else:
                for match in STATUS.finditer(data):
                    pending = int(match[1])
                    log(out, c=connection, status=pending)
                if pending is not None and pending >= 400:
                    error = ERROR.search(data)
                    message = MESSAGE.search(data)
                    if error or message:
                        log(out, c=connection, error=error[1].decode() if error else None,
                            message=message[1].decode('ascii', 'replace') if message else None)
                        pending = None
                    elif b'\r\n\r\n' in data:
                        body = data.split(b'\r\n\r\n', 1)[1]
                        keys = sorted(set(k.decode() for k in KEY.findall(body)))[:12]
                        if keys:
                            enums = {k.decode(): v.decode() for k, v in ENUM.findall(body)}
                            log(out, c=connection, body_keys=keys, enums=enums)
                            pending = None
            sink.sendall(data)
    except OSError:
        pass
    finally:
        for end in (source, sink):
            try:
                end.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def main():
    listen, upstream, path = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
    out = open(path, 'a', buffering=1)
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('127.0.0.1', listen))
    server.listen(64)
    counter = 0
    while True:
        client, _ = server.accept()
        counter += 1
        backend = socket.create_connection(('127.0.0.1', upstream))
        threading.Thread(target=pump, args=(client, backend, out, counter, 'request'), daemon=True).start()
        threading.Thread(target=pump, args=(backend, client, out, counter, 'response'), daemon=True).start()


if __name__ == '__main__':
    main()
