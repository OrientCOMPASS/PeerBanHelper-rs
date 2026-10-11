#!/usr/bin/env python3
"""Minimal BitTorrent helpers for real-downloader e2e tests (stdlib only).

- bencode encode/decode
- .torrent file creation (single file)
- FakePeer: raw TCP BitTorrent peer performing a real handshake with a custom
  peer_id, then acting as a minimal LEECH: sends interested, requests blocks
  upon unchoke, so the torrent counts as *active* in qBittorrent and the peer
  shows realistic flags/speeds (this is what a real吸血 peer looks like).
"""
import hashlib
import os
import socket
import struct
import threading
import time

# ---------------------------------------------------------------- bencode

def bencode(obj):
    if isinstance(obj, int):
        return b"i%de" % obj
    if isinstance(obj, bytes):
        return b"%d:%s" % (len(obj), obj)
    if isinstance(obj, str):
        return bencode(obj.encode("utf-8", "surrogateescape"))
    if isinstance(obj, list):
        return b"l" + b"".join(bencode(x) for x in obj) + b"e"
    if isinstance(obj, dict):
        items = sorted(obj.items(), key=lambda kv: _to_bytes(kv[0]))
        out = b"d"
        for k, v in items:
            out += bencode(_to_bytes(k)) + bencode(v)
        return out + b"e"
    raise TypeError(f"cannot bencode {type(obj)}")

def _to_bytes(k):
    return k if isinstance(k, bytes) else k.encode("utf-8", "surrogateescape")

def bdecode(data):
    val, _ = _bdecode(data, 0)
    return val

def _bdecode(data, i):
    c = data[i:i + 1]
    if c == b"i":
        j = data.index(b"e", i)
        return int(data[i + 1:j]), j + 1
    if c == b"l":
        i += 1
        out = []
        while data[i:i + 1] != b"e":
            v, i = _bdecode(data, i)
            out.append(v)
        return out, i + 1
    if c == b"d":
        i += 1
        out = {}
        while data[i:i + 1] != b"e":
            k, i = _bdecode(data, i)
            v, i = _bdecode(data, i)
            out[k] = v
        return out, i + 1
    if c.isdigit():
        j = data.index(b":", i)
        n = int(data[i:j])
        s = j + 1
        return data[s:s + n], s + n
    raise ValueError(f"bad bencode at {i}: {c!r}")

# ---------------------------------------------------------------- torrent

def make_torrent(file_path, announce="http://127.0.0.1:9/announce",
                 piece_length=262144, comment="pbh-rs real e2e test torrent"):
    size = os.path.getsize(file_path)
    pieces = bytearray()
    with open(file_path, "rb") as f:
        while True:
            chunk = f.read(piece_length)
            if not chunk:
                break
            pieces += hashlib.sha1(chunk).digest()
    info = {
        b"name": os.path.basename(file_path).encode(),
        b"piece length": piece_length,
        b"pieces": bytes(pieces),
        b"length": size,
    }
    meta = {
        b"announce": announce.encode(),
        b"creation date": int(time.time()),
        b"created by": b"pbh-rs-test",
        b"comment": comment.encode(),
        b"info": info,
    }
    data = bencode(meta)
    infohash = hashlib.sha1(bencode(info)).digest()
    return data, infohash

# ---------------------------------------------------------------- raw leech peer

HANDSHAKE_PSTR = b"\x13BitTorrent protocol"
MSG_INTERESTED = b"\x00\x00\x00\x01\x02"
MSG_UNCHOKE_ID = 1
MSG_BITFIELD_ID = 5
MSG_REQUEST_ID = 6
MSG_PIECE_ID = 7
BLOCK = 16384


def _msg(mid, payload=b""):
    return struct.pack(">I", 1 + len(payload)) + bytes([mid]) + payload


class FakePeer(threading.Thread):
    """Completes a real BT handshake and leeches data (interested + requests)."""

    def __init__(self, host, port, infohash: bytes, peer_id: bytes,
                 piece_length: int = 262144, total_size: int = 0, leech: bool = True):
        super().__init__(daemon=True)
        assert len(infohash) == 20 and len(peer_id) == 20
        self.host, self.port = host, port
        self.infohash, self.peer_id = infohash, peer_id
        self.piece_length = piece_length
        self.total_size = total_size
        self.leech = leech
        self.connected = threading.Event()
        self.closed = threading.Event()
        self.error = None
        self.downloaded = 0
        self.stop_evt = threading.Event()
        self._sock = None

    # -- connection with retry (torrent may still be rechecking) --
    def run(self):
        for _ in range(10):
            if self.stop_evt.is_set():
                return
            if self._handshake_once():
                break
            time.sleep(2)
        else:
            self.closed.set()
            return
        self._session()

    def _handshake_once(self):
        try:
            s = socket.create_connection((self.host, self.port), timeout=10)
            self._sock = s
            s.sendall(HANDSHAKE_PSTR + b"\x00" * 8 + self.infohash + self.peer_id)
            buf = b""
            s.settimeout(10)
            while len(buf) < 68:
                chunk = s.recv(4096)
                if not chunk:
                    return False
                buf += chunk
            if not buf.startswith(HANDSHAKE_PSTR):
                self.error = f"bad handshake response: {buf[:20]!r}"
                return False
            self.connected.set()
            return True
        except OSError as e:
            self.error = repr(e)
            return False

    # -- minimal leech session --
    def _session(self):
        s = self._sock
        try:
            if self.leech:
                s.sendall(MSG_INTERESTED)
            s.settimeout(1.0)
            buf = b""
            unchoked = False
            next_index, next_begin = 0, 0
            blocks_per_piece = max(1, self.piece_length // BLOCK)
            last_req = 0.0
            while not self.stop_evt.is_set():
                try:
                    data = s.recv(65536)
                    if not data:
                        break
                    buf += data
                except socket.timeout:
                    pass
                except OSError:
                    break
                # parse messages
                while len(buf) >= 4:
                    (ln,) = struct.unpack(">I", buf[:4])
                    if ln == 0:  # keepalive
                        buf = buf[4:]
                        continue
                    if len(buf) < 4 + ln:
                        break
                    payload = buf[4:4 + ln]
                    buf = buf[4 + ln:]
                    mid = payload[0]
                    if mid == MSG_UNCHOKE_ID:
                        unchoked = True
                    elif mid == MSG_PIECE_ID and len(payload) >= 9:
                        block = payload[9:]
                        self.downloaded += len(block)
                        # request next block
                        next_begin += BLOCK
                        if next_begin >= self.piece_length:
                            next_index += 1
                            next_begin = 0
                        if self.leech and self.total_size and next_index * self.piece_length >= self.total_size:
                            next_index, next_begin = 0, 0
                        try:
                            s.sendall(_msg(MSG_REQUEST_ID,
                                           struct.pack(">II", next_index, next_begin) + struct.pack(">I", BLOCK)))
                        except OSError:
                            return
                # first request after unchoke (or retry slowly)
                if unchoked and self.leech and time.time() - last_req > 2:
                    try:
                        s.sendall(_msg(MSG_REQUEST_ID,
                                       struct.pack(">II", next_index, next_begin) + struct.pack(">I", BLOCK)))
                        last_req = time.time()
                    except OSError:
                        break
                if not unchoked and self.leech and time.time() - last_req > 5:
                    try:
                        s.sendall(MSG_INTERESTED)
                        last_req = time.time()
                    except OSError:
                        break
        except Exception as e:  # noqa: BLE001
            self.error = repr(e)
        finally:
            self.closed.set()
            try:
                s.close()
            except OSError:
                pass

    def stop(self):
        self.stop_evt.set()
        try:
            if self._sock:
                self._sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    @property
    def is_alive_conn(self):
        return self.connected.is_set() and not self.closed.is_set()


def try_handshake(host, port, infohash: bytes, peer_id: bytes, timeout=6):
    """Attempt a handshake; return True if the remote completed it, False if
    the connection was refused/closed (e.g. banned IP)."""
    try:
        s = socket.create_connection((host, port), timeout=timeout)
    except OSError:
        return False
    try:
        s.settimeout(timeout)
        s.sendall(HANDSHAKE_PSTR + b"\x00" * 8 + infohash + peer_id)
        buf = b""
        while len(buf) < 68:
            chunk = s.recv(4096)
            if not chunk:
                return False
            buf += chunk
        return buf.startswith(HANDSHAKE_PSTR)
    except OSError:
        return False
    finally:
        try:
            s.close()
        except OSError:
            pass


def random_payload(path, size=4 * 1024 * 1024):
    with open(path, "wb") as f:
        f.write(os.urandom(size))
    return path
