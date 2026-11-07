import os, socket, sys
path = sys.argv[1]
s = socket.socket(socket.AF_UNIX); s.bind(path); os.chmod(path, 0o666); s.listen(4)
for _ in range(int(sys.argv[2])):
    c, _ = s.accept(); data = b""
    while b"\r\n\r\n" not in data: data += c.recv(4096)
    head = data.split(b"\r\n")[0].decode()
    body = ('{"principal":"agent","note":"stub","request_line":"%s"}' % head).encode()
    c.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n" % len(body) + body); c.close()
