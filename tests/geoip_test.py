#!/usr/bin/env python3
"""GeoIP 集成测试：ASN 封禁规则 + BanDTO.ipGeoData 注入。

前置：target/geoip-data/ipdb/ 下已有 mmdb（由 geoip 冒烟运行下载），测试直接复用，
避免重复下载。若缺失则跳过（CI 可选下载）。
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
LOG_PATH = os.path.join(tempfile.gettempdir(), "pbh-geoiprule.log")
DATA = os.path.join(WORKSPACE, "target", "geoiprule-data")
MMDB_SRC = os.path.join(WORKSPACE, "target", "geoip-data", "ipdb")
FAKE_PORT = 18095
PBH_PORT = 29890
TOKEN = "geoip-test-token"

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

def main():
    if not os.path.exists(os.path.join(MMDB_SRC, "GeoLite2-ASN.mmdb")):
        print(f"[INFO] mmdb 数据缺失（{MMDB_SRC}），先运行一次 pbh 下载后再测。跳过。")
        return
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(os.path.join(DATA, "config"))
    subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, timeout=60)
    shutil.copytree(MMDB_SRC, os.path.join(DATA, "ipdb"))

    cfg_path = os.path.join(DATA, "config", "config.yml")
    cfg = open(cfg_path).read().replace('token: ""', f'token: "{TOKEN}"')
    cfg = cfg.replace("auto-update: true", "auto-update: false")
    cfg += f"""
client:
  qb-geo:
    type: qbittorrent
    name: GeoQB
    endpoint: http://127.0.0.1:{FAKE_PORT}
    username: admin
    password: adminadmin
    increment-ban: false
    verify-ssl: true
    ignore-private: false
    paused: false
"""
    open(cfg_path, "w").write(cfg)

    # profile：为 ip-address-blocker 配置 ASN 15169 (Google) 封禁
    prof_path = os.path.join(DATA, "config", "profile.yml")
    prof = open(prof_path).read()
    # 模板中已存在空的 asns 键（ip-address-blocker 节内），就地填充
    import re as _re
    m = _re.search(r"(  ip-address-blocker:\n(?:.*\n)*?    asns:\n)((?:      - .*\n)*)", prof)
    assert m, "asns key not found under ip-address-blocker"
    prof = prof[:m.end(1)] + "      - 15169\n" + prof[m.end(2):]
    open(prof_path, "w").write(prof)

    fakeqb = subprocess.Popen([sys.executable, os.path.join(WORKSPACE, "tests", "fakeqb.py"), str(FAKE_PORT)],
                              stdout=subprocess.DEVNULL)
    time.sleep(1)
    peers = {}
    for ip, port in [("8.8.8.8", 6400), ("1.1.1.1", 6401)]:
        peers[f"{ip}:{port}"] = {
            "client": "qBittorrent/5.1.0", "connection": "BT", "dl_speed": 1000, "up_speed": 500,
            "downloaded": 0, "uploaded": 100, "flags": "d u e p",
            "ip": ip, "peer_id_client": "-qB5100-", "port": port, "progress": 0.5,
        }
    http_call(f"http://127.0.0.1:{FAKE_PORT}/control/setpeers", "POST",
              data=json.dumps(peers).encode(), headers={"Content-Type": "application/json"})

    logf = open(LOG_PATH, "w")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT)
    auth = {"Authorization": f"Bearer {TOKEN}"}
    try:
        assert wait_for(lambda: http_call(f"http://127.0.0.1:{PBH_PORT}/api/metadata/manifest", timeout=2)[0] == 200, 30)

        def history():
            conn = sqlite3.connect(os.path.join(DATA, "db", "peerbanhelper.db"))
            rows = conn.execute("SELECT ip, module_name, structured_data, peer_geoip FROM history").fetchall()
            conn.close()
            return {r[0]: r for r in rows}

        IBL = "com.ghostchu.peerbanhelper.module.impl.rule.IPBlackList"
        ok = wait_for(lambda: "8.8.8.8" in history(), 40)
        h = history()
        check("ASN 15169 (Google) 规则命中 8.8.8.8", ok is not None and h["8.8.8.8"][1] == IBL, str({k: v[1] for k, v in h.items()}))
        if "8.8.8.8" in h:
            sd = json.loads(h["8.8.8.8"][2] or "{}")
            check("structured type=asn rule=15169", sd.get("type") == "asn" and sd.get("rule") == 15169, str(sd))
            geo = json.loads(h["8.8.8.8"][3]) if h["8.8.8.8"][3] else None
            check("history.peer_geoip 快照落库", geo and geo.get("as", {}).get("number") == 15169, str(geo)[:150])
        check("AS13335 (Cloudflare) 1.1.1.1 未被封", "1.1.1.1" not in h, str(list(h.keys())))

        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/bans?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        results = j["data"]["results"]
        target = next((r for r in results if r["address"] == "8.8.8.8"), None)
        check("/api/bans 注入 ipGeoData", target and target.get("ipGeoData", {}).get("as", {}).get("number") == 15169,
              json.dumps(target.get("ipGeoData") if target else None, ensure_ascii=False)[:200])
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
        print(open(LOG_PATH, errors="ignore").read()[-1500:])
        sys.exit(1)
    print(f"\n{GREEN}GEOIP RULE TEST: ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
