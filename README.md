# pbh-rs — PeerBanHelper 的 Rust 重写（实验性）

[PeerBanHelper](https://github.com/PBH-BTN/PeerBanHelper)（BT 反吸血工具）无头后端的 **Rust 全量重写**。
设计原则：**不复刻上游内部实现，而是理解其功能与设计后，保持进程外行为一致**，做成可直接替换的
x86_64 CLI 守护进程：

- 直接接管上游数据目录（`config.yml` v48 / `profile.yml` v40 / SQLite 库，DDL 逐字一致）
- Web API 与上游 Gson 序列化形状一致（`StdResp`、epoch 毫秒、`TranslationComponent`、null 省略、
  `PBHPage` 分页），**上游 Vue WebUI（v9.5.1 dist）零改动接入**
- 封禁决策、模块顺序、合并语义（SKIP > BAN > BAN_FOR_DISCONNECT > NO_ACTION，同级取更长时长）、
  日志文案、数据库落库（history/banlist/pcb_*/rule_sub_*/peer_records/tracked_swarm/…）全面对齐
- 上游历史怪癖按 bug-compat 原则原样保留（如 PID 模块命中时 reason 使用 CNB 键、"Hit-" 前缀、
  IdleDos 的百分比单位不一致、AutoRangeBan 的 handshaking→pass 等，代码内均有注释标注）

> ⚠️ **非官方项目**。独立实验性衍生作品，GPL-3.0（与上游同许可）。上游 `AGENTS.md` 禁止 AI 生成
> 代码进入其仓库，本项目不向上游提交代码；本仓库明确允许 AI 生成代码，由人类维护者审查合入。

## 实测指标（x86_64 Linux，沙盒环境）

| 指标 | pbh-rs | 上游 Java (v9.5.1) |
|---|---|---|
| 分发体积 | **12.3 MB** 单二进制（strip+LTO） | JRE 镜像 300MB+ / 安装包 60~125MB |
| 常驻内存 | **~17 MB RSS**（含 GeoIP 三库 mmap；无 GeoIP ~12 MB） | 堆软上限 386MB（实际 RSS 常见 300~600MB） |
| 冷启动 | **~55 ms**（API 就绪） | JVM+Spring+Flyway 5~15 s |
| BanWave | 无 GC 停顿，5s 周期毫秒级完成 | G1 周期性 GC |

## 功能状态

### ✅ 已实现并通过测试

| 领域 | 内容 |
|---|---|
| 引擎 | BanWave 调度（check-interval）、过期解封、手动封禁/解封（含上游 scheduleBanPeerNoAssign 全部细节：100 年时长、`-USROPS-` mock peer、Lombok toString 参数）、needReApply 全量重应用、全局暂停、封禁表持久化（1h+关闭+变更节流）、崩溃安全（WAL） |
| 规则模块 | PeerID 黑名单、ClientName 黑名单、IP/端口黑名单（含 GeoIP ASN/地区/城市/网络类型）、**PCB 假进度检测**（fastPcbTest/过量下载/进度差异延迟窗口/进度回退，pcb_address+pcb_range 双实体、缓慢失忆攻击缓解）、多拨追猎（含 keep-hunting）、AutoRangeBan 连锁封禁（/30、/48）、空闲连接 DoS 保护（三种保护模式）、AntiVampire 迅雷预设、**IP 集规则订阅**（远程拉取/本地缓存回退/注释合并解析/二分区间匹配/定时刷新/rule_sub_info+log 落库） |
| 下载器 | **qBittorrent/qBEE**（真机 4.5.2 验证）、**Transmission**（真机 3.00 验证，目标 4.1+）、**BiglyBT**（协议级 fake 验证）、**Aria2Next**（协议级 fake 验证）；统一特性旗标、限速/端口透传、增量+全量封禁、banlist-remapping |
| 封禁通道 | qB `banned_IPs`/`banPeers`、Transmission blocklist-url 三格式端点（/blocklist/ip、p2p-plain-format 含 zlib 头规避与 UA workaround、dat-emule）、BiglyBT POST/PUT /bans、aria2 setBtPeerBlocklist |
| Web API | WebUI 引用的 **61 个端点**全覆盖：认证（Bearer/?token/Cookie、403/401/303-NeedInit）、OOBE 全流程、下载器 CRUD+测试+状态、bans(GET/POST/DELETE)+logs+ranks、配置 JSON 视图读写（**保留注释**的 YAML round-trip、键 `-`↔`_`、规则串↔对象列表、push-notification 字典↔列表）、热重载、全局暂停、日志（history+SSE live）、统计（counter/banTrends/date/field 含 SUBSTR(8) peerId 语义）、torrent/peer 查询、alerts、模块视图、订阅管理、推送 CRUD+测试 |
| GeoIP | GeoLite2-City/ASN + GeoCN（上游公开镜像、xz 解压、独立线程下载防饿死 async runtime、行政区划 CSV 前缀查找、CN/TW/HK/MO 覆盖合并、1h 缓存、auto-update）、history.peer_geoip 快照、BanDTO.ipGeoData、IPBlackList 全维度规则 |
| 监视 | PeerRecording（peer_records + accessHistory API）、SwarmTracking（tracked_swarm + counter）、ActiveMonitoring（traffic_journal_v3 60s 桶）、数据保留清理 |
| 推送/告警 | 9 种 Provider（Telegram/Webhook/Bark/Gotify/ntfy/PushDeer/PushPlus/ServerChan/SMTP）、alert 表 + 未读去重 + API |
| i18n | 上游 messages.yml 三语言全量内嵌（~580 键），Accept-Language 协商，服务端烘焙 + WebUI 键值透传 |
| 平台 | SIGTERM/SIGINT 优雅停机（Docker stop 语义）、滚动日志文件、`doctor` 诊断子命令 |

### ⛔ 按决策禁用 / 未实现（明确记录）

| 项 | 状态 |
|---|---|
| BTN 网络 | 按项目决策禁用（含脚本下发）；`/api/modules/btn` 返回 disabled |
| 插件系统 (PF4J) | 按项目决策砍掉；`/api/plugins` 返回空列表 |
| AviatorScript 表达式规则 | 随 BTN 脚本决策禁用；启用时记录警告 |
| PBH Plus | 不提供（图表端点按上游未激活许可行为返回 402；status 返回 not activated） |
| Deluge / BitComet 适配器 | 未实现（Deluge 依赖 PBH-Adapter-Deluge 插件协议、BitComet 需 AES 握手且仅 Windows；配置中出现时记录明确警告并跳过） |
| AutoSTUN / NAT 检测 / UPnP | 未实现（上游默认关闭；端点返回 enabled=false） |
| PTR 黑名单 | 上游本身已注释停用（registerModules 中被注释），保持一致 |
| MySQL/PostgreSQL、H2 | 仅 SQLite（覆盖无头场景主流用法） |
| 桌面 GUI (Swing/SWT) | 不在范围内（WebUI 替代） |
| umami 遥测 / Sentry | 不移植（衍生项目不向上游回传数据） |

## 快速开始

```bash
cargo build --release                      # 需 Rust ≥1.85 与 C 工具链（sqlite bundled 编译）

./target/release/pbh doctor --data-dir ./data   # 初始化数据目录并诊断
# 浏览器打开 http://127.0.0.1:9898/init 走 OOBE 向导（设置 token + 添加下载器）
./target/release/pbh run --data-dir ./data

# WebUI：将上游 webui dist（或官方发行包 jar 内 static/）放入 <data>/static/
```

数据目录与上游完全同构：`config/{config.yml,profile.yml}`、`db/peerbanhelper.db`、`logs/`、
`static/`、`ipdb/`、`sub/`。**从 Java 版迁移**：停旧进程 → 备份数据目录 → 用 pbh-rs 指向同一
目录即可（banlist/history/PCB 记录直接可读；建议单向迁移，回退前先恢复备份）。

## 测试

```bash
cargo test                                  # 规则引擎/YAML 编辑器/GeoIP 单元测试
python3 tests/smoke.py                      # 基础闭环（假 qB，26 项）
python3 tests/api_test.py                   # OOBE/CRUD/配置写回/热重载（33 项）
python3 tests/pcb_test.py                   # PCB 四场景（12 项）
python3 tests/modules_test.py               # 多拨/连锁/反吸血（10 项）
python3 tests/rule_sub_test.py              # 规则订阅全链路（13 项）
python3 tests/geoip_test.py                 # ASN 规则+真实 mmdb（5 项）
python3 tests/adapters_test.py              # BiglyBT/Aria2Next 协议（12 项）
python3 tests/batch_a_test.py               # WebUI API 面+监视+推送+SSE（36 项）
python3 tests/real_qb_test.py               # ★ 真机 qbittorrent-nox 4.5.2 三阶段（19 项）
python3 tests/real_tr_test.py               # ★ 真机 transmission-daemon 3.00 全链路（9 项）
PBH_BIN=target/release/pbh python3 tests/...  # 以 release 二进制跑任意套件
```

Windows 本地跑测试：先 `set PYTHONUTF8=1`（Python 默认 cp1252 编码无法解码
UTF-8 配置/日志；CI 已内置该环境变量）。

CI（`.github/workflows/ci.yml`，可复用）在**两个目标平台**执行：ubuntu-24.04 上跑
单元测试 + 全部 8 套 fake-downloader 套件 + 真机 qbittorrent-nox/transmission-daemon
两套 e2e；windows-latest 上跑单元测试 + 同 8 套 fake-downloader 套件。
Release 工作流（tag `v*`）先以完整 CI 为门禁，构建各平台 release 二进制后，
**在目标平台用实际发布工件再跑一遍 smoke 套件自检**，通过后才打包 WebUI/SHA256 并发布。

真机测试自管理下载器实例（独立 HOME/端口），使用原始 BitTorrent 协议假 Peer
（真实握手 + 分块请求）驱动封禁判定，并验证下载器侧实际生效（连接被断开/拒绝、
blocklist 收敛、解封后恢复）。

## 架构

```
crates/
├── pbh-core        # 领域模型（Gson 兼容 serde）、规则引擎、CIDR、BanList、i18n
├── pbh-config      # 数据目录、点分路径 YAML 读取、注释保留 round-trip 编辑器、JSON 视图转换、热重载句柄
├── pbh-db          # rusqlite：上游 DDL 逐字一致的全部表 + 查询/批量写
├── pbh-geoip       # mmdb 读取、镜像下载(xz)、GeoCN 行政区划、netType 映射
├── pbh-downloader  # Downloader trait + qBittorrent/Transmission/BiglyBT/Aria2Next + banlist 重映射
├── pbh-modules     # 9 个规则模块（上游 registerModules 顺序）+ PCB 双实体存储 + 订阅匹配器
├── pbh-push        # 9 种推送 Provider（含 lettre SMTP）
├── pbh-engine      # BanWave 循环、封禁落地/解封/持久化、监视数据收集与刷写、告警
├── pbh-web         # axum：61 端点、认证中间件、SSE、静态 WebUI+SPA 回退
└── pbh-cli         # pbh 二进制：run/doctor/version、tracing（stdout+滚动文件+LogHub 层）
```

并发模型：上游 BanOrgan 七级流水线以 tokio 任务 + 有界 channel 语义等价实现（单调度者
BanWave + 每下载器顺序 torrent 迭代 + 模块同步判定）；GeoIP 下载/xz 解压在独立 OS 线程，
避免 CPU 密集操作饿死 async worker。

## 部署

- `deploy/Dockerfile`：多阶段构建 → scratch 运行镜像（~15MB）
- `deploy/pbh.service`：systemd 单元
- Windows：`pbh.exe run` 可直接注册为服务（sc create / NSSM），SIGTERM 语义对应服务停止

## 许可与致谢

GPL-3.0。行为规格、配置模板、多语言文件、表结构、行政区划数据派生自
[PBH-BTN/PeerBanHelper](https://github.com/PBH-BTN/PeerBanHelper) v9.5.1（GPL-3.0，
Copyright PBH-BTN 团队及贡献者）。GeoLite2/GeoCN 数据来自 MaxMind 及 PBH-BTN 公开镜像。
本项目与上游组织无隶属关系；「PeerBanHelper」名称与商标权利归上游所有。
