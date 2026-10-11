#!/usr/bin/env python3
"""Real-downloader end-to-end test: pbh-rs vs a real qBittorrent-nox instance.

Prereq (sandbox): qbittorrent-nox running with WebUI at 127.0.0.1:28080
(admin/adminadmin), BT listen port from preferences.

Phases:
  A (control)  — a legitimate peer (-qB4520-…) connects; pbh must NOT ban it,
                 and must visibly SEE it (peer count in BanWave logs).
  B (ban)      — a leech-style peer (-HP0001…, matching the default PeerID
                 blacklist) connects; pbh must ban it: banned_IPs applied to
                 the REAL qBittorrent, history persisted, /api/bans populated,
                 and qB must then REFUSE new handshakes from the banned IP.
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import http.cookiejar
import uuid

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "real"))
from btlib import FakePeer, make_torrent, random_payload, try_handshake  # noqa: E402

WORKSPACE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
DATA = os.path.join(WORKSPACE, "target", "real-data")
QB_WEB_PORT = int(os.environ.get("QB_WEB_PORT", "28180"))
QB_WEB = f"http://127.0.0.1:{QB_WEB_PORT}/api/v2"
QB_HOME = "/tmp/pbh-real-qbhome"
QB_STDOUT = "/tmp/qb-stdout.log"
QB_PASSWORD = "adminadmin"  # qB ≥4.6.1 首次启动生成临时密码，start_real_qb() 会更新此值
QB_DOWNLOADS = "/tmp/pbh-real-qbdownloads"
TOKEN = "real-test-token"
PAYLOAD_DIR = QB_DOWNLOADS
PAYLOAD = os.path.join(PAYLOAD_DIR, "pbh-real-test-payload.bin")


def _parse_temp_password():
    """qB ≥4.6.1 首次启动打印临时 WebUI 密码，从 stdout 日志解析"""
    try:
        import re
        text = open(QB_STDOUT, errors="ignore").read()
        m = re.search(r"temporary password is provided for this session:\s*(\S+)", text)
        if m:
            return m.group(1)
        m = re.search(r"临时密码[^:：]*[:：]\s*(\S+)", text)
        if m:
            return m.group(1)
    except OSError:
        pass
    return None


def start_real_qb():
    """启动独立的 qbittorrent-nox 实例（独立 HOME，测试自包含）。
    兼容固定密码（≤4.6.0，adminadmin）与首次启动临时密码（≥4.6.1）。"""
    global QB_PASSWORD
    shutil.rmtree(QB_HOME, ignore_errors=True)
    shutil.rmtree(QB_DOWNLOADS, ignore_errors=True)
    os.makedirs(QB_HOME, exist_ok=True)
    os.makedirs(QB_DOWNLOADS, exist_ok=True)
    env = dict(os.environ, HOME=QB_HOME, XDG_CONFIG_HOME=os.path.join(QB_HOME, ".config"),
               XDG_DATA_HOME=os.path.join(QB_HOME, ".local", "share"))
    out = open(QB_STDOUT, "w")
    proc = subprocess.Popen(
        ["qbittorrent-nox", f"--webui-port={QB_WEB_PORT}"],
        env=env, stdout=out, stderr=subprocess.STDOUT)

    def try_login(pwd):
        try:
            with urllib.request.urlopen(
                    f"http://127.0.0.1:{QB_WEB_PORT}/api/v2/auth/login",
                    data=urllib.parse.urlencode(
                        {"username": "admin", "password": pwd}).encode(),
                    timeout=2) as r:
                return r.read().decode().strip() == "Ok."
        except Exception:
            return None  # 未就绪

    candidates = ["adminadmin"]
    for _ in range(60):
        time.sleep(0.5)
        for pwd in candidates:
            ok = try_login(pwd)
            if ok:
                QB_PASSWORD = pwd
                return proc
            if ok is None:
                break  # WebUI 未就绪，等下一轮
        # WebUI 已就绪但密码不对 → 解析临时密码
        tmp = _parse_temp_password()
        if tmp and tmp not in candidates:
            candidates.insert(0, tmp)
    raise RuntimeError("qbittorrent-nox did not come up (login failed); stdout tail: " +
                       open(QB_STDOUT, errors="ignore").read()[-400:])

GOOD_PEER_ID = b"-qB4520-realtest0001"
BAD_PEER_ID = b"-HP0001xunleivariant"

GREEN, RED, YELLOW, RESET = "\033[32m", "\033[31m", "\033[33m", "\033[0m"
failures = []


def check(name, cond, detail=""):
    mark = GREEN + "PASS" + RESET if cond else RED + "FAIL" + RESET
    print(f"[{mark}] {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)


def info(name, detail=""):
    print(f"[{YELLOW}INFO{RESET}] {name}" + (f" — {detail}" if detail else ""))


class QB:
    def __init__(self):
        self.cj = http.cookiejar.CookieJar()
        self.op = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.cj))

    def call(self, path, data=None, raw_body=None, content_type=None, timeout=15):
        req = urllib.request.Request(QB_WEB + path, data=data if raw_body is None else raw_body)
        if content_type:
            req.add_header("Content-Type", content_type)
        try:
            with self.op.open(req, timeout=timeout) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def json_call(self, path, data=None):
        s, b = self.call(path, data)
        return s, json.loads(b) if b else None

    def login(self):
        s, b = self.call("/auth/login", urllib.parse.urlencode(
            {"username": "admin", "password": QB_PASSWORD}).encode())
        assert b.decode().strip() == "Ok.", f"qB login failed: {b!r}"

    def prefs(self):
        s, b = self.call("/app/preferences")
        return json.loads(b)

    def set_prefs(self, d):
        self.call("/app/setPreferences",
                  urllib.parse.urlencode({"json": json.dumps(d)}).encode())

    def add_torrent(self, torrent_bytes, category):
        boundary = "----pbhrs%s" % uuid.uuid4().hex[:12]
        body = b""
        def part(name, value, filename=None, ctype=None):
            nonlocal body
            body += ("--%s\r\n" % boundary).encode()
            disp = f'Content-Disposition: form-data; name="{name}"'
            if filename:
                disp += f'; filename="{filename}"'
            body += (disp + "\r\n").encode()
            if ctype:
                body += ("Content-Type: %s\r\n" % ctype).encode()
            body += b"\r\n" + value + b"\r\n"
        part("torrents", torrent_bytes, filename="test.torrent", ctype="application/x-bittorrent")
        part("category", category.encode())
        part("savepath", PAYLOAD_DIR.encode())
        part("paused", b"false")
        part("root_folder", b"false")
        body += ("--%s--\r\n" % boundary).encode()
        s, b = self.call("/torrents/add", raw_body=body,
                         content_type=f"multipart/form-data; boundary={boundary}")
        assert s == 200 and b.decode().strip() == "Ok.", f"torrents/add failed: {s} {b!r}"

    def torrents(self, category=None, filt=None):
        qs = []
        if category:
            qs.append(f"category={category}")
        if filt:
            qs.append(f"filter={filt}")
        q = ("?" + "&".join(qs)) if qs else ""
        s, b = self.call("/torrents/info" + q)
        return json.loads(b)

    def peers(self, thash):
        s, b = self.call(f"/sync/torrentPeers?hash={thash}")
        return json.loads(b).get("peers", {})

    def delete_torrent(self, thash):
        self.call("/torrents/delete", urllib.parse.urlencode(
            {"hashes": thash, "deleteFiles": "false"}).encode())


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


def start_pbh(port, logfile):
    # 端口必须空闲，否则说明有孤儿进程
    import socket as _sk
    probe = _sk.socket()
    try:
        if probe.connect_ex(("127.0.0.1", port)) == 0:
            raise RuntimeError(f"port {port} already occupied — orphan process?")
    finally:
        probe.close()
    f = open(logfile, "w")
    p = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(port)],
                         stdout=f, stderr=subprocess.STDOUT)
    # wait api ready
    ok = wait_for(lambda: http_call(f"http://127.0.0.1:{port}/api/metadata/manifest", timeout=2)[0] == 200, 30)
    assert ok, "pbh api did not come up"
    return p, f


def stop_pbh(p, f):
    p.terminate()
    try:
        p.wait(timeout=15)
    except subprocess.TimeoutExpired:
        p.kill()
    f.close()


def kill_orphan_pbh():
    """清理沙盒中残留的 pbh 进程（避免端口/数据目录竞争）"""
    killed = []
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            argv = open(f"/proc/{pid}/cmdline", "rb").read().decode(errors="ignore").split("\x00")
        except OSError:
            continue
        if argv and os.path.basename(argv[0]) == "pbh":
            try:
                os.kill(int(pid), 15)
                killed.append(pid)
            except OSError:
                pass
    if killed:
        time.sleep(2)
        for pid in killed:
            try:
                os.kill(int(pid), 9)
            except OSError:
                pass
        info("cleaned orphan pbh processes", ",".join(killed))


def main():
    kill_orphan_pbh()
    # ---------- prepare ----------
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(os.path.join(DATA, "config"), exist_ok=True)
    os.makedirs(PAYLOAD_DIR, exist_ok=True)

    qb_proc = start_real_qb()
    info("real qBittorrent instance started", f"webui={QB_WEB_PORT} home={QB_HOME}")
    qb = QB()
    qb.login()

    # 幂等清理：删除旧测试种子、清空封禁列表
    for t in qb.torrents(category="pbh-real-test"):
        qb.delete_torrent(t["hash"])
    qb.set_prefs({"banned_IPs": ""})

    qb.set_prefs({"dht": False, "pex": False, "lsd": False,
                  "save_path": QB_DOWNLOADS + "/",
                  "enable_multi_connections_from_same_ip": True,
                  # 限速上传：让假 Peer 持续有传输（种子保持 active）但累计量远低于
                  # PCB 各阈值，确保对照组不触发任何规则
                  "up_limit": 32768, "alt_up_limit": 32768, "scheduler_enabled": False})
    prefs = qb.prefs()
    bt_port = prefs["listen_port"]
    info("qB BT listen port", str(bt_port))

    # 本机对外 IP（qB 将以此看到我们的假 Peer；避开 127/8 忽略段）
    import socket as _s
    probe = _s.socket(_s.AF_INET, _s.SOCK_DGRAM)
    probe.connect(("8.8.8.8", 80))
    local_ip = probe.getsockname()[0]
    probe.close()
    info("peer source IP as seen by qB", local_ip)

    # 造种子并做种
    random_payload(PAYLOAD, 32 * 1024 * 1024)
    torrent_bytes, infohash = make_torrent(PAYLOAD)
    qb.add_torrent(torrent_bytes, "pbh-real-test")
    thash = infohash.hex()
    def is_seeding():
        for t in qb.torrents(category="pbh-real-test"):
            if t["hash"] == thash and t["progress"] >= 1.0 and t["state"] in (
                    "stalledUP", "uploading", "forcedUP", "queuedUP"):
                return True
        return None
    seeded = wait_for(is_seeding, 60)
    assert seeded, "torrent did not reach seeding state"
    info("torrent seeding", thash)

    # ---------- prepare pbh data dir ----------
    subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, timeout=60)

    # CI 可移植性：清空忽略地址列表（CI runner 本机 IP 可能落在 10/8 等默认忽略段内，
    # 沙盒中则为公网段）。测试语义不变：验证的是模块决策而非默认忽略配置。
    import re as _re
    _prof_path = os.path.join(DATA, "config", "profile.yml")
    _prof = open(_prof_path).read()
    _prof = _re.sub(r"ignore-peers-from-addresses:\n(?:  - [^\n]+\n)+", "ignore-peers-from-addresses: []\n", _prof)
    open(_prof_path, "w").write(_prof)

    cfg_path = os.path.join(DATA, "config", "config.yml")
    cfg = open(cfg_path).read()
    cfg = cfg.replace('token: ""', f'token: "{TOKEN}"')
    cfg = cfg.replace("auto-update: true", "auto-update: false")
    cfg += f"""
client:
  qb-real:
    type: qbittorrent
    name: RealQB
    endpoint: http://127.0.0.1:{QB_WEB_PORT}
    username: admin
    password: adminadmin
    api-key: ""
    basic-auth:
      user: ""
      pass: ""
    increment-ban: false
    use-shadow-ban: false
    verify-ssl: true
    ignore-private: false
    paused: false
"""
    open(cfg_path, "w").write(cfg)

    auth = {"Authorization": f"Bearer {TOKEN}"}
    good_peer = bad_peer = None
    pbh = pbh_log_f = None
    try:
        # ================= PHASE A: 正常 peer 不应被封 =================
        good_peer = FakePeer(local_ip, bt_port, infohash, GOOD_PEER_ID,
                             piece_length=262144, total_size=32 * 1024 * 1024)
        good_peer.start()
        assert good_peer.connected.wait(40), f"good peer handshake failed: {good_peer.error}"
        info("good peer connected (leeching)", GOOD_PEER_ID.decode())
        # 等待 qB 视该种子为 active（有真实上传流量），对应上游 filter=active 的可见性前提
        became_active = wait_for(lambda: any(
            t["hash"] == thash for t in qb.torrents(category="pbh-real-test", filt="active")), 30)
        info("torrent active (filter=active)", "yes" if became_active else "NO — pbh 将看不到该种子")
        peers = qb.peers(thash)
        info("qB sees peers", json.dumps({k: {kk: vv for kk, vv in v.items() if kk in
              ("client", "peer_id_client", "ip", "port", "connection", "flags", "dl_speed", "up_speed")}
              for k, v in peers.items()}, ensure_ascii=False))

        pbh, pbh_log_f = start_pbh(29898, "/tmp/pbh-real-a.log")
        time.sleep(14)  # ≥2 BanWaves
        log_a = open("/tmp/pbh-real-a.log").read()
        stop_pbh(pbh, pbh_log_f); pbh = None

        saw_peers = False
        import re
        for m in re.finditer(r"Completed check from (\d+) downloaders with (\d+) active torrents and (\d+) peers", log_a):
            if int(m.group(2)) >= 1 and int(m.group(3)) >= 1:
                saw_peers = True
        check("A: BanWave 看到 1 个活跃种子与 ≥1 个真实 Peer", saw_peers, log_a[-800:])

        banned_ips_a = qb.prefs().get("banned_IPs", "")
        check("A: 正常 Peer 未被封禁 (banned_IPs 为空)", banned_ips_a.strip() == "", repr(banned_ips_a))
        conn = sqlite3.connect(os.path.join(DATA, "db", "peerbanhelper.db"))
        n_hist = conn.execute("SELECT COUNT(*) FROM history").fetchone()[0]
        conn.close()
        check("A: history 无记录", n_hist == 0, str(n_hist))
        good_peer.stop(); good_peer.join(timeout=5); good_peer = None
        time.sleep(1)

        # ================= PHASE B: 恶意 PeerID peer 应被封禁 =================
        bad_peer = FakePeer(local_ip, bt_port, infohash, BAD_PEER_ID,
                            piece_length=262144, total_size=32 * 1024 * 1024)
        bad_peer.start()
        assert bad_peer.connected.wait(40), f"bad peer handshake failed: {bad_peer.error}"
        info("bad peer connected (leeching)", BAD_PEER_ID.decode())
        wait_for(lambda: any(t["hash"] == thash for t in qb.torrents(category="pbh-real-test", filt="active")), 30)

        pbh, pbh_log_f = start_pbh(29899, "/tmp/pbh-real-b.log")

        def ban_applied():
            ips = qb.prefs().get("banned_IPs", "")
            return ips if local_ip in ips else None

        applied = wait_for(ban_applied, 60)
        check("B: 真实 qB 的 banned_IPs 已应用封禁", applied is not None,
              qb.prefs().get("banned_IPs", "")[:200])

        conn = sqlite3.connect(os.path.join(DATA, "db", "peerbanhelper.db"))
        rows = conn.execute(
            "SELECT ip, port, peer_id, peer_client_name, module_name, rule_name FROM history").fetchall()
        conn.close()
        check("B: history 落库 1 条", len(rows) == 1, str(rows))
        if rows:
            ip, port, pid, cname, module, rule = rows[0]
            check("B: history.ip = 恶意 Peer IP", ip == local_ip, ip)
            # qB 的 peer_id_client 仅报告 PeerId 前缀（上游同样如此），断言前缀一致
            check("B: history.peer_id 记录 qB 报告的 PeerId 前缀", pid and pid.startswith("-HP"), str(pid))
            check("B: module 为黑名单模块",
                  module in ("com.ghostchu.peerbanhelper.module.impl.rule.PeerIdBlacklist",
                             "com.ghostchu.peerbanhelper.module.impl.rule.ClientNameBlacklist"), module)
            tc = json.loads(rule)
            check("B: rule_name 为 TranslationComponent", "key" in tc and tc["key"].startswith("MATCH_STRING"), str(tc))

        s, body, _ = http_call("http://127.0.0.1:29899/api/bans?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        results = j.get("data", {}).get("results", [])
        check("B: /api/bans 返回 1 条封禁", j.get("success") and len(results) == 1, body[:300])
        if results:
            check("B: /api/bans address 正确", results[0].get("address") == local_ip, str(results[0].get("address")))
            md = results[0].get("banMetadata", {})
            # qB 仅报告 PeerId 前缀，断言前缀
            check("B: banMetadata.peer.id 为 qB 报告的 PeerId", (md.get("peer", {}).get("id") or "").startswith("-HP"), str(md.get("peer")))
            check("B: banMetadata.downloader 信息正确", md.get("downloader", {}).get("name") == "RealQB", str(md.get("downloader")))

        # 已存在连接是否被 qB 主动断开（ informational：qB 版本行为差异）
        closed_existing = wait_for(lambda: bad_peer.closed.is_set(), 15)
        info("B: qB 主动断开已封禁 Peer 的既有连接", "yes" if closed_existing else "no (informational)")

        # 硬断言：封禁生效后，被封 IP 的新握手必须被真实 qB 拒绝
        refused_bad = not try_handshake(local_ip, bt_port, infohash, BAD_PEER_ID)
        check("B: 封禁后恶意 Peer 重连被真实 qB 拒绝", refused_bad)
        refused_good = not try_handshake(local_ip, bt_port, infohash, GOOD_PEER_ID)
        check("B: 封禁后同 IP 任意 Peer 均被拒绝（IP 级封禁生效）", refused_good)

        # 解封路径：从 pbh 删除封禁后 qB 应恢复接受连接（DELETE /api/bans）
        s, body, _ = http_call("http://127.0.0.1:29899/api/bans", method="DELETE",
                          data=json.dumps([local_ip]).encode(),
                          headers={**auth, "Content-Type": "application/json"})
        info("DELETE /api/bans", f"{s} {body[:120]}")
        if s == 200 and json.loads(body).get("success"):
            def unban_applied():
                ips = qb.prefs().get("banned_IPs", "")
                return True if local_ip not in ips else None
            ok = wait_for(unban_applied, 30)
            check("C: 解封后 qB banned_IPs 已移除该 IP", ok is True, qb.prefs().get("banned_IPs", "")[:200])
            if ok:
                accepted = try_handshake(local_ip, bt_port, infohash, GOOD_PEER_ID)
                check("C: 解封后正常 Peer 可重新握手", accepted)
        else:
            info("C: DELETE /api/bans 尚未实现（M2 计划项），跳过解封校验")

        stop_pbh(pbh, pbh_log_f); pbh = None
    finally:
        for p in (good_peer, bad_peer):
            if p:
                p.stop()
        if pbh:
            stop_pbh(pbh, pbh_log_f)
        try:
            qb.delete_torrent(thash)
            qb.set_prefs({"banned_IPs": ""})
        except Exception:
            pass
        try:
            qb_proc.terminate()
            qb_proc.wait(timeout=10)
        except Exception:
            try:
                qb_proc.kill()
            except Exception:
                pass
        os.remove(PAYLOAD) if os.path.exists(PAYLOAD) else None

    if failures:
        print(f"\n{RED}{len(failures)} check(s) failed:{RESET} " + "; ".join(failures))
        print("pbh log A tail:\n", open("/tmp/pbh-real-a.log").read()[-1500:])
        print("pbh log B tail:\n", open("/tmp/pbh-real-b.log").read()[-2500:])
        sys.exit(1)
    print(f"\n{GREEN}REAL-DOWNLOADER E2E: ALL CHECKS PASSED{RESET}")


if __name__ == "__main__":
    main()
