#!/usr/bin/env python3
"""End-to-end smoke test for pbh-rs.

启动 fake qBittorrent → 启动 pbh 守护进程 → 等待 BanWave →
校验：封禁决策、下载器侧 banned_IPs 应用、Web API（login/bans/status/modules）、sqlite 落库。
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.abspath(__file__))
WORKSPACE = os.path.dirname(ROOT)
DATA = os.path.join(WORKSPACE, "target", "smoke-data")
PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
FAKE_QB_PORT = 18080
PBH_PORT = 19898
TOKEN = "smoke-test-token"

GREEN, RED, YELLOW, RESET = "\033[32m", "\033[31m", "\033[33m", "\033[0m"
failures = []

def check(name, cond, detail=""):
    mark = GREEN + "PASS" + RESET if cond else RED + "FAIL" + RESET
    print(f"[{mark}] {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)

def http(url, method="GET", data=None, headers=None, timeout=10):
    req = urllib.request.Request(url, method=method, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(), dict(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(), dict(e.headers)

def main():
    # 清理并准备数据目录
    shutil.rmtree(DATA, ignore_errors=True)
    os.makedirs(os.path.join(DATA, "config"))

    # 首次运行生成模板，然后注入 token 与 client 节
    probe = subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, text=True, timeout=60)
    cfg_path = os.path.join(DATA, "config", "config.yml")
    with open(cfg_path) as f:
        cfg = f.read()
    cfg = cfg.replace('token: ""', f'token: "{TOKEN}"')
    cfg = cfg.replace("auto-update: true", "auto-update: false")
    cfg += f"""
# ---- smoke test downloader ----
client:
  qb-test:
    type: qbittorrent
    name: FakeQB
    endpoint: http://127.0.0.1:{FAKE_QB_PORT}
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
    with open(cfg_path, "w") as f:
        f.write(cfg)

    # 启动 fake qBittorrent
    fakeqb = subprocess.Popen([sys.executable, os.path.join(ROOT, "fakeqb.py"), str(FAKE_QB_PORT)],
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    time.sleep(1)

    # 启动 pbh
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    pbh_log = []
    try:
        # 等待 API 就绪
        ready = False
        for _ in range(50):
            time.sleep(0.3)
            try:
                s, _, _ = http(f"http://127.0.0.1:{PBH_PORT}/api/metadata/manifest", timeout=2)
                if s == 200:
                    ready = True
                    break
            except Exception:
                pass
        check("daemon 启动并监听 API", ready)
        if not ready:
            return

        # 等待 BanWave 应用封禁（fakeqb 收到含目标 IP 的 banned_IPs）
        qb_state = {}
        banned_ips = set()
        for _ in range(40):
            time.sleep(0.5)
            try:
                s, body, _ = http(f"http://127.0.0.1:{FAKE_QB_PORT}/state", timeout=2)
                qb_state = json.loads(body)
            except Exception:
                continue
            banned_ips = set()
            for pref in qb_state.get("prefs", []):
                try:
                    j = json.loads(pref)
                    if "banned_IPs" in j:
                        for line in j["banned_IPs"].split("\n"):
                            if line.strip():
                                banned_ips.add(line.strip())
                except Exception:
                    pass
            if "198.51.100.20" in banned_ips and "198.51.100.30" in banned_ips:
                break

        # --- 校验封禁决策 ---
        check("恶意 PeerID(-hp…) 被封禁 → 198.51.100.20", "198.51.100.20" in banned_ips, str(banned_ips))
        check("恶意 ClientName(hp/torrent…) 被封禁 → 198.51.100.30", "198.51.100.30" in banned_ips, str(banned_ips))
        check("内网 Peer(192.168.1.50) 命中忽略列表未被封", "192.168.1.50" not in banned_ips, str(banned_ips))
        check("Handshaking Peer(203.0.113.99) 未被封", "203.0.113.99" not in banned_ips, str(banned_ips))
        check("正常 Peer(203.0.113.10) 未被封", "203.0.113.10" not in banned_ips, str(banned_ips))
        check("HTTP(WebSeed) 连接被适配器跳过", "203.0.113.77" not in banned_ips, str(banned_ips))

        # 首次成功登录后应下发 enable_multi_connections_from_same_ip=false
        same_ip_disabled = any("enable_multi_connections_from_same_ip" in p for p in qb_state.get("prefs", []))
        check("登录后关闭 qB 同 IP 多连接", same_ip_disabled)

        # --- 校验 Web API ---
        base = f"http://127.0.0.1:{PBH_PORT}"
        s, body, _ = http(base + "/api/general/status")
        check("未登录访问受保护端点 → 403", s == 403, f"status={s} body={body}")

        s, body, _ = http(base + "/api/auth/login", "POST",
                          data=json.dumps({"token": "wrong-token"}).encode(),
                          headers={"Content-Type": "application/json"})
        check("错误 token 登录 → 401", s == 401, f"status={s}")

        s, body, _ = http(base + "/api/auth/login", "POST",
                          data=json.dumps({"token": TOKEN}).encode(),
                          headers={"Content-Type": "application/json"})
        check("正确 token 登录 → 200 success", s == 200 and json.loads(body).get("success") is True, body[:200])

        auth = {"Authorization": f"Bearer {TOKEN}"}
        s, body, _ = http(base + "/api/bans?page=1&pageSize=10", headers=auth)
        ok_shape = False
        ban_addrs = []
        if s == 200:
            j = json.loads(body)
            d = j.get("data", {})
            ok_shape = {"page", "size", "total", "results"} <= set(d.keys())
            ban_addrs = [r.get("address") for r in d.get("results", [])]
        check("/api/bans 返回 PBHPage 结构", ok_shape, body[:300])
        check("/api/bans 包含 2 条封禁", len(ban_addrs) == 2, str(ban_addrs))
        if ban_addrs:
            s2, body2, _ = http(base + "/api/bans?page=1&pageSize=10", headers={**auth, "Accept-Language": "zh-CN"})
            j2 = json.loads(body2)
            first = j2["data"]["results"][0]["banMetadata"]
            check("banMetadata 时间戳为 epoch 毫秒", isinstance(first.get("banAt"), int), str(first.get("banAt")))
            check("rule/description 已按 locale 烘焙为字符串", isinstance(first.get("rule"), str) and isinstance(first.get("description"), str), str(first))

        s, body, _ = http(base + "/api/downloaders", headers=auth)
        j = json.loads(body)
        dls = j.get("data", [])
        check("/api/downloaders 返回 FakeQB", s == 200 and any(d.get("name") == "FakeQB" for d in dls), body[:200])

        s, body, _ = http(base + "/api/modules/peer-id-blacklist", headers=auth)
        j = json.loads(body)
        check("/api/modules/peer-id-blacklist 返回规则列表", s == 200 and len(j.get("data", {}).get("peerId", [])) > 0, body[:200])

        s, body, _ = http(base + "/api/general/status", headers=auth)
        j = json.loads(body)
        st = j.get("data", {}).get("peerbanhelper", {})
        check("/api/general/status 报告封禁数=2", st.get("bannedPeers") == 2, str(st)[:300])

        # --- 校验数据库落库 ---
        db_path = os.path.join(DATA, "db", "peerbanhelper.db")
        conn = sqlite3.connect(db_path)
        cur = conn.cursor()
        n_banlist = cur.execute("SELECT COUNT(*) FROM banlist").fetchone()[0]
        n_history = cur.execute("SELECT COUNT(*) FROM history").fetchone()[0]
        n_torrents = cur.execute("SELECT COUNT(*) FROM torrents").fetchone()[0]
        row = cur.execute("SELECT ip, module_name, rule_name FROM history LIMIT 1").fetchone()
        conn.close()
        check("sqlite banlist 持久化 2 条", n_banlist == 2, str(n_banlist))
        check("sqlite history 记录 2 条", n_history == 2, str(n_history))
        check("sqlite torrents 记录 1 条", n_torrents == 1, str(n_torrents))
        check("history.module_name 使用上游 FQCN", row and row[1].startswith("com.ghostchu.peerbanhelper.module.impl.rule."), str(row))
        check("history.rule_name 为 TranslationComponent JSON", row and json.loads(row[2]).get("key"), str(row))

        # banlist metadata 可反序列化且字段为 camelCase
        conn = sqlite3.connect(db_path)
        meta_raw = conn.execute("SELECT metadata FROM banlist LIMIT 1").fetchone()[0]
        conn.close()
        meta = json.loads(meta_raw)
        need = {"downloader", "torrent", "peer", "context", "randomId", "banAt", "unbanAt", "banForDisconnect", "rule", "description"}
        check("banlist.metadata JSON 字段与上游 Gson 形状一致", need <= set(meta.keys()), str(sorted(meta.keys())))

    finally:
        pbh.terminate()
        try:
            out, _ = pbh.communicate(timeout=10)
            pbh_log.append(out)
        except Exception:
            pbh.kill()
        fakeqb.kill()

    log_text = "\n".join(pbh_log)
    check("BanWave 完成日志出现", "Completed check from" in log_text or "completed" in log_text.lower(), log_text[-500:])
    if failures:
        print(f"\n{RED}{len(failures)} check(s) failed:{RESET} " + ", ".join(failures))
        print("\n--- pbh output (tail) ---")
        print(log_text[-3000:])
        sys.exit(1)
    print(f"\n{GREEN}ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
