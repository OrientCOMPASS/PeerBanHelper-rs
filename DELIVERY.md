# 交付说明与装机测试清单

> 交付物：本仓库（`dev` 分支）+ CI 产出的 Release（tag `v0.1.0`）。
> 行为兼容目标：PeerBanHelper **v9.5.1** 无头（CLI/服务器）形态。
> 兼容性细节与已知差异：见 [docs/compatibility.md](docs/compatibility.md)。

## 1. 交付物清单

**Release 页面**: https://github.com/OrientCOMPASS/PeerBanHelper-rs/releases/tag/v0.1.0

| 交付物 | 位置 | 说明 |
|---|---|---|
| Linux x86_64 静态二进制 | Release `pbh-linux-x86_64-musl` | musl 静态链接，无外部依赖；NAS/软路由/Docker/裸机通用 |
| Windows x86_64 可执行 | Release `pbh-windows-x86_64.exe` | 控制台程序；可用 `sc create` / NSSM 注册为服务 |
| WebUI 静态资源 | Release `pbh-webui-v9.5.1.zip` | 上游官方 v9.5.1 WebUI（零改动兼容），解压到 `<data>/static/` |
| 校验和 | Release `SHA256SUMS.txt` | `sha256sum -c` 验证 |
| Docker | `deploy/Dockerfile` | scratch 运行镜像（~15MB），CI 不推送 registry，可自行 `docker build` |
| systemd | `deploy/pbh.service` | 含资源限制与安全加固 |
| 文档 | `README.md`、`docs/compatibility.md` | 架构、迁移、兼容性矩阵 |
| 测试 | `tests/`（10 套） | 含两套真机下载器 e2e；CI 每次 push 全量执行 |

## 2. CI 验证状态

- 工作流：`.github/workflows/ci.yml`（push/PR 触发；`workflow_call` 可复用）、
  `release.yml`（tag `v*` 触发，先调用完整 CI 作为门禁）
- CI 内容（两个目标平台）：
  - **ubuntu-24.04**：cargo build + 单元测试 → 8 套 fake-downloader 集成测试 →
    **真机 qbittorrent-nox 与真机 transmission-daemon 两套 e2e**（raw BT 协议假 Peer 驱动）
  - **windows-latest**：cargo build + 单元测试 → 同 8 套 fake-downloader 集成测试
    （测试套件已做跨平台化：临时日志走 `tempfile.gettempdir()`、`/proc` 孤儿清扫仅 Linux 生效）
- Release 流水线：完整 CI 门禁全绿 → 构建 musl 静态 Linux 与 Windows MSVC release 二进制 →
  **在每个目标平台用实际发布工件再跑一遍 smoke 套件（26 项）自检** → 提取打包上游 WebUI →
  SHA256（三资产、纯文件名）→ 发布/更新 Release
- 历史节点：首次 CI 全绿 run
  [#38129052232](https://github.com/OrientCOMPASS/PeerBanHelper-rs/actions/runs/38129052232)
  @ `5992076`（2026-10-11）；期间修复 qB ≥4.6.1 首启临时密码兼容（测试侧）与
  `DELETE /api/bans` count 语义（产品侧，对齐上游）
- **发布工件沙盒复核（本会话，v0.1.0 资产）**：SHA256 三资产全匹配；musl release 二进制
  直接跑通全部 8 套 fake-downloader 集成测试（smoke 26 项、api 33 项、batch_a 36 项、
  modules、pcb、rule_sub、adapters、geoip 全 PASS）+ version/doctor 装机冒烟
- 注意（上游 quirk，bug-compat 保留）：未知 `/api/*` 路由返回 **405**（上游
  `EndpointNotFound→METHOD_NOT_ALLOWED`），并非 404

## 3. 装机测试清单（实机验收）

### 3.1 全新安装（Linux x86_64）

1. `sha256sum -c SHA256SUMS.txt` 校验三个资产
2. `chmod +x pbh-linux-x86_64-musl && ./pbh-linux-x86_64-musl version`
3. `./pbh-linux-x86_64-musl doctor --data-dir ./data`
   - 预期：生成 `data/{config,db,logs,static}`；打印配置/数据库诊断；提示无下载器
4. `./pbh-linux-x86_64-musl run --data-dir ./data`
   - 预期：<1s 内日志出现 `WebUI/API server listening on http://0.0.0.0:9898`
5. 解压 `pbh-webui-v9.5.1.zip` 到 `data/static/`
6. 浏览器打开 `http://<host>:9898` → 应重定向 OOBE（/init）
7. OOBE：设置 token → 添加你的真实 qBittorrent（≥4.5.0，host 网络）→ 测试连接 → 完成
8. 登录后检查各页：仪表盘（状态卡/计数器）、封禁列表、封禁日志、排行榜、
   下载器管理、规则管理（PeerID/CNB/IP/订阅）、设置（config/profile 编辑器保存后
   **确认注释未丢失**）、日志页（SSE 实时滚动）
9. 观察真实封禁：等吸血客户端连入，确认封禁列表出现记录、qB 的
   `工具→选项→高级→已封禁 IP` 同步、封禁日志落库
10. `Ctrl+C` / `kill <pid>`（SIGTERM）→ 预期优雅停机：保存封禁表、`Goodbye.`
11. 重启 `run` → 预期日志 `Restored N ban records`（持久化恢复）

### 3.2 内存/体积核对

- `ps -o rss= -p $(pidof pbh-linux-x86_64-musl)` → 无 GeoIP ~12MB；GeoIP 三库加载后 ~17MB
- 首次启动会自动下载 GeoLite2-City/ASN/GeoCN（约 68MB，镜像见日志）；
  不需要则 `config.yml → ip-database.auto-update: false`

### 3.3 从 Java 版 PBH 迁移（可选，重点验收）

1. 停止 Java 版 PBH；**完整备份**其数据目录
2. `pbh doctor --data-dir <Java版数据目录>` → 预期：config v48/profile v40 识别、
   sqlite 打开、banlist 行数读取、下载器登录自检
3. `pbh run --data-dir <同一目录>` → 预期：`Restored N ban records`（N=Java 版封禁数）、
   WebUI token 沿用、封禁列表/历史/PCB 记录/订阅全部可见
4. 运行 ≥1 小时，比对新封禁与 Java 版历史行为（误封/漏封）
5. 回退预案：停 pbh-rs → 恢复备份 → 启动 Java 版（注意：pbh-rs 运行期间新写入的
   记录对 Java 版可读，但建议以备份回退为准）

### 3.4 Windows

1. `pbh-windows-x86_64.exe version` / `doctor --data-dir .\data` / `run`
   （首次运行若 Windows 防火墙弹窗 → 允许访问；控制台 Ctrl+C 应优雅停机打印 `Goodbye.`）
2. 解压 `pbh-webui-v9.5.1.zip` 到 `data\static\` → 浏览器打开 `http://127.0.0.1:9898`
   走 OOBE，其余同 3.1
3. 服务化（推荐 NSSM/WinSW 包装）：
   `nssm install pbh C:\pbh\pbh-windows-x86_64.exe "run --data-dir C:\pbh\data"`
   → `nssm start pbh` / `nssm stop pbh` 验证启停
   - 注意：裸控制台程序直接 `sc create`/`sc start` 会报 1053（pbh 未实现 SCM 服务握手，
     与上游 Java 版行为一致），须配合 NSSM/WinSW 之类包装器注册服务

### 3.5 Docker（可选）

```bash
git clone https://github.com/OrientCOMPASS/PeerBanHelper-rs && cd PeerBanHelper-rs
curl -sSL -o /tmp/pbh.zip https://github.com/PBH-BTN/PeerBanHelper/releases/download/v9.5.1/PeerBanHelper_9.5.1.zip
# 提取 jar 内 static/ 到 webui-dist/（或用 Release 的 pbh-webui zip 解压为 webui-dist/）
docker build -f deploy/Dockerfile -t pbh-rs .
docker run -d --name pbh --network host -v pbh-data:/data -p 9898:9898 pbh-rs
```
注意：与上游一致，**必须 host 网络**（需要看到 Peer 真实 IP）。

### 3.6 建议的重点观察项（一周）

- [ ] 误封率：正常 Peer 是否被 PCB/多拨误伤（对比 Java 版历史）
- [ ] qB `banned_IPs` 条目数与 WebUI 封禁列表一致性
- [ ] 订阅规则（all-in-one）自动更新与 `sub/` 缓存
- [ ] 内存曲线（应平稳在 ~20MB 级）
- [ ] 停机/重启后封禁表恢复数量一致

## 4. 已知边界（详见 docs/compatibility.md 第 5 节）

- Deluge / BitComet 适配器未实现（配置中出现时警告并跳过）
- BTN 网络 / PBH Plus / 插件 / AviatorScript / AutoSTUN 按项目决策禁用或不移植
- 数据库仅 SQLite；图表端点按上游 Plus 门控返回 402
- Transmission 生产目标 ≥4.1.0（CI 用仓库版 + 版本旁路开关验证协议链路）
