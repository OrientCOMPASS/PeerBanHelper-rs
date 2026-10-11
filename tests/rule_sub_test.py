#!/usr/bin/env python3
"""IPBlackRuleList（规则订阅）行为测试：
本地 HTTP 规则服务器 → pbh 拉取订阅 → 命中订阅网段的 Peer 被封禁 →
rule_sub_info/rule_sub_log 落库 → /api/sub/* 端点行为。
"""
import http.server
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

WORKSPACE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
DATA = os.path.join(WORKSPACE, "target", "sub-data")
FAKE_PORT = 18094
RULE_PORT = 18095
PBH_PORT = 29892
TOKEN = "sub-test-token"

RULE_CONTENT = """# PBH-RS test subscription
# 测试网段
203.0.113.0/24
198.51.100.77 specific-host-entry
"""

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


class RuleHandler(http.server.BaseHTTPRequestHandler):
    hits = 0
    def log_message(self, *a):
        pass
    def do_GET(self):
        RuleHandler.hits += 1
        body = RULE_CONTENT.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(os.path.join(DATA, "config"))
    subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, timeout=60)
    cfg_path = os.path.join(DATA, "config", "config.yml")
    cfg = open(cfg_path).read().replace('token: ""', f'token: "{TOKEN}"')
    cfg = cfg.replace("auto-update: true", "auto-update: false")
    cfg += f"""
client:
  qb-sub:
    type: qbittorrent
    name: SubQB
    endpoint: http://127.0.0.1:{FAKE_PORT}
    username: admin
    password: adminadmin
    increment-ban: false
    verify-ssl: true
    ignore-private: false
    paused: false
"""
    open(cfg_path, "w").write(cfg)

    # profile：默认订阅指向本地规则服务器
    prof_path = os.path.join(DATA, "config", "profile.yml")
    prof = open(prof_path).read()
    prof = prof.replace("url: https://bcr.pbh-btn.com/combine/all.txt",
                        f"url: http://127.0.0.1:{RULE_PORT}/rules.txt")
    open(prof_path, "w").write(prof)

    rule_srv = http.server.ThreadingHTTPServer(("127.0.0.1", RULE_PORT), RuleHandler)
    threading.Thread(target=rule_srv.serve_forever, daemon=True).start()

    fakeqb = subprocess.Popen([sys.executable, os.path.join(WORKSPACE, "tests", "fakeqb.py"), str(FAKE_PORT)],
                              stdout=subprocess.DEVNULL)
    time.sleep(1)
    # peers：订阅网段内 + 网段外
    peers = {}
    for ip, port, pid, client in [
        ("203.0.113.66", 6300, "-qB5100-subtest0001", "qBittorrent/5.1.0"),   # 在 203.0.113.0/24 → 应封
        ("198.51.100.77", 6301, "-qB5100-subtest0002", "qBittorrent/5.1.0"),  # 精确条目 → 应封
        ("8.8.4.4", 6302, "-qB5100-subtest0003", "qBittorrent/5.1.0"),        # 不在订阅 → 不封
    ]:
        peers[f"{ip}:{port}"] = {
            "client": client, "connection": "BT", "dl_speed": 1000, "up_speed": 500,
            "downloaded": 0, "uploaded": 100, "flags": "d u e p",
            "ip": ip, "peer_id_client": pid, "port": port, "progress": 0.5,
        }
    http_call(f"http://127.0.0.1:{FAKE_PORT}/control/setpeers", "POST",
              data=json.dumps(peers).encode(), headers={"Content-Type": "application/json"})

    logf = open("/tmp/pbh-sub.log", "w")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT)
    db_path = os.path.join(DATA, "db", "peerbanhelper.db")
    auth = {"Authorization": f"Bearer {TOKEN}"}
    try:
        assert wait_for(lambda: http_call(f"http://127.0.0.1:{PBH_PORT}/api/metadata/manifest", timeout=2)[0] == 200, 30)

        def history_ips():
            conn = sqlite3.connect(db_path)
            rows = conn.execute("SELECT ip, module_name FROM history").fetchall()
            conn.close()
            return {ip: mod for ip, mod in rows}

        SUB_FQCN = "com.ghostchu.peerbanhelper.module.impl.rule.IPBlackRuleList"
        ok = wait_for(lambda: {"203.0.113.66", "198.51.100.77"} <= set(history_ips()), 45)
        h = history_ips()
        check("订阅网段 203.0.113.0/24 内 Peer 被封", h.get("203.0.113.66") == SUB_FQCN, str(h))
        check("订阅精确条目 198.51.100.77 被封", h.get("198.51.100.77") == SUB_FQCN, str(h))
        check("订阅外 IP 8.8.4.4 未被封", "8.8.4.4" not in h, str(h))
        check("规则服务器被访问过", RuleHandler.hits >= 1, str(RuleHandler.hits))

        # rule_sub_info / rule_sub_log
        conn = sqlite3.connect(db_path)
        info_rows = conn.execute("SELECT rule_id, enabled, rule_name, sub_url, ent_count FROM rule_sub_info").fetchall()
        log_rows = conn.execute("SELECT rule_id, count, update_type FROM rule_sub_log").fetchall()
        conn.close()
        check("rule_sub_info 落库", len(info_rows) == 1 and info_rows[0][0] == "all-in-one"
              and info_rows[0][1] == 1 and info_rows[0][4] == 2, str(info_rows))
        check("rule_sub_log 记录 AUTO 更新", any(r[2] == "AUTO" and r[1] == 2 for r in log_rows), str(log_rows))

        # API
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/sub/rules", headers=auth)
        j = json.loads(body)
        rules = j.get("data", [])
        check("GET /api/sub/rules 返回订阅规则", any(r["ruleId"] == "all-in-one" and r["entCount"] == 2 for r in rules), body[:250])

        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/sub/logs?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        check("GET /api/sub/logs 分页", j["data"]["total"] >= 1 and j["data"]["results"][0]["updateType"] == "AUTO", body[:250])

        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/sub/rules/update", "POST", data=b"", headers=auth)
        j = json.loads(body)
        check("POST /api/sub/rules/update 手动更新", s == 200 and j.get("success") is True, body[:250])

        # 新增自定义订阅规则（PUT /api/sub/rule）→ 写回 profile.yml 并生效
        new_rule = {"id": "extra-list", "name": "extra-list", "enabled": True,
                    "url": f"http://127.0.0.1:{RULE_PORT}/rules.txt"}
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/sub/rule", "PUT",
                               data=json.dumps(new_rule).encode(),
                               headers={**auth, "Content-Type": "application/json"})
        check("PUT /api/sub/rule 创建订阅 → 200", s == 200 and json.loads(body).get("success") is True, f"{s} {body[:150]}")
        prof = open(prof_path).read()
        check("订阅写回 profile.yml（保留注释）", "extra-list:" in prof and "# 检查间隔" in prof, prof[-300:])

        ok = wait_for(lambda: "extra-list" in [r["ruleId"] for r in
                        json.loads(http_call(f"http://127.0.0.1:{PBH_PORT}/api/sub/rules", headers=auth)[1])["data"]], 20)
        check("新订阅出现在 /api/sub/rules", ok is not None)

        # 封禁已应用到下载器
        s, body, _ = http_call(f"http://127.0.0.1:{FAKE_PORT}/state")
        banned = set()
        for pref in json.loads(body).get("prefs", []):
            j2 = json.loads(pref)
            if "banned_IPs" in j2:
                banned = {x.strip() for x in j2["banned_IPs"].split("\n") if x.strip()}
        check("订阅封禁已应用到下载器", {"203.0.113.66", "198.51.100.77"} <= banned, str(banned))

        # DELETE 订阅规则
        s, body, _ = http_call(f"http://127.0.0.1:{PBH_PORT}/api/sub/rule/extra-list", "DELETE", headers=auth)
        check("DELETE /api/sub/rule/{id} → 200", s == 200 and json.loads(body).get("success") is True, body[:150])
        prof = open(prof_path).read()
        check("删除写回 profile.yml", "extra-list" not in prof)
    finally:
        pbh.terminate()
        try:
            pbh.wait(timeout=15)
        except subprocess.TimeoutExpired:
            pbh.kill()
        logf.close()
        fakeqb.kill()
        rule_srv.shutdown()

    if failures:
        print(f"\n{RED}{len(failures)} failed:{RESET} " + "; ".join(failures))
        print(open("/tmp/pbh-sub.log", errors="ignore").read()[-2500:])
        sys.exit(1)
    print(f"\n{GREEN}RULE-SUB TEST: ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
