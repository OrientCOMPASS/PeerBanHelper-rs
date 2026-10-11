#!/usr/bin/env python3
"""ProgressCheatBlocker behavior test (fake qBittorrent with scripted peers).

Scenarios (torrent size = 5,000,000,000 bytes):
  excessive : uploaded 8.0GB > max(5GB,50MB)*1.5   → 立即 excessive ban
  desync    : uploaded 3.0GB (60%) but reports 1%  → 延迟窗口(2s)后 deSyncDifference ban
  rewind    : uploaded 3GB, reports 90% then 10%   → computed(60%)>reported 时进入判定 → rewindProgress ban
  good      : uploaded 1.0GB (20%), reports 25%    → 永不封禁
Profile overrides: fast-pcb-test-percentage=-1 (禁用快速测试), max-wait-duration=2000ms
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
LOG_PATH = os.path.join(tempfile.gettempdir(), "pbh-pcb.log")
DATA = os.path.join(WORKSPACE, "target", "pcb-data")
FAKE_PORT = 18091
PBH_PORT = 29896
TOKEN = "pcb-test-token"
TORRENT_SIZE = 5_000_000_000

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

def make_peer(ip, port, uploaded, progress, client="qBittorrent/5.1.0", pid="-qB5100-"):
    return {
        f"{ip}:{port}": {
            "client": client, "connection": "BT", "dl_speed": 1000, "up_speed": 1000,
            "downloaded": 0, "uploaded": uploaded, "flags": "d u e p",
            "ip": ip, "peer_id_client": pid, "port": port, "progress": progress,
        }
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
  qb-pcb:
    type: qbittorrent
    name: PcbQB
    endpoint: http://127.0.0.1:{FAKE_PORT}
    username: admin
    password: adminadmin
    increment-ban: false
    verify-ssl: true
    ignore-private: false
    paused: false
"""
    open(cfg_path, "w").write(cfg)

    prof_path = os.path.join(DATA, "config", "profile.yml")
    prof = open(prof_path).read()
    prof = prof.replace("fast-pcb-test-percentage: 0.1", "fast-pcb-test-percentage: -1")
    prof = prof.replace("max-wait-duration: 30000", "max-wait-duration: 2000")
    open(prof_path, "w").write(prof)

    fakeqb = subprocess.Popen([sys.executable, os.path.join(WORKSPACE, "tests", "fakeqb.py"), str(FAKE_PORT)],
                              stdout=subprocess.DEVNULL)
    time.sleep(1)

    # 初始 peer 集：excessive + desync + rewind(90%) + good
    peers = {}
    peers.update(make_peer("198.51.100.40", 6100, 8_000_000_000, 0.5))    # excessive
    peers.update(make_peer("198.51.101.41", 6101, 3_000_000_000, 0.01))   # desync
    peers.update(make_peer("198.51.102.42", 6102, 3_000_000_000, 0.9))     # rewind (先高报；computed 60% > 改后 10% 才会进入 rewind 判定，对齐上游门序)
    peers.update(make_peer("198.51.103.43", 6103, 1_000_000_000, 0.25))   # good
    http_call(f"http://127.0.0.1:{FAKE_PORT}/control/setpeers", "POST",
              data=json.dumps(peers).encode(), headers={"Content-Type": "application/json"})

    logf = open(LOG_PATH, "w")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT)
    db_path = os.path.join(DATA, "db", "peerbanhelper.db")
    auth = {"Authorization": f"Bearer {TOKEN}"}
    try:
        assert wait_for(lambda: http_call(f"http://127.0.0.1:{PBH_PORT}/api/metadata/manifest", timeout=2)[0] == 200, 30)

        def history_types():
            conn = sqlite3.connect(db_path)
            rows = conn.execute(
                "SELECT ip, module_name, structured_data FROM history ORDER BY id").fetchall()
            conn.close()
            out = {}
            for ip, module, sd in rows:
                t = json.loads(sd).get("type") if sd else None
                out[ip] = (module, t)
            return out

        PCB_FQCN = "com.ghostchu.peerbanhelper.module.impl.rule.ProgressCheatBlocker"

        # 1. excessive：首轮即封
        hit = wait_for(lambda: "198.51.100.40" in history_types(), 30)
        types = history_types()
        check("excessive: 超量下载立即封禁", hit is not None and types.get("198.51.100.40", (None, None))[0] == PCB_FQCN, str(types))
        check("excessive: structured type=excessiveMaxDownloadThreshold",
              types.get("198.51.100.40", (None, None))[1] == "excessiveMaxDownloadThreshold", str(types))

        # 2. desync：延迟窗口(2s)后封禁
        hit = wait_for(lambda: "198.51.101.41" in history_types(), 40)
        types = history_types()
        check("desync: 进度差异经延迟窗口后封禁", hit is not None, str(types))
        check("desync: structured type=deSyncDifference",
              types.get("198.51.101.41", (None, None))[1] == "deSyncDifference", str(types))

        # 3. good：始终未被封
        check("good: 正常 Peer 未被封禁", "198.51.103.43" not in history_types(), str(history_types()))

        # 4. rewind：把 0.9 改成 0.1
        http_call(f"http://127.0.0.1:{FAKE_PORT}/control/mutate", "POST",
                  data=json.dumps({"ip": "198.51.102.42", "progress": 0.1}).encode(),
                  headers={"Content-Type": "application/json"})
        hit = wait_for(lambda: "198.51.102.42" in history_types(), 40)
        types = history_types()
        check("rewind: 进度回退封禁", hit is not None, str(types))
        check("rewind: structured type=rewindProgress",
              types.get("198.51.102.42", (None, None))[1] == "rewindProgress", str(types))

        # 5. PCB 持久化表已回写
        conn = sqlite3.connect(db_path)
        n_addr = conn.execute("SELECT COUNT(*) FROM pcb_address").fetchone()[0]
        n_range = conn.execute("SELECT COUNT(*) FROM pcb_range").fetchone()[0]
        sample = conn.execute("SELECT ip, last_report_uploaded, tracking_uploaded_increase_total FROM pcb_address LIMIT 1").fetchone()
        conn.close()
        check("pcb_address/pcb_range 已持久化", n_addr >= 4 and n_range >= 4, f"addr={n_addr} range={n_range}")
        info("pcb_address sample", str(sample))

        # 6. 封禁已应用到下载器（fakeqb prefs banned_IPs 含三个恶意 IP，不含 good）
        s, body, _ = http_call(f"http://127.0.0.1:{FAKE_PORT}/state")
        st = json.loads(body)
        banned = set()
        for pref in st.get("prefs", []):
            j = json.loads(pref)
            if "banned_IPs" in j:
                banned = {x.strip() for x in j["banned_IPs"].split("\n") if x.strip()}
        check("恶意三 IP 已应用到下载器", {"198.51.100.40", "198.51.101.41", "198.51.102.42"} <= banned, str(banned))
        check("good IP 未被应用", "198.51.103.43" not in banned, str(banned))

        # 7. /api/bans 展示三条（banForDisconnect 不算；desync/excessive/rewind 均为普通 BAN）
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/bans?page=1&pageSize=20", headers=auth)
        j = json.loads(body)
        addrs = [r["address"] for r in j["data"]["results"]]
        check("/api/bans 含 3 条 PCB 封禁", len(addrs) == 3, str(addrs))
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
    print(f"\n{GREEN}PCB TEST: ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
