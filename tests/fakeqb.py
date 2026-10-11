#!/usr/bin/env python3
"""Fake qBittorrent WebAPI for pbh-rs smoke testing (stdlib only).

Endpoints implemented (subset used by pbh-rs QBittorrent adapter):
  POST /api/v2/auth/login          -> "Ok." + SID cookie
  GET  /api/v2/app/buildInfo       -> {"libtorrent":"2.0.10", ...}
  GET  /api/v2/app/version         -> "v5.1.0"
  POST /api/v2/app/setPreferences  -> records banned_IPs / prefs
  GET  /api/v2/torrents/info       -> paginated torrent list (1 active torrent)
  GET  /api/v2/torrents/properties -> is_private/piece info
  GET  /api/v2/sync/torrentPeers   -> peers dict with good + bad peers
  POST /api/v2/transfer/banPeers   -> records increment bans
  GET  /api/v2/sync/maindata       -> alltime stats
  GET  /state                      -> test introspection (what prefs/bans were received)
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

TORRENT_HASH = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

TORRENT = {
    "hash": TORRENT_HASH,
    "name": "Ubuntu 24.04 LTS",
    "size": 5_000_000_000,
    "progress": 1.0,
    "dlspeed": 0,
    "upspeed": 1024,
    "completed": 5_000_000_000,
    "is_private": False,
    "piece_size": 4194304,
    "pieces_have": 1192,
    "state": "uploading",
}

PROPERTIES = {
    "save_path": "/downloads/",
    "piece_size": 4194304,
    "pieces_have": 1192,
    "pieces_num": 1192,
    "private": False,
    "is_private": False,
    "completed": 5_000_000_000,
}

def peer(client, peer_id_client, ip, port, dl=0, up=0, downloaded=0, uploaded=0, progress=0.5, flags="d u e p"):
    return {
        "client": client,
        "connection": "BT",
        "dl_speed": dl,
        "downloaded": downloaded,
        "up_speed": up,
        "uploaded": uploaded,
        "flags": flags,
        "ip": ip,
        "peer_id_client": peer_id_client,
        "port": port,
        "progress": progress,
    }

PEERS = {
    # 正常 peer：qBittorrent，有速度（非 handshaking）
    "203.0.113.10:5000": peer("qBittorrent/5.1.0", "-qB5100-goodpeer01", "203.0.113.10", 5000, dl=1000, up=2000, progress=0.8),
    # 恶意 PeerID：-hp 开头（profile 默认黑名单 STARTS_WITH -hp）
    "198.51.100.20:6000": peer("hp/torrent", "-hp0001xunleivariant", "198.51.100.20", 6000, dl=5000, up=0, progress=0.01),
    # 恶意 ClientName：包含 xunlei 变体（默认黑名单 STARTS_WITH hp/torrent）
    "198.51.100.30:6001": peer("hp/torrent 1.2.3", "-uD1234someclient", "198.51.100.30", 6001, dl=3000, up=0, progress=0.02),
    # 内网地址：应命中 ignore-peers-from-addresses → SKIP
    "192.168.1.50:7000": peer("hp/torrent", "-hp0002lanpeer00", "192.168.1.50", 7000, dl=100, up=0, progress=0.5),
    # handshaking（速度全 0）且无 peerid/client → 各模块返回 handshaking/pass，不封
    "203.0.113.99:8000": peer("", "", "203.0.113.99", 8000, dl=0, up=0, progress=0.0),
    # HTTP 连接类型 → 适配器应跳过
    "203.0.113.77:9000": {**peer("WebSeed", "", "203.0.113.77", 9000), "connection": "HTTP"},
}

STATE = {
    "prefs": [],          # list of json strings posted to setPreferences
    "ban_peers": [],      # list of peers= payloads posted to transfer/banPeers
    "login_count": 0,
}
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass  # quiet

    def _send(self, code, body, content_type="application/json", extra_headers=None):
        data = body if isinstance(body, bytes) else body.encode()
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        for k, v in (extra_headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def _read_form(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length).decode() if length else ""
        return parse_qs(raw)

    def do_GET(self):
        u = urlparse(self.path)
        p = u.path
        if p == "/api/v2/app/buildInfo":
            self._send(200, json.dumps({"bitness": 64, "libtorrent": "2.0.10", "qt": "6.7.0", "platform": "test"}))
        elif p == "/api/v2/app/version":
            self._send(200, "v5.1.0", content_type="text/plain")
        elif p == "/api/v2/torrents/info":
            q = parse_qs(u.query)
            offset = int(q.get("offset", ["0"])[0])
            limit = int(q.get("limit", ["100"])[0])
            all_t = [TORRENT]
            self._send(200, json.dumps(all_t[offset:offset + limit]))
        elif p == "/api/v2/torrents/properties":
            self._send(200, json.dumps(PROPERTIES))
        elif p == "/api/v2/sync/torrentPeers":
            self._send(200, json.dumps({"rid": 1, "full_update": True, "peers": PEERS}))
        elif p == "/api/v2/sync/maindata":
            self._send(200, json.dumps({"rid": 1, "server_state": {"alltime_ul": 123456, "alltime_dl": 654321}}))
        elif p == "/state":
            with LOCK:
                self._send(200, json.dumps(STATE))
        else:
            self._send(404, json.dumps({"error": "not found"}))

    def do_POST(self):
        u = urlparse(self.path)
        p = u.path
        # --- 测试控制端点 ---
        if p == "/control/setpeers":
            length = int(self.headers.get("Content-Length", 0))
            payload = json.loads(self.rfile.read(length).decode()) if length else {}
            with LOCK:
                PEERS.clear()
                PEERS.update(payload)
            self._send(200, json.dumps({"ok": True, "count": len(payload)}))
            return
        if p == "/control/mutate":
            length = int(self.headers.get("Content-Length", 0))
            payload = json.loads(self.rfile.read(length).decode()) if length else {}
            ip = payload.get("ip")
            with LOCK:
                for k, v in PEERS.items():
                    if v.get("ip") == ip:
                        for field in ("progress", "uploaded", "downloaded", "up_speed", "dl_speed", "client", "peer_id_client"):
                            if field in payload:
                                v[field] = payload[field]
            self._send(200, json.dumps({"ok": True}))
            return
        form = self._read_form()
        if p == "/api/v2/auth/login":
            with LOCK:
                STATE["login_count"] += 1
            user = form.get("username", [""])[0]
            pwd = form.get("password", [""])[0]
            if user == "admin" and pwd == "adminadmin":
                self._send(200, "Ok.", content_type="text/plain",
                           extra_headers={"Set-Cookie": "SID=fakesessionid123; path=/"})
            else:
                self._send(200, "Fails.", content_type="text/plain")
        elif p == "/api/v2/app/setPreferences":
            payload = form.get("json", [""])[0]
            with LOCK:
                STATE["prefs"].append(payload)
            self._send(200, b"", content_type="text/plain")
        elif p == "/api/v2/transfer/banPeers":
            payload = form.get("peers", [""])[0]
            with LOCK:
                STATE["ban_peers"].append(payload)
            self._send(200, b"", content_type="text/plain")
        else:
            self._send(404, json.dumps({"error": "not found"}))


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"fakeqb listening on {port}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
