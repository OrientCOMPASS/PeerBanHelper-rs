#!/usr/bin/env python3
"""Batch-A endpoint tests: logs (history/SSE), statistic, banlogs, ranks,
torrent/peer queries, alerts shape, charts 402 (Plus gate), feature stubs."""
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

WORKSPACE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
DATA = os.path.join(WORKSPACE, "target", "batcha-data")
FAKE_PORT = 18093
PBH_PORT = 29893
TOKEN = "batcha-token"

GREEN, RED, RESET = "\033[32m", "\033[31m", "\033[0m"
failures = []

def check(name, cond, detail=""):
    mark = GREEN + "PASS" + RESET if cond else RED + "FAIL" + RESET
    print(f"[{mark}] {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)

def http_call(url, method="GET", data=None, headers=None, timeout=10):
    req = urllib.request.Request(url, method=method, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)

def wait_for(cond, timeout, interval=0.4):
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
  qb-a:
    type: qbittorrent
    name: BatchAQB
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
    prof = prof.replace("    peer-recording:\n      enabled: true\n      data-flush-interval: 900000",
                        "    peer-recording:\n      enabled: true\n      data-flush-interval: 5000")
    prof = prof.replace("    swarm-tracking:\n      enabled: true\n      data-flush-interval: 3600000",
                        "    swarm-tracking:\n      enabled: true\n      data-flush-interval: 5000")
    open(prof_path, "w").write(prof)

    fakeqb = subprocess.Popen([sys.executable, os.path.join(WORKSPACE, "tests", "fakeqb.py"), str(FAKE_PORT)],
                              stdout=subprocess.DEVNULL)
    time.sleep(1)
    logf = open("/tmp/pbh-batcha.log", "w")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT)
    base = f"http://127.0.0.1:{PBH_PORT}"
    auth = {"Authorization": f"Bearer {TOKEN}"}
    try:
        assert wait_for(lambda: http_call(base + "/api/metadata/manifest", timeout=2)[0] == 200, 30)
        # 清空 fakeqb 默认 peers，注入一个良性 peer 供监视模块记录
        benign = {"203.0.113.5:6500": {
            "client": "qBittorrent/5.1.0", "connection": "BT", "dl_speed": 1000, "up_speed": 500,
            "downloaded": 123, "uploaded": 456, "flags": "d u e p",
            "ip": "203.0.113.5", "peer_id_client": "-qB5100-", "port": 6500, "progress": 0.42}}
        http_call(f"http://127.0.0.1:{FAKE_PORT}/control/setpeers", "POST",
                  data=json.dumps(benign).encode(), headers={"Content-Type": "application/json"})
        time.sleep(7)  # 至少一轮 BanWave

        # --- logs ---
        s, body, _ = http_call(base + "/api/logs/history", headers=auth)
        j = json.loads(body)
        entries = j.get("data", [])
        check("GET /api/logs/history 返回日志环形缓冲", s == 200 and len(entries) > 0, body[:150])
        if entries:
            e = entries[-1]
            check("日志条目字段齐全 (time/thread/level/content/seq)",
                  {"time", "thread", "level", "content", "seq"} <= set(e.keys()), str(e))

        # SSE 原始读取
        got_sse = False
        try:
            sk = socket.create_connection(("127.0.0.1", PBH_PORT), timeout=5)
            sk.sendall((f"GET /api/logs/live HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").encode())
            sk.settimeout(4)
            buf = b""
            t0 = time.time()
            while time.time() - t0 < 3 and len(buf) < 8192:
                try:
                    d = sk.recv(4096)
                    if not d:
                        break
                    buf += d
                except socket.timeout:
                    break
            sk.close()
            got_sse = b"text/event-stream" in buf and b"data:" in buf
        except Exception as ex:
            print("sse error:", ex)
        check("GET /api/logs/live 为 SSE 且推送历史日志", got_sse)

        # --- 手动封禁一条以产生数据 ---
        http_call(base + "/api/bans", "POST", data=json.dumps(["203.0.113.200"]).encode(),
                  headers={**auth, "Content-Type": "application/json"})
        def manual_ban_logged():
            d = json.loads(http_call(base + "/api/bans/logs?page=1&pageSize=10", headers=auth)[1])["data"]
            return d if any(r.get("peerIp") == "203.0.113.200" for r in d["results"]) else None
        ok = wait_for(manual_ban_logged, 30)
        s, body, _ = http_call(base + "/api/bans/logs?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        d = j["data"]
        check("GET /api/bans/logs PBHPage + 记录", ok and d["total"] >= 1 and len(d["results"]) >= 1, body[:200])
        if d["results"]:
            r = d["results"][0]
            check("banLog 字段 (peerIp/banAt/module/rule)",
                  r.get("peerIp") == "203.0.113.200" and isinstance(r.get("banAt"), int)
                  and r.get("module", "").startswith("com.ghostchu") and isinstance(r.get("rule"), str),
                  str(r)[:300])
            check("banLog.downloader 为 DownloaderBasicInfo", r.get("downloader", {}).get("name") == "BatchAQB", str(r.get("downloader")))

        s, body, _ = http_call(base + "/api/bans/ranks?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        check("GET /api/bans/ranks {key,value}", any(x["key"] == "203.0.113.200" and x["value"] >= 1 for x in j["data"]["results"]), body[:200])

        now_ms = int(time.time() * 1000)
        s, body, _ = http_call(base + f"/api/statistic/analysis/banTrends?startAt=0&endAt={now_ms}", headers=auth)
        j = json.loads(body)
        check("banTrends 按天聚合", s == 200 and len(j["data"]) >= 1 and "key" in j["data"][0], body[:200])

        s, body, _ = http_call(base + "/api/statistic/analysis/field?type=count&field=module_name", headers=auth)
        j = json.loads(body)
        check("field 分析 (count by module_name)", s == 200 and any("DownloaderServerImpl" in x["data"] for x in j["data"]), body[:250])

        s, body, _ = http_call(base + f"/api/statistic/analysis/date?startAt=0&endAt={now_ms}", headers=auth)
        j = json.loads(body)
        check("analysis/date {timestamp,count,percent}", s == 200 and len(j["data"]) >= 1 and "timestamp" in j["data"][0], body[:200])

        s, body, _ = http_call(base + "/api/statistic/counter", headers=auth)
        j = json.loads(body)
        d = j["data"]
        check("statistic/counter 形状", {"checkCounter", "peerBanCounter", "banlistCounter"} <= set(d.keys()) and d["checkCounter"] >= 1, str(d)[:200])

        # --- torrent / peer ---
        s, body, _ = http_call(base + "/api/torrent/query?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        check("torrent/query 含手动封禁的 mock 种子", any(t.get("infoHash") == "0" * 20 for t in j["data"]["results"]), body[:250])

        s, body, _ = http_call(base + "/api/peer/203.0.113.200", headers=auth)
        j = json.loads(body)
        check("peer/{ip} banCount=1 且当前在封禁中", j["data"].get("banCount") == 1 and j["data"].get("banned") is True, body[:200])

        s, body, _ = http_call(base + "/api/peer/203.0.113.200/banHistory?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        check("peer/{ip}/banHistory 分页", j["data"]["total"] == 1, body[:200])

        # --- alerts ---
        s, body, _ = http_call(base + "/api/alerts", headers=auth)
        check("GET /api/alerts → []", s == 200 and json.loads(body)["data"] == [], body[:120])
        s, body, _ = http_call(base + "/api/alert/dismissAll", "POST", data=b"", headers=auth)
        check("POST /api/alert/dismissAll → OK!", s == 200 and json.loads(body)["message"] == "OK!", body[:120])

        # --- Plus gate & stubs ---
        s, body, _ = http_call(base + "/api/chart/traffic", headers=auth)
        check("chart/* → 402 (Plus gate 对齐)", s == 402, str(s))
        s, body, _ = http_call(base + "/api/pbhplus/status", headers=auth)
        check("pbhplus/status → activated=false", json.loads(body)["data"].get("activated") is False, body[:120])
        for path, expect in [("/api/laboratory/experiments", []), ("/api/expression-engine/scripts", []),
                             ("/api/plugins", []), ("/api/push", [])]:
            s, body, _ = http_call(base + path, headers=auth)
            check(f"{path} → []", s == 200 and json.loads(body)["data"] == expect, body[:120])
        s, body, _ = http_call(base + "/api/sub/rules", headers=auth)
        j = json.loads(body)
        check("/api/sub/rules 返回默认 all-in-one 订阅", s == 200 and any(
            r.get("ruleId") == "all-in-one" for r in j["data"]), body[:200])
        s, body, _ = http_call(base + "/api/autostun/status", headers=auth)
        check("autostun/status → enabled=false", json.loads(body)["data"].get("enabled") is False, body[:120])
        s, body, _ = http_call(base + "/api/modules/btn", headers=auth)
        check("modules/btn → disabled", json.loads(body)["data"].get("enabled") is False, body[:120])
        s, body, _ = http_call(base + "/api/general/checkModuleAvailable?module=Progress%20Cheat%20Blocker", headers=auth)
        check("checkModuleAvailable(PCB) → true", json.loads(body)["data"].get("available") is True, body[:150])
        s, body, _ = http_call(base + "/api/modules/ipblacklist/ip", headers=auth)
        check("modules/ipblacklist/ip 返回默认规则", "0.0.0.0" in json.loads(body)["data"].get("ip", []), body[:150])
        s, body, _ = http_call(base + "/api/modules/ipblacklist/ip/test?ip=8.8.8.8", headers=auth)
        check("ipblacklist ip/test 8.8.8.8 → hit=false", json.loads(body)["data"].get("hit") is False, body[:150])
        s, body, _ = http_call(base + "/api/statistics_list/get", headers=auth)
        j = json.loads(body)
        check("statistics_list/get → fakeqb 流量统计", any(x.get("totalUploaded") == 123456 for x in j["data"]), body[:200])

        # --- Push（webhook provider 本地闭环）---
        import http.server as _hs
        import threading as _th
        received = []
        class _Hook(_hs.BaseHTTPRequestHandler):
            def log_message(self, *a): pass
            def do_POST(self):
                n = int(self.headers.get("Content-Length", 0))
                received.append(json.loads(self.rfile.read(n).decode()))
                self.send_response(200); self.send_header("Content-Length","2"); self.end_headers(); self.wfile.write(b"ok")
        hook_srv = _hs.ThreadingHTTPServer(("127.0.0.1", 18099), _Hook)
        _th.Thread(target=hook_srv.serve_forever, daemon=True).start()
        draft = {"name": "hook1", "type": "webhook",
                 "config": {"url": "http://127.0.0.1:18099/hook", "method": "POST", "content-type": "application/json"}}
        s, body, _ = http_call(base + "/api/push", "PUT", data=json.dumps(draft).encode(),
                               headers={**auth, "Content-Type": "application/json"})
        check("PUT /api/push 创建 webhook → 201", s == 201 and json.loads(body).get("success") is True, f"{s} {body[:150]}")
        s, body, _ = http_call(base + "/api/push", headers=auth)
        check("GET /api/push 列出 PushWrapperDTO", any(p["name"] == "hook1" and p["type"] == "webhook" for p in json.loads(body)["data"]), body[:200])
        cfgtxt = open(cfg_path).read()
        check("push 配置写回 config.yml 且保留注释", "hook1:" in cfgtxt and "# 设置程序语言" in cfgtxt)
        s, body, _ = http_call(base + "/api/push/test", "POST", data=json.dumps(draft).encode(),
                               headers={**auth, "Content-Type": "application/json"})
        ok = wait_for(lambda: len(received) >= 1, 10)
        check("POST /api/push/test 实际送达 webhook", s == 200 and ok and received[0].get("title"), str(received)[:200])
        s, body, _ = http_call(base + "/api/push/hook1", "DELETE", headers=auth)
        check("DELETE /api/push/{name} → 200", s == 200 and json.loads(body).get("success") is True, body[:150])
        check("删除后配置同步移除", "hook1:" not in open(cfg_path).read())
        hook_srv.shutdown()

        # --- 监视模块 ---
        import sqlite3 as _sq
        def mon_state():
            conn = _sq.connect(os.path.join(DATA, "db", "peerbanhelper.db"))
            pr = conn.execute("SELECT COUNT(*) FROM peer_records").fetchone()[0]
            ts = conn.execute("SELECT COUNT(*) FROM tracked_swarm").fetchone()[0]
            tj = conn.execute("SELECT COUNT(*) FROM traffic_journal_v3").fetchone()[0]
            conn.close()
            return pr, ts, tj
        ok = wait_for(lambda: (lambda x: x[0] >= 1 and x[1] >= 1 and x[2] >= 1)(mon_state()), 40)
        pr, ts, tj = mon_state()
        check("peer_records 记录良性 Peer", pr >= 1, f"pr={pr}")
        check("tracked_swarm 记录", ts >= 1, f"ts={ts}")
        check("traffic_journal_v3 记录", tj >= 1, f"tj={tj}")

        s, body, _ = http_call(base + "/api/peer/203.0.113.5/accessHistory?page=1&pageSize=10", headers=auth)
        j = json.loads(body)
        d = j.get("data", {})
        check("accessHistory 返回记录", d.get("total", 0) >= 1 and d["results"][0].get("uploaded") == 456, body[:250])

        s, body, _ = http_call(base + "/api/statistic/counter", headers=auth)
        j = json.loads(body)
        check("counter.trackedSwarmCount 实时", j["data"].get("trackedSwarmCount", 0) >= 1, body[:200])

        # --- WebUI 静态资源（若已放置 dist）---
        dist = os.path.join(DATA, "static")
        src_dist = os.path.join(WORKSPACE, "webui-dist")
        if os.path.exists(os.path.join(src_dist, "index.html")):
            for name in os.listdir(src_dist):
                sp = os.path.join(src_dist, name)
                dp = os.path.join(dist, name)
                if os.path.isdir(sp):
                    shutil.copytree(sp, dp, dirs_exist_ok=True)
                else:
                    shutil.copy2(sp, dp)
            s, body, hdrs = http_call(base + "/")
            check("WebUI index.html 被服务", s == 200 and "<div id=\"app\">" in body or "html" in body.lower(), str(s))
            s2, b2, _ = http_call(base + "/banlist")
            check("SPA 深链回退到 index.html", s2 == 200, f"status={s2} body={b2[:120]}")
        else:
            print("[INFO] webui-dist 不存在，跳过静态资源检查")
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
        print(open("/tmp/pbh-batcha.log", errors="ignore").read()[-2000:])
        sys.exit(1)
    print(f"\n{GREEN}BATCH-A TEST: ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
