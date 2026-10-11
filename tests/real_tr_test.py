#!/usr/bin/env python3
"""Real-downloader e2e test #2: pbh-rs vs a real transmission-daemon.

验证 Transmission 适配器的完整封禁链路：
  login(session-get) → 版本旁路(3.00 测试) → session-set 配置 blocklist-url 指向 pbh 自身
  → blocklist-update 拉取 /blocklist/p2p-plain-format → 恶意 Peer 被封 → IP 被 transmission
  阻断（重连拒绝）→ DELETE /api/bans → blocklist 收敛 → 正常 Peer 可重连。

注意：上游要求 Transmission ≥ 4.1.0（peer_id/bytes 字段）；沙盒内为 3.00，
测试通过 PBH_RS_TR_BYPASS_VERSION_CHECK=1 旁路版本检查（3.00 缺少 peer_id →
PeerID 规则不命中，封禁经由 ClientName 规则或 IP 规则触发，测试按实际报告断言）。
"""
import base64
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request

WORKSPACE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(WORKSPACE, "tests", "real"))
from btlib import FakePeer, make_torrent, random_payload, try_handshake  # noqa: E402

PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
DATA = os.path.join(WORKSPACE, "target", "real-tr-data")
TR_HOME = "/tmp/pbh-real-trhome"
TR_DL = "/tmp/pbh-real-trdownloads"
TR_RPC = "http://127.0.0.1:29091/transmission/rpc"
PBH_PORT = 29894
TOKEN = "tr-test-token"

GREEN, RED, YELLOW, RESET = "\033[32m", "\033[31m", "\033[33m", "\033[0m"
failures = []

def check(name, cond, detail=""):
    mark = GREEN + "PASS" + RESET if cond else RED + "FAIL" + RESET
    print(f"[{mark}] {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)

def info(name, detail=""):
    print(f"[{YELLOW}INFO{RESET}] {name}" + (f" — {detail}" if detail else ""))

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

def http_call(url, method="GET", data=None, headers=None, timeout=10):
    req = urllib.request.Request(url, method=method, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)


class TR:
    """Transmission RPC 客户端（409 会话握手）"""
    def __init__(self, url=TR_RPC):
        self.url = url
        self.sid = None

    def call(self, method, arguments=None, timeout=15):
        body = json.dumps({"method": method, "arguments": arguments or {}}).encode()
        for _ in range(3):
            req = urllib.request.Request(self.url, data=body,
                                         headers={"Content-Type": "application/json"})
            if self.sid:
                req.add_header("X-Transmission-Session-Id", self.sid)
            try:
                with urllib.request.urlopen(req, timeout=timeout) as r:
                    resp = json.loads(r.read().decode())
                    assert resp.get("result") == "success", resp.get("result")
                    return resp.get("arguments", {})
            except urllib.error.HTTPError as e:
                if e.code == 409:
                    self.sid = e.headers.get("X-Transmission-Session-Id")
                    continue
                raise
        raise RuntimeError("tr rpc: no session")


def kill_procs(names):
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            argv = open(f"/proc/{pid}/cmdline", "rb").read().decode(errors="ignore").split("\x00")
        except OSError:
            continue
        if argv and any(n in argv[0] for n in names) and "real_tr_test" not in " ".join(argv):
            try:
                os.kill(int(pid), signal.SIGTERM)
            except OSError:
                pass
    time.sleep(1.5)


def start_transmission():
    kill_procs(["transmission-daemon"])
    shutil.rmtree(TR_HOME, ignore_errors=True)
    shutil.rmtree(TR_DL, ignore_errors=True)
    os.makedirs(TR_HOME, exist_ok=True)
    os.makedirs(TR_DL, exist_ok=True)
    # 第一次启动生成 settings.json，然后停止并改写
    p = subprocess.Popen(["transmission-daemon", "-g", TR_HOME, "-f"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(3)
    p.terminate()
    p.wait(timeout=10)
    sp = os.path.join(TR_HOME, "settings.json")
    s = json.load(open(sp))
    s.update({
        "rpc-port": 29091,
        "rpc-whitelist-enabled": False,
        "rpc-authentication-required": False,
        "download-dir": TR_DL,
        "dht-enabled": False,
        "utp-enabled": False,
        "lpd-enabled": False,
        "pex-enabled": False,
        "speed-limit-up": 32,
        "speed-limit-up-enabled": True,
        "blocklist-enabled": False,
        "peer-port": 25123,
        "port-forwarding-enabled": False,
        "message-level": "none",
    })
    json.dump(s, open(sp, "w"), indent=2)
    proc = subprocess.Popen(["transmission-daemon", "-g", TR_HOME, "-f"],
                            stdout=open("/tmp/tr-test.log", "w"), stderr=subprocess.STDOUT)
    tr = TR()
    assert wait_for(lambda: tr.call("session-get").get("version"), 30), "transmission rpc not up"
    return proc, tr


def main():
    kill_procs(["/pbh"])
    tr_proc, tr = start_transmission()
    info("transmission started", tr.call("session-get")["version"])

    # 种子
    payload = os.path.join(TR_DL, "pbh-tr-test-payload.bin")
    random_payload(payload, 32 * 1024 * 1024)
    torrent_bytes, infohash = make_torrent(payload)
    added = tr.call("torrent-add", {
        "metainfo": base64.b64encode(torrent_bytes).decode(),
        "download-dir": TR_DL,
        "paused": False,
    })
    thash = added["torrent-added"]["hashString"]
    info("torrent added", thash)

    def is_seeding():
        ts = tr.call("torrent-get", {"ids": [thash], "fields": ["percentDone", "status", "rateDownload"]})["torrents"]
        return ts and ts[0]["percentDone"] >= 1.0
    assert wait_for(is_seeding, 60), "torrent not seeding"
    info("torrent seeding", "ok")

    # pbh 数据目录
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(os.path.join(DATA, "config"))
    subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, timeout=60)

    # CI 可移植性：清空忽略地址列表（CI runner 本机 IP 可能落在 10/8 等默认忽略段内，
    # 沙盒中则为公网段）。测试语义不变：验证的是模块决策而非默认忽略配置。
    import re as _re
    _prof_path = os.path.join(DATA, "config", "profile.yml")
    _prof = open(_prof_path).read()
    _prof = _re.sub(r"ignore-peers-from-addresses:\n(?:  - [^\n]+\n)+", "ignore-peers-from-addresses: []\n", _prof)
    open(_prof_path, "w").write(_prof)

    cfg_path = os.path.join(DATA, "config", "config.yml")
    cfg = open(cfg_path).read().replace('token: ""', f'token: "{TOKEN}"')
    cfg = cfg.replace("auto-update: true", "auto-update: false")
    cfg = cfg.replace('prefix: "http://127.0.0.1:9898"', f'prefix: "http://127.0.0.1:{PBH_PORT}"')
    cfg += f"""
client:
  tr-real:
    type: transmission
    name: RealTR
    endpoint: http://127.0.0.1:29091
    rpc-url: /transmission/rpc
    username: ""
    password: ""
    verify-ssl: true
    ignore-private: false
    paused: false
"""
    open(cfg_path, "w").write(cfg)

    # Transmission 3.00 不提供 peer_id 字段（上游因此要求 ≥4.1）；其 clientName 为
    # 截断的原始 PeerId（如 "-HP0001x"）。为在 3.00 上验证完整封禁链路，注入一条
    # ClientName 前缀规则（4.1+ 环境由默认 PeerID 规则覆盖，无需此规则）。
    prof_path = os.path.join(DATA, "config", "profile.yml")
    prof = open(prof_path).read()
    prof = prof.replace("""    banned-client-name:
""", """    banned-client-name:
      - '{"method":"STARTS_WITH","content":"-hp"}'
""", 1)
    open(prof_path, "w").write(prof)

    import socket as _s
    probe = _s.socket(_s.AF_INET, _s.SOCK_DGRAM)
    probe.connect(("8.8.8.8", 80))
    local_ip = probe.getsockname()[0]
    probe.close()
    bt_port = tr.call("session-get")["peer-port"]
    info("peer source ip / tr bt port", f"{local_ip}:{bt_port}")

    auth = {"Authorization": f"Bearer {TOKEN}"}
    good_peer = bad_peer = None
    logf = open("/tmp/pbh-real-tr.log", "w")
    env = dict(os.environ, PBH_RS_TR_BYPASS_VERSION_CHECK="1")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT, env=env)
    try:
        assert wait_for(lambda: http_call(f"http://127.0.0.1:{PBH_PORT}/api/metadata/manifest", timeout=2)[0] == 200, 30)

        # --- pbh 登录应把 blocklist-url 指到自己，并启用 blocklist ---
        def blocklist_configured():
            s = tr.call("session-get")
            return s if (s.get("blocklist-enabled") and
                         s.get("blocklist-url", "").startswith(f"http://127.0.0.1:{PBH_PORT}/blocklist/")) else None
        s = wait_for(blocklist_configured, 45)
        check("登录流程配置 blocklist-url 并启用", s is not None,
              json.dumps({k: tr.call('session-get').get(k) for k in ('blocklist-enabled', 'blocklist-url')}))

        # --- Phase A：正常 Peer ---
        good_peer = FakePeer(local_ip, bt_port, infohash, b"-qB4520-realtest0001",
                             piece_length=262144, total_size=32 * 1024 * 1024)
        good_peer.start()
        assert good_peer.connected.wait(40), f"good peer handshake failed: {good_peer.error}"
        info("good peer connected", "-qB4520-realtest0001")

        # 诊断：transmission 如何报告这个 peer
        def peers_of():
            ts = tr.call("torrent-get", {"ids": [thash], "fields": ["peers"]})["torrents"]
            return ts[0].get("peers", []) if ts else []
        info("tr peers view", json.dumps(peers_of(), ensure_ascii=False)[:400])

        time.sleep(14)
        import re
        log = open("/tmp/pbh-real-tr.log", errors="ignore").read()
        saw = any(int(m.group(1)) >= 1 and int(m.group(2)) >= 1 for m in re.finditer(
            r"with (\d+) active torrents and (\d+) peers", log))
        check("BanWave 看到 transmission 的种子与 Peer", saw, log[-400:])
        bl = tr.call("session-get")["blocklist-size"]
        check("Phase A：blocklist 无实际封禁条目", bl <= 1, str(bl))  # 空表时 TransmissionWorkaround 占 1 条

        # --- Phase B：恶意 Peer ---
        good_peer.stop()
        good_peer.join(timeout=5)
        good_peer = None
        time.sleep(1)
        bad_peer = FakePeer(local_ip, bt_port, infohash, b"-HP0001xunleivariant",
                            piece_length=262144, total_size=32 * 1024 * 1024)
        bad_peer.start()
        assert bad_peer.connected.wait(40), f"bad peer handshake failed: {bad_peer.error}"
        info("bad peer connected", "-HP0001xunleivariant")
        time.sleep(3)
        info("tr peers view (bad)", json.dumps(peers_of(), ensure_ascii=False)[:400])

        def banned_in_blocklist():
            # 无 UA 请求 → 空表返回空串；封禁时返回 uuid:IP-IP 行
            body = http_call(f"http://127.0.0.1:{PBH_PORT}/blocklist/p2p-plain-format")[1]
            if local_ip not in body:
                return None
            size = tr.call("session-get")["blocklist-size"]
            return body if size >= 1 else None
        hit = wait_for(banned_in_blocklist, 60)
        check("恶意 Peer 被封并进入 transmission blocklist", hit is not None,
              f"size={tr.call('session-get')['blocklist-size']}")

        conn = sqlite3.connect(os.path.join(DATA, "db", "peerbanhelper.db"))
        rows = conn.execute("SELECT ip, peer_client_name, module_name FROM history").fetchall()
        conn.close()
        check("history 落库", len(rows) >= 1 and rows[0][0] == local_ip, str(rows))
        if rows:
            info("封禁模块/ClientName", f"module={rows[0][2]} clientName={rows[0][1]!r}")

        # 排除同 IP 既有连接干扰：先断开 bad peer，等 tr 清理，再探测
        bad_peer.stop(); bad_peer.join(timeout=5); bad_peer = None
        wait_for(lambda: len(peers_of()) == 0, 20)
        time.sleep(2)
        probe_before = try_handshake(local_ip, bt_port, infohash, b"-qB4520-probe0000001")
        check("封禁生效：blocklist 拒绝被封 IP 的新握手", not probe_before)

        # --- Phase C：解封 ---
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/bans", "DELETE",
                               data=json.dumps([local_ip]).encode(),
                               headers={**auth, "Content-Type": "application/json"})
        check("DELETE /api/bans → 200", s == 200 and json.loads(body).get("success") is True, body[:120])

        def unban_applied():
            size = tr.call("session-get")["blocklist-size"]
            body = http_call(f"http://127.0.0.1:{PBH_PORT}/blocklist/p2p-plain-format")[1]
            return True if (size <= 1 and local_ip not in body) else None
        ok = wait_for(unban_applied, 45)
        check("解封后 transmission blocklist 收敛", ok is True,
              f"size={tr.call('session-get')['blocklist-size']}")
        if ok:
            accepted = wait_for(lambda: try_handshake(local_ip, bt_port, infohash, b"-qB4520-probe0000002"), 20)
            check("解封后正常 Peer 可重新握手", accepted is True)
    finally:
        if bad_peer:
            bad_peer.stop()
        if good_peer:
            good_peer.stop()
        pbh.terminate()
        try:
            pbh.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pbh.kill()
        logf.close()
        try:
            tr.call("torrent-remove", {"ids": [thash], "delete-local-data": True})
        except Exception:
            pass
        tr_proc.terminate()
        try:
            tr_proc.wait(timeout=10)
        except Exception:
            tr_proc.kill()

    if failures:
        print(f"\n{RED}{len(failures)} failed:{RESET} " + "; ".join(failures))
        print(open("/tmp/pbh-real-tr.log", errors="ignore").read()[-2500:])
        sys.exit(1)
    print(f"\n{GREEN}REAL-TRANSMISSION E2E: ALL CHECKS PASSED{RESET}")


if __name__ == "__main__":
    main()
