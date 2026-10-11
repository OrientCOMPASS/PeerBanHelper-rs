#!/usr/bin/env python3
"""Rule-module behavior tests: MultiDialingBlocker / AutoRangeBan / AntiVampire.

Torrent: 5GB seeding (fakeqb). All scripted peers are non-handshaking (speeds>0),
uploaded=1e6 (PCB gates never fire), progress=0.5.
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

WORKSPACE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
LOG_PATH = os.path.join(tempfile.gettempdir(), "pbh-modules.log")
DATA = os.path.join(WORKSPACE, "target", "modules-data")
FAKE_PORT = 18092
PBH_PORT = 29895
TOKEN = "modules-test-token"

GREEN, RED, YELLOW, RESET = "\033[32m", "\033[31m", "\033[33m", "\033[0m"
failures = []

def check(name, cond, detail=""):
    mark = GREEN + "PASS" + RESET if cond else RED + "FAIL" + RESET
    print(f"[{mark}] {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)

def info(name, detail=""):
    print(f"[{YELLOW}INFO{RESET}] {name}" + (f" — {detail}" if detail else ""))

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

def peer_entry(ip, port, pid="-qB5100-", client="qBittorrent/5.1.0", uploaded=1_000_000, progress=0.5):
    return {
        f"{ip}:{port}": {
            "client": client, "connection": "BT", "dl_speed": 2000, "up_speed": 100,
            "downloaded": 0, "uploaded": uploaded, "flags": "d u e p",
            "ip": ip, "peer_id_client": pid, "port": port, "progress": progress,
        }
    }

FQCN = {
    "mdb": "com.ghostchu.peerbanhelper.module.impl.rule.MultiDialingBlocker",
    "arb": "com.ghostchu.peerbanhelper.module.impl.rule.AutoRangeBan",
    "av": "com.ghostchu.peerbanhelper.module.impl.rule.AntiVampire",
    "manual": "com.ghostchu.peerbanhelper.DownloaderServerImpl",
}

def main():
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(os.path.join(DATA, "config"))
    subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, timeout=60)
    cfg_path = os.path.join(DATA, "config", "config.yml")
    cfg = open(cfg_path).read().replace('token: ""', f'token: "{TOKEN}"')
    cfg = cfg.replace("auto-update: true", "auto-update: false")
    cfg += f"""
client:
  qb-mod:
    type: qbittorrent
    name: ModQB
    endpoint: http://127.0.0.1:{FAKE_PORT}
    username: admin
    password: adminadmin
    increment-ban: false
    verify-ssl: true
    ignore-private: false
    paused: false
"""
    open(cfg_path, "w").write(cfg)

    fakeqb = subprocess.Popen([sys.executable, os.path.join(WORKSPACE, "tests", "fakeqb.py"), str(FAKE_PORT)],
                              stdout=subprocess.DEVNULL)
    time.sleep(1)

    # peers：三个同 /24 多拨 + 两个迅雷
    peers = {}
    peers.update(peer_entry("198.51.199.10", 6200))
    peers.update(peer_entry("198.51.199.11", 6201))
    peers.update(peer_entry("198.51.199.12", 6202))
    peers.update(peer_entry("198.18.0.10", 6204, pid="-XL0011-", client="Xunlei 0.0.1.1"))
    peers.update(peer_entry("198.18.1.11", 6205, pid="-XL0019-", client="Xunlei 0.0.1.9"))
    http_call(f"http://127.0.0.1:{FAKE_PORT}/control/setpeers", "POST",
              data=json.dumps(peers).encode(), headers={"Content-Type": "application/json"})

    logf = open(LOG_PATH, "w")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT)
    db_path = os.path.join(DATA, "db", "peerbanhelper.db")
    auth = {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"}
    try:
        assert wait_for(lambda: http_call(f"http://127.0.0.1:{PBH_PORT}/api/metadata/manifest", timeout=2)[0] == 200, 30)

        def history():
            conn = sqlite3.connect(db_path)
            rows = conn.execute("SELECT ip, module_name, rule_name FROM history ORDER BY id").fetchall()
            conn.close()
            return {ip: (mod, json.loads(rule)["key"] if rule else None) for ip, mod, rule in rows}

        # --- MultiDial：三个同 /24 IP，两轮内全部封禁（模块=MDB，时长更长胜过 ARB）---
        ok = wait_for(lambda: all(ip in history() for ip in
                                  ("198.51.199.10", "198.51.199.11", "198.51.199.12")), 40)
        h = history()
        check("MultiDial: 同子网 3 IP 全部封禁", ok is not None, str(list(h.keys())))
        check("MultiDial: 模块 FQCN 与规则键正确",
              all(h.get(ip, (None, None))[0] == FQCN["mdb"] and h[ip][1] == "MDB_MULTI_DIALING_DETECTED"
                  for ip in ("198.51.199.10", "198.51.199.11", "198.51.199.12") if ip in h), str(h))

        # --- AntiVampire：迅雷非 0019 + 0019 做种期均封禁 ---
        ok = wait_for(lambda: "198.18.0.10" in history() and "198.18.1.11" in history(), 30)
        h = history()
        check("AntiVampire: 迅雷非 0019 封禁", h.get("198.18.0.10", (None,))[0] == FQCN["av"], str(h.get("198.18.0.10")))
        check("AntiVampire: 迅雷 0019 做种期封禁", h.get("198.18.1.11", (None,))[0] == FQCN["av"], str(h.get("198.18.1.11")))

        # --- AutoRangeBan：手动封 203.0.113.50 后，同 /30 的 .51 被连锁封禁 ---
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/bans", "POST",
                               data=json.dumps(["203.0.113.50"]).encode(), headers=auth)
        check("手动封禁 API → 200", s == 200 and json.loads(body)["data"]["count"] == 1, body[:150])
        wait_for(lambda: "203.0.113.50" in history(), 20)
        h = history()
        check("手动封禁落库（context=DownloaderServerImpl）",
              h.get("203.0.113.50", (None, None))[0] == FQCN["manual"], str(h.get("203.0.113.50")))

        peers2 = dict(peers)
        peers2.update(peer_entry("203.0.113.51", 6203))
        http_call(f"http://127.0.0.1:{FAKE_PORT}/control/setpeers", "POST",
                  data=json.dumps(peers2).encode(), headers={"Content-Type": "application/json"})
        ok = wait_for(lambda: "203.0.113.51" in history(), 30)
        h = history()
        check("AutoRangeBan: /30 连锁封禁", h.get("203.0.113.51", (None, None))[0] == FQCN["arb"], str(h.get("203.0.113.51")))
        check("AutoRangeBan: 规则键为 IPv4/30", h.get("203.0.113.51", (None, None))[1] == "IPv4/30", str(h.get("203.0.113.51")))

        # --- 封禁全部应用到下载器 ---
        s, body, _ = http_call(f"http://127.0.0.1:{FAKE_PORT}/state")
        banned = set()
        for pref in json.loads(body).get("prefs", []):
            j = json.loads(pref)
            if "banned_IPs" in j:
                banned = {x.strip() for x in j["banned_IPs"].split("\n") if x.strip()}
        expect = {"198.51.199.10", "198.51.199.11", "198.51.199.12", "198.18.0.10", "198.18.1.11",
                  "203.0.113.50", "203.0.113.51"}
        check("全部封禁已应用到下载器", expect <= banned, f"missing={expect - banned}")
    finally:
        pbh.terminate()
        try:
            pbh.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pbh.kill()
        logf.close()
        fakeqb.kill()

    if failures:
        print(f"\n{RED}{len(failures)} failed:{RESET} " + "; ".join(failures))
        print(open(LOG_PATH).read()[-2500:])
        sys.exit(1)
    print(f"\n{GREEN}MODULES TEST: ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
