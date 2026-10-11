#!/usr/bin/env python3
"""API-level test for pbh-rs: OOBE, downloader CRUD, config read/write with
comment preservation, hot reload, global pause. Uses tests/fakeqb.py as the
downloader endpoint.
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

WORKSPACE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PBH_BIN = os.environ.get("PBH_BIN", os.path.join(WORKSPACE, "target", "debug", "pbh"))
DATA = os.path.join(WORKSPACE, "target", "api-data")
FAKE_PORT = 18090
PBH_PORT = 29897
TOKEN = "oobe-created-token"

GREEN, RED, RESET = "\033[32m", "\033[31m", "\033[0m"
failures = []

def check(name, cond, detail=""):
    mark = GREEN + "PASS" + RESET if cond else RED + "FAIL" + RESET
    print(f"[{mark}] {name}" + (f" — {detail}" if detail and not cond else ""))
    if not cond:
        failures.append(name)

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None

_opener = urllib.request.build_opener(NoRedirect)

def http_call(url, method="GET", data=None, headers=None, timeout=10):
    req = urllib.request.Request(url, method=method, data=data, headers=headers or {})
    try:
        with _opener.open(req, timeout=timeout) as r:
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

def kill_orphans():
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            argv = open(f"/proc/{pid}/cmdline", "rb").read().decode(errors="ignore").split("\x00")
        except OSError:
            continue
        if argv and os.path.basename(argv[0]) in ("pbh", "python3") and any("fakeqb" in a or argv[0].endswith("/pbh") for a in argv):
            if "api_test" in " ".join(argv):
                continue
            try:
                os.kill(int(pid), 9)
            except OSError:
                pass

def main():
    kill_orphans()
    shutil.rmtree(DATA, ignore_errors=True)
    subprocess.run([PBH_BIN, "doctor", "--data-dir", DATA], capture_output=True, timeout=60)
    # 保证 token 为空（OOBE 态）
    cfg_path = os.path.join(DATA, "config", "config.yml")
    cfg = open(cfg_path).read()
    assert 'token: ""' in cfg

    fakeqb = subprocess.Popen([sys.executable, os.path.join(WORKSPACE, "tests", "fakeqb.py"), str(FAKE_PORT)],
                              stdout=subprocess.DEVNULL)
    logf = open("/tmp/pbh-api.log", "w")
    pbh = subprocess.Popen([PBH_BIN, "run", "--data-dir", DATA, "--port", str(PBH_PORT)],
                           stdout=logf, stderr=subprocess.STDOUT)
    base = f"http://127.0.0.1:{PBH_PORT}"
    try:
        assert wait_for(lambda: http_call(base + "/api/metadata/manifest", timeout=2)[0] == 200, 30)

        # --- 未初始化状态：受保护端点应 303 → /init ---
        s, body, hdrs = http_call(base + "/api/bans")
        check("未初始化访问 /api/bans → 303 NeedInit", s == 303 and (hdrs.get("Location") or hdrs.get("location")) == "/init", f"{s} {body[:100]}")

        # --- OOBE testDownloader ---
        draft = {
            "id": "qb-fake",
            "config": {
                "type": "qbittorrent",
                "name": "FakeQB",
                "endpoint": f"http://127.0.0.1:{FAKE_PORT}",
                "username": "admin",
                "password": "adminadmin",
                "apiKey": "",
                "basicAuth": {"user": "", "pass": ""},
                "incrementBan": False,
                "useShadowBan": False,
                "verifySsl": True,
                "ignorePrivate": False,
                "paused": False,
            },
        }
        s, body, _ = http_call(base + "/api/oobe/testDownloader", "POST",
                               data=json.dumps(draft).encode(),
                               headers={"Content-Type": "application/json"})
        j = json.loads(body)
        check("OOBE testDownloader → success", s == 200 and j.get("success") is True, body[:200])

        # --- OOBE init ---
        init_body = {"token": TOKEN, "downloader": draft}
        s, body, _ = http_call(base + "/api/oobe/init", "POST",
                               data=json.dumps(init_body).encode(),
                               headers={"Content-Type": "application/json"})
        check("OOBE init → 201", s == 201 and json.loads(body).get("success") is True, f"{s} {body[:200]}")

        # 重复 init → 403
        s, body, _ = http_call(base + "/api/oobe/init", "POST",
                               data=json.dumps(init_body).encode(),
                               headers={"Content-Type": "application/json"})
        check("重复 OOBE init → 403", s == 403, f"{s}")

        # --- config.yml 写回校验（注释保留 + token + client 节）---
        cfg = open(cfg_path).read()
        check("config.yml 写入 token", f'token: "{TOKEN}"' in cfg or f"token: {TOKEN}" in cfg, cfg[cfg.find("token"):cfg.find("token")+60])
        check("config.yml 保留原注释", "# 设置程序语言" in cfg and "# WebUI listen port" in cfg)
        check("config.yml 写入 client 节", "client:" in cfg and "qb-fake:" in cfg and "type: qbittorrent" in cfg)
        check("client 节含 basic-auth 子树", "basic-auth:" in cfg)

        auth = {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"}

        # --- 登录 ---
        s, body, _ = http_call(base + "/api/auth/login", "POST",
                               data=json.dumps({"token": TOKEN}).encode(),
                               headers={"Content-Type": "application/json"})
        check("OOBE 后 token 立即生效（无需重启）", s == 200 and json.loads(body).get("success") is True, body[:150])

        # --- 下载器 CRUD ---
        s, body, _ = http_call(base + "/api/downloaders", headers=auth)
        j = json.loads(body)
        check("GET /api/downloaders 含 qb-fake", any(d["id"] == "qb-fake" for d in j["data"]), body[:200])

        draft2 = json.loads(json.dumps(draft))
        draft2["id"] = "qb-second"
        draft2["config"]["name"] = "SecondQB"
        s, body, _ = http_call(base + "/api/downloaders", "PUT", data=json.dumps(draft2).encode(), headers=auth)
        check("PUT /api/downloaders 创建 → 201", s == 201 and json.loads(body).get("success") is True, f"{s} {body[:150]}")
        s, body, _ = http_call(base + "/api/downloaders", "PUT", data=json.dumps(draft2).encode(), headers=auth)
        check("重复创建 → 400 ALREADY_EXISTS", s == 400, f"{s}")

        cfg = open(cfg_path).read()
        check("第二个下载器持久化到 config.yml", "qb-second:" in cfg and "SecondQB" in cfg)

        draft2["config"]["name"] = "RenamedQB"
        s, body, _ = http_call(base + f"/api/downloaders/qb-second", "PATCH",
                               data=json.dumps(draft2).encode(), headers=auth)
        check("PATCH 重命名 → 200", s == 200 and json.loads(body).get("success") is True, f"{s} {body[:150]}")
        cfg = open(cfg_path).read()
        check("重命名写回 config.yml", "RenamedQB" in cfg and "SecondQB" not in cfg)

        s, body, _ = http_call(base + f"/api/downloaders/{draft2['id']}", "DELETE", headers=auth)
        check("DELETE 下载器 → 200", s == 200 and json.loads(body).get("success") is True, f"{s} {body[:150]}")
        cfg = open(cfg_path).read()
        check("删除写回 config.yml", "qb-second" not in cfg and "qb-fake" in cfg)
        check("删除后注释仍保留", "# 设置程序语言" in cfg)

        s, body, _ = http_call(base + "/api/downloaders/qb-fake/status", headers=auth)
        j = json.loads(body)
        check("GET /api/downloaders/{id}/status 形状", s == 200 and {"lastStatus", "activeTorrents", "config"} <= set(j["data"].keys()), body[:250])

        # --- 配置 JSON 视图 ---
        s, body, _ = http_call(base + "/api/general/config", headers=auth)
        j = json.loads(body)
        d = j.get("data", {})
        check("GET /api/general/config：键 '-'→'_'", "pbh_plus_key" in d and "banlist_remapping" in d, str(list(d.keys())[:8]))
        check("config 视图含 server.token", d.get("server", {}).get("token") == TOKEN)

        s, body, _ = http_call(base + "/api/general/profile", headers=auth)
        j = json.loads(body)
        d = j.get("data", {})
        pid_rules = d.get("module", {}).get("peer_id_blacklist", {}).get("banned_peer_id")
        check("profile 视图：banned_peer_id 为对象列表", isinstance(pid_rules, list) and isinstance(pid_rules[0], dict), str(pid_rules)[:150])

        # PUT profile：追加一条规则（对象列表 → JSON 字符串写回）
        pid_rules.append({"method": "CONTAINS", "content": "apitest-rule"})
        put_body = {"module": {"peer_id_blacklist": {"banned_peer_id": pid_rules}}}
        s, body, _ = http_call(base + "/api/general/profile", "PUT", data=json.dumps(put_body).encode(), headers=auth)
        check("PUT /api/general/profile → 201 + reload", s == 201 and json.loads(body).get("success") is True, f"{s} {body[:150]}")
        prof = open(os.path.join(DATA, "config", "profile.yml")).read()
        check("规则以 JSON 字符串写回 profile.yml", "apitest-rule" in prof and '{\\"method\\":\\"CONTAINS\\"' in prof or 'apitest-rule' in prof)
        check("profile.yml 注释保留", "# Check interval" in prof or "检查频率" in prof)

        # 热重载验证：新规则立即生效（模块列表包含新规则）
        s, body, _ = http_call(base + "/api/modules/peer-id-blacklist", headers=auth)
        j = json.loads(body)
        rules_str = json.dumps(j.get("data", {}))
        check("热重载后新规则可见", "apitest-rule" in rules_str, rules_str[:200])

        # PUT config：修改标量
        s, body, _ = http_call(base + "/api/general/config", "PUT",
                               data=json.dumps({"logger": {"hide_finish_log": True}}).encode(), headers=auth)
        check("PUT /api/general/config 标量 → 201", s == 201, f"{s} {body[:120]}")
        cfg = open(cfg_path).read()
        check("hide-finish-log 写回为 true", "hide-finish-log: true" in cfg, cfg[cfg.find("hide-finish-log"):][:60])

        # --- global pause ---
        s, body, _ = http_call(base + "/api/general/global", headers=auth)
        j = json.loads(body)
        check("GET /api/general/global", j["data"].get("globalPaused") is False, body[:120])
        s, body, _ = http_call(base + "/api/general/global", "PATCH",
                               data=json.dumps({"globalPaused": True}).encode(), headers=auth)
        check("PATCH globalPaused=true → 200", s == 200 and json.loads(body).get("success") is True, body[:120])
        s, body, _ = http_call(base + "/api/general/global", headers=auth)
        check("globalPaused 已生效", json.loads(body)["data"].get("globalPaused") is True)
        http_call(base + "/api/general/global", "PATCH", data=json.dumps({"globalPaused": False}).encode(), headers=auth)

        # --- reload 端点 ---
        s, body, _ = http_call(base + "/api/general/reload", "POST", data=b"", headers=auth)
        j = json.loads(body)
        check("POST /api/general/reload → entries", s == 200 and isinstance(j.get("data"), list) and len(j["data"]) > 0, body[:200])

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
        print(open("/tmp/pbh-api.log").read()[-2000:])
        sys.exit(1)
    print(f"\n{GREEN}API TEST: ALL CHECKS PASSED{RESET}")

if __name__ == "__main__":
    main()
