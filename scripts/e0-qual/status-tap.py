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


class ConnectionLog:
    def __init__(self, out, connection):
        self.out = out
        self.connection = connection
        self.pending = 0

    def record(self, **fields):
        log(self.out, c=self.connection, **fields)

    def request(self, data):
        for match in REQUEST.finditer(data):
            self.record(method=match[1].decode(), path=match[2].decode())

    def response(self, data):
        for match in STATUS.finditer(data):
            self.pending = int(match[1])
            self.record(status=self.pending)
        if self.pending >= 400 and self.error_detail(data):
            self.pending = 0

    def error_detail(self, data):
        error = ERROR.search(data)
        message = MESSAGE.search(data)
        if error or message:
            self.record(error=error[1].decode() if error else None,
                        message=message[1].decode('ascii', 'replace') if message else None)
            return True
        _, separator, body = data.partition(b'\r\n\r\n')
        if separator:
            keys = sorted(set(k.decode() for k in KEY.findall(body)))[:12]
            if keys:
                enums = {k.decode(): v.decode() for k, v in ENUM.findall(body)}
                self.record(body_keys=keys, enums=enums)
                return True
        return False


def pump(source, sink, observe):
    try:
        while data := source.recv(65536):
            observe(data)
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
        connection = ConnectionLog(out, counter)
        threading.Thread(target=pump, args=(client, backend, connection.request), daemon=True).start()
        threading.Thread(target=pump, args=(backend, client, connection.response), daemon=True).start()


if __name__ == '__main__':
    main()
