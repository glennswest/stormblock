#!/usr/bin/env python3
"""A DNS server that knows a few names, for tests/initramfs-node-name.sh (#559).

The real `nslookup` the initramfs carries (busybox's) asks it, so the test
parses what that nslookup really prints, not what a stub says it prints.

    dns-stub.py <addr> <port> <ready-file> RECORD...

A RECORD is `PTR:<arpa name>=<name>` or `A:<name>=<ipv4>`. Anything else asked
is answered NXDOMAIN, except AAAA of a known name (no error, no answer), as a
server does for a name that has only an A record. The ready file is written
once the socket is bound.
"""
import socket
import struct
import sys


def read_name(msg, off):
    labels = []
    while True:
        n = msg[off]
        off += 1
        if n == 0:
            return ".".join(labels).lower(), off
        labels.append(msg[off:off + n].decode())
        off += n


def encode_name(name):
    out = b""
    for label in name.rstrip(".").split("."):
        out += bytes([len(label)]) + label.encode()
    return out + b"\x00"


def main():
    addr, port, ready = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    ptr, a = {}, {}
    for rec in sys.argv[4:]:
        kind, rest = rec.split(":", 1)
        key, value = rest.split("=", 1)
        (ptr if kind == "PTR" else a)[key.lower().rstrip(".")] = value
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((addr, port))
    open(ready, "w").close()
    while True:
        msg, peer = s.recvfrom(512)
        if len(msg) < 12:
            continue
        qid, _flags, qd = struct.unpack(">HHH", msg[:6])
        if qd != 1:
            continue
        qname, off = read_name(msg, 12)
        qtype, _qclass = struct.unpack(">HH", msg[off:off + 4])
        question = msg[12:off + 4]
        answers = b""
        rcode = 0
        if qtype == 12 and qname in ptr:
            rdata = encode_name(ptr[qname])
            answers = b"\xc0\x0c" + struct.pack(">HHIH", 12, 1, 300, len(rdata)) + rdata
        elif qtype == 1 and qname in a:
            rdata = socket.inet_aton(a[qname])
            answers = b"\xc0\x0c" + struct.pack(">HHIH", 1, 1, 300, len(rdata)) + rdata
        elif qname in a or qname in ptr:
            pass  # known name, no record of that type
        else:
            rcode = 3
        an = 1 if answers else 0
        header = struct.pack(">HHHHHH", qid, 0x8180 | rcode, 1, an, 0, 0)
        s.sendto(header + question + answers, peer)


if __name__ == "__main__":
    main()
