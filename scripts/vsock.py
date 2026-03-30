#!/usr/bin/env python3
import os
import socket
import sys
import termios
import tty

vmname = sys.argv[1]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(f"/run/odorobo/vms/{vmname}/vsock.sock")
sock.sendall(b"CONNECT 1234\n")

# read the OK response
resp = b""
while b"\n" not in resp:
    resp += sock.recv(1)

# put terminal in raw mode
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
tty.setraw(fd)

try:
    import select

    while True:
        r, _, _ = select.select([sock, sys.stdin], [], [])
        if sock in r:
            data = sock.recv(1024)
            if not data:
                break
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
        if sys.stdin in r:
            data = os.read(fd, 1024)
            if not data:
                break
            sock.sendall(data)
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
