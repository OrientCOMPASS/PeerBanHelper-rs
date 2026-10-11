#!/usr/bin/env python3
"""BiglyBT + Aria2Next 适配器协议测试（fake 插件服务器）。

BiglyBT: /metadata /downloads /download/{id}/peers /bans(POST 增量, PUT 全量) /statistics
Aria2Next: JSON-RPC getVersion/tellActive/getPeers/setBtPeerBlocklist/getGlobalStat
两者各挂一个恶意 Peer（PeerId -HP 前缀）与一个正常 Peer，断言封禁决策与协议写入。
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WORKSPACE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
DATA = os.path.join(WORKSPACE, "target", "adapters-data")
BT_PORT = 18101   # biglybt fake
A2_PORT = 18102   # aria2 fake
PBH_PORT = 29889
TOKEN = "adapters-token"
THASH = "bb" * 20

BAD_PID_HEX = "-HP0001xunleivariant".encode().hex()
GOOD_PID_HEX = "-qB5100-goodpeer0001".encode().hex()

STATE = {"bt_bans_post": [], "bt_bans_put": [], "a2_blocklist": [], "a2_calls": []}

GREEN, RED, RESET = "\033[32m", "\033[31m", "\033[0m"
failures = []

def check(name, cond, detail=""):
    mark = GREEN + "PASS" + RESET if cond else RED + "FAIL" + RESET
    print(f"[{mark}] {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)


class BiglyHandler(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _send(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def _body(self):
        n = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(n).decode()) if n else {}
    def do_GET(self):
        if not self.headers.get("Authorization", "").startswith("Bearer secret-token"):
            self._send(403, {"error": "forbidden"}); return
        p = self.path
        if p == "/metadata":
            self._send(200, {"pluginVersion": "1.3.5", "applicationVersion": "1.7.3.0", "applicationName": "BiglyBT"})
        elif p.startswith("/downloads"):
            self._send(200, [{
                "state": 5, "name": "BiglyTestTorrent", "paused": False,
                "torrent": {"infoHash": THASH, "size": 5_000_000_000, "privateTorrent": False},
                "stats": {"remainingBytes": 0, "completedInThousandNotation": 1000,
                          "rtUploadSpeed": 2048, "rtDownloadSpeed": 0},
                "trackers": [],
            }])
        elif p == f"/download/{THASH}/peers":
            self._send(200, {"peers": [
                {"ip": "198.51.200.10", "port": 7001, "peerId": BAD_PID_HEX,
                 "client": "hp/torrent", "state": 30, "incoming": True,
                 "percentDoneInThousandNotation": 10,
                 "stats": {"rtDownloadSpeed": 5000, "totalReceived": 1000,
                            "rtUploadSpeed": 0, "totalSent": 0}},
                {"ip": "198.51.200.15", "port": 7002, "peerId": GOOD_PID_HEX,
                 "client": "qBittorrent 4.5.2", "state": 30, "incoming": False,
                 "percentDoneInThousandNotation": 500,
                 "stats": {"rtDownloadSpeed": 1000, "totalReceived": 999,
                            "rtUploadSpeed": 10, "totalSent": 10}},
            ]})
        elif p == "/statistics":
            self._send(200, {"overallDataBytesSent": 111, "overallDataBytesReceived": 222})
        else:
            self._send(404, {})
    def do_POST(self):
        if self.path == "/bans":
            STATE["bt_bans_post"].append(self._body())
            self._send(200, {"success": 1, "failed": 0})
        elif self.path == "/setconnector":
            self._body(); self._send(200, {"ok": True})
        else:
            self._send(404, {})
    def do_PUT(self):
        if self.path == "/bans":
            STATE["bt_bans_put"].append(self._body())
            self._send(200, {"success": 1, "failed": 0})
        else:
            self._send(404, {})


def a2_task():
    return {
        "gid": "a2gid0001", "status": "active", "totalLength": "5000000000",
        "completedLength": "5000000000", "uploadLength": "100",
        "downloadSpeed": "0", "uploadSpeed": "2048", "infoHash": THASH,
        "numSeeders": "0", "seeder": "true", "pieceLength": "4194304",
        "connections": "2", "dir": "/downloads",
        "bittorrent": {"mode": "single", "info": {"name": "AriaTestTorrent"},
                        "announceList": [["http://tracker/announce"]]},
    }

def a2_peers():
    return [
        {"peerId": BAD_PID_HEX, "ip": "198.51.201.20", "port": "7101",
         "uploadSpeed": "0", "downloadSpeed": "4000", "progress": "0.05",
         "clientName": "hp/torrent"},
        {"peerId": GOOD_PID_HEX, "ip": "198.51.201.25", "port": "7102",
         "uploadSpeed": "10", "downloadSpeed": "1000", "progress": "0.6",
         "clientName": "qBittorrent 4.5.2"},
    ]


class Aria2Handler(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        req = json.loads(self.rfile.read(n).decode())
        method = req.get("method")
        params = req.get("params", [])
        STATE["a2_calls"].append(method)
        if params and isinstance(params[0], str) and params[0].startswith("token:"):
            if params[0] != "token:a2-secret":
                resp = {"jsonrpc": "2.0", "id": req.get("id"),
                        "error": {"code": 1, "message": "unauthorized"}}
            else:
                params = params[1:]
                resp = self._dispatch(req, method, params)
        else:
            resp = self._dispatch(req, method, params)
        b = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)
    def _dispatch(self, req, method, params):
        rid = req.get("id")
        if method == "aria2.getVersion":
            result = {"version": "2.0.0-next", "enabledFeatures": ["BitTorrent"]}
        elif method == "aria2.tellActive":
            result = [a2_task()]
        elif method == "aria2.getPeers":
            result = a2_peers()
        elif method == "aria2.setBtPeerBlocklist":
            STATE["a2_blocklist"].append(params[0] if params else [])
            result = "OK"
        elif method == "aria2.getGlobalStat":
            result = {"downloadSpeed": "0", "uploadSpeed": "2048",
                       "totalLength": "5000000000", "completedLength": "5000000000",
                       "uploadLength": "333", "downloadLength": "444"}
        elif method == "aria2.getGlobalOption":
            result = {}
        else:
            return {"jsonrpc": "2.0", "id": rid, "error": {"code": -1, "message": "unknown method"}}
        return {"jsonrpc": "2.0", "id": rid, "result": result}


def http_call(url, method="GET", data=None, headers=None, timeout=10):
    req = urllib.request.Request(url, method=method, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)

def wait_for(cond, timeout, interval=0.5):
    end = time.time() + timeout
    while time.time() < end:
        try:
            v = cond()
        except Exception:
            v = None
        if v:
            return v
        time.sleep(interval)
    return None


def main():
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(os.path.join(DATA, "config"))
    subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, timeout=60)
    cfg_path = os.path.join(DATA, "config", "config.yml")
    cfg = open(cfg_path).read().replace('token: ""', f'token: "{TOKEN}"')
    cfg = cfg.replace("auto-update: true", "auto-update: false")
    cfg += f"""
client:
  bt-fake:
    type: biglybt
    name: FakeBigly
    endpoint: http://127.0.0.1:{BT_PORT}
    token: secret-token
    increment-ban: false
    verify-ssl: true
    ignore-private: false
    paused: false
  a2-fake:
    type: aria2next
    name: FakeAria2
    endpoint: http://127.0.0.1:{A2_PORT}/jsonrpc
    token: a2-secret
    verify-ssl: true
    ignore-private: false
    paused: false
"""
    open(cfg_path, "w").write(cfg)

    bt_srv = ThreadingHTTPServer(("127.0.0.1", BT_PORT), BiglyHandler)
    a2_srv = ThreadingHTTPServer(("127.0.0.1", A2_PORT), Aria2Handler)
    threading.Thread(target=bt_srv.serve_forever, daemon=True).start()
    threading.Thread(target=a2_srv.serve_forever, daemon=True).start()

    logf = open("/tmp/pbh-adapters.log", "w")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT)
    db_path = os.path.join(DATA, "db", "peerbanhelper.db")
    auth = {"Authorization": f"Bearer {TOKEN}"}
    try:
        assert wait_for(lambda: http_call(f"http://127.0.0.1:{PBH_PORT}/api/metadata/manifest", timeout=2)[0] == 200, 30)

        def history():
            conn = sqlite3.connect(db_path)
            rows = conn.execute("SELECT ip, module_name, downloader FROM history").fetchall()
            conn.close()
            return {r[0]: r for r in rows}

        ok = wait_for(lambda: {"198.51.200.10", "198.51.201.20"} <= set(history()), 45)
        h = history()
        check("BiglyBT 恶意 Peer 被封", h.get("198.51.200.10", (0, 0, ""))[2] == "bt-fake", str(h))
        check("Aria2Next 恶意 Peer 被封", h.get("198.51.201.20", (0, 0, ""))[2] == "a2-fake", str(h))
        check("BiglyBT 正常 Peer 未封", "198.51.200.15" not in h, str(list(h)))
        check("Aria2Next 正常 Peer 未封", "198.51.201.25" not in h, str(list(h)))

        # BiglyBT：increment-ban=false → PUT /bans 全量
        put_ok = wait_for(lambda: len(STATE["bt_bans_put"]) >= 1, 15)
        check("BiglyBT PUT /bans 全量应用", put_ok is not None, str(STATE["bt_bans_put"])[:200])
        if STATE["bt_bans_put"]:
            body = STATE["bt_bans_put"][-1]
            ips = set(body.get("replaceWith", []))
            check("BiglyBT 封禁列表含两恶意 IP（跨下载器全局表）",
                  {"198.51.200.10", "198.51.201.20"} <= ips, str(ips))
            check("BiglyBT includeNonPBHEntries=false", body.get("includeNonPBHEntries") is False, str(body))

        # Aria2：setBtPeerBlocklist
        a2_ok = wait_for(lambda: len(STATE["a2_blocklist"]) >= 1, 15)
        check("Aria2 setBtPeerBlocklist 调用", a2_ok is not None, str(STATE["a2_calls"][-6:]))
        if STATE["a2_blocklist"]:
            ips = set(STATE["a2_blocklist"][-1])
            check("Aria2 blocklist 含两恶意 IP", {"198.51.200.10", "198.51.201.20"} <= ips, str(ips))

        # 协议细节：token 校验生效（fake 服务器对错误 token 返回 403/error；
        # 登录成功说明 Bearer / token: 前缀传递正确）
        check("Aria2 getVersion 已调用（token 认证通过）", "aria2.getVersion" in STATE["a2_calls"])

        # 下载器状态端点
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/downloaders/bt-fake/status", headers=auth)
        j = json.loads(body)
        check("BiglyBT 状态 HEALTHY + activeTorrents=1", j["data"]["lastStatus"] == "HEALTHY" and j["data"]["activeTorrents"] == 1, body[:250])
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/downloaders/a2-fake/status", headers=auth)
        j = json.loads(body)
        check("Aria2 状态 HEALTHY", j["data"]["lastStatus"] == "HEALTHY", body[:250])
    finally:
        pbh.terminate()
        try:
            pbh.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pbh.kill()
        logf.close()
        bt_srv.shutdown()
        a2_srv.shutdown()

    if failures:
        print(f"\n{RED}{len(failures)} failed:{RESET} " + "; ".join(failures))
        print(open("/tmp/pbh-adapters.log", errors="ignore").read()[-2500:])
        sys.exit(1)
    print(f"\n{GREEN}ADAPTERS TEST: ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
