# pbh-rs 兼容性矩阵（对照上游 PeerBanHelper v9.5.1）

本文件记录进程外行为的对齐情况与已知差异。行为验证方式见 `tests/`（10 套自动化套件，
其中 2 套针对真实下载器：qbittorrent-nox 4.5.2、transmission-daemon 3.00）。

## 1. Web API 端点（WebUI dist v9.5.1 静态扫描共 61 个引用端点）

| 端点 | 状态 | 备注 |
|---|---|---|
| POST /api/auth/login · logout | ✅ | 401/200、Set-Cookie PBH-Session、NeedInit 303→/init |
| GET/POST/DELETE /api/bans | ✅ | PBHPage/BanDTO(BakedBanMetadata)/手动封禁(scheduleBanPeerNoAssign 语义)/`*` 全量解封 |
| GET /api/bans/logs · /ranks | ✅ | BanLogDTO 烘焙文案、orderBy 白名单重映射、ranks {key,value} |
| GET/PUT /api/downloaders · /{id} · /{id}/status · /test · /scan | ✅*/⛔ | CRUD+测试+DownloaderStatusDTO；scan（局域网发现）返回空列表 |
| GET/PUT /api/general/config · profile | ✅ | JSON 视图（`-`↔`_`、规则串↔对象、push 字典↔列表）、保留注释合并写回、201+ReloadEntry |
| GET /api/general/status | ✅ | jvm(近似:进程 RSS)/system/peerbanhelper 三段 |
| GET/PATCH /api/general/global · POST /reload · checkModuleAvailable · stacktrace | ✅ | heapdump → 501（JVM 专属） |
| POST /api/oobe/init · testDownloader · testDatabaseConfig · scanDownloader | ✅*/⛔ | init 全链路（token+下载器+btn/database 写回）；scan 返回空 |
| GET /api/modules/{configName} | ✅ | 9 个规则模块视图；ipblacklist/{ruleType}、/ip/test |
| GET /api/logs/history · /api/logs/live (SSE) | ✅ | WebUILogEntryDTO {time,thread,level,content,seq} 环形缓冲+广播 |
| GET /api/statistic/{counter,analysis/*} | ✅ | weeklySessions 恒 0（依赖 Plus 门控的会话度量表） |
| GET /api/chart/* | ⛔ 402 | 上游即 PBH Plus 专属；pbh-rs 无 Plus → 与未激活许可行为一致 |
| GET /api/alerts · PATCH dismiss · dismissAll · DELETE | ✅ | alert 表、未读去重、TC 存 JSON 读时烘焙 |
| GET/PUT/DELETE /api/push* · POST /api/push/test | ✅ | 9 Provider 实际可发送（SMTP 走 lettre/rustls） |
| /api/sub/*（rules/rule/logs/interval/update） | ✅ | 完整订阅生命周期 + rule_sub_info/log |
| GET /api/torrent/query · /{hash} · /api/peer/{ip}/* | ✅* | btnQuery → 不支持（BTN 禁用）；accessHistory 为 peer_records 实数据 |
| /api/pbhplus/* · /laboratory/* · /expression-engine/* · /autostun/* · /modules/btn · /plugins · /graphql | ⛔ stub | 按决策禁用/未实现，返回与「未激活/空/关闭」一致的安全形状 |
| GET /blocklist/{ip,p2p-plain-format,dat-emule} | ✅ | remapping 应用、Transmission UA workaround、zlib 头样字节规避 |

## 2. 规则模块（执行顺序 = 上游 registerModules）

| 模块 | 状态 | 验证 |
|---|---|---|
| IPBlackList（ips/ports/asns/regions/cities/net-type） | ✅ | modules_test、geoip_test（真实 ASN 库命中 AS15169） |
| PeerIdBlacklist / ClientNameBlacklist | ✅ | smoke、real_qb（真机）、adapters |
| ExpressionRule (AviatorScript) | ⛔ | 项目决策禁用（启用时警告日志） |
| ProgressCheatBlocker | ✅ | pcb_test 四场景（excessive/deSync/rewind/对照） |
| MultiDialingBlocker（含 keep-hunting） | ✅ | modules_test（3 IP 同 /24 两轮全封） |
| AutoRangeBan（v4/30、v6/48） | ✅ | modules_test（/30 连锁 + 时长合并规则验证） |
| BtnNetworkOnline / BlockListController(规则下发部分) | ⛔/✅ | BTN 禁用；blocklist 端点见上 |
| IPBlackRuleList（订阅） | ✅ | rule_sub_test（本地订阅服务器全链路） |
| PTRBlacklist | — | 上游已注释停用，保持一致 |
| IdleConnectionDosProtection | ✅（默认禁用） | 逻辑含上游百分比单位 quirk（bug-compat） |
| AntiVampire（迅雷预设） | ✅ | modules_test（非 0019 / 0019 做种期） |
| 监视：ActiveMonitoring / PeerRecording / SwarmTracking / SessionAnalyse | ✅*/⛔ | 前三者真实落库（batch_a_test）；SessionAnalyse（peer_connection_metrics，Plus 图表专属）未实现 |

## 3. 下载器适配器

| 下载器 | 状态 | 验证 |
|---|---|---|
| qBittorrent / qBEE | ✅ | **真机 4.5.2**：登录/版本门/分页取种/属性缓存/peers 过滤(HTTP/onion/i2p)/增量+全量封禁/断连与重连拒绝/解封恢复/统计/同 IP 多连接关闭 |
| Transmission (≥4.1 目标) | ✅ | **真机 3.00**（版本门旁路开关）：session 握手/blocklist-url 自动配置/blocklist-update/p2p-plain 拉取/解封收敛；4.1+ 专属字段（peer_id/bytes）已按上游 null→-1 语义处理 |
| BiglyBT（PBH-Adapter ≥1.3.0） | ✅ 协议级 | fake 插件服务器：Bearer 认证/403 语义/downloads 过滤器/peers hex PeerId/state 30-40 握手判定/POST+PUT /bans 载荷 |
| Aria2Next | ✅ 协议级 | fake JSON-RPC：token: 前缀/tellActive 键集/getPeers/setBtPeerBlocklist/getGlobalStat/字符串数值解析 |
| Deluge（PBH-Adapter-Deluge） | ⛔ | 插件 JSON-RPC 协议需进一步逆向；配置出现时警告跳过 |
| BitComet | ⛔ | AES 握手 + 仅 Windows；同上 |

## 4. 数据与配置

- **SQLite schema**：banlist/history/torrents/pcb_address/pcb_range(V1_5 ip_range)/alert/
  rule_sub_info/rule_sub_log/peer_records/tracked_swarm/traffic_journal_v3 —— DDL 与上游
  Flyway V1_1(+V1_5) 逐字一致；**Java 版的库可直接被 pbh-rs 打开**（单向兼容，迁移前备份）。
- **banlist.metadata / history.rule_name/description/structured_data**：Gson 兼容 JSON
  （epoch 毫秒、TC {key,params}、null 省略、字段序=父类在前）。
- **config/profile**：直读直写上游 v48/v40；写回保留全部注释（行级 round-trip 编辑器）；
  config-version 迁移脚本链未移植（版本不符时警告并继续）。
- **i18n**：上游 zh_cn/zh_tw/en_us messages.yml 全量内嵌；Lang 键名=枚举名。
- **ipdb/**、**sub/** 目录布局与上游一致。

## 5. 已知有意差异（均记录于代码注释）

1. 单 IP 手动解封：pbh-rs 立即置 needReApply（下一轮 ≤5s 全量重应用到下载器）；上游仅 `*`
   全量解封置位，单 IP 解封的下载器侧收敛依赖下一次自然变更（疑似上游缺陷）。
2. set_ban_list 失败时 pbh-rs 置 needReApply 于下一轮自动重试；上游仅告警。
3. GeoIP 下载/xz 解压在独立 OS 线程（上游在 JVM 后台任务）；对行为无影响。
4. peer_records/tracked_swarm 刷写间隔被夹在 5s~60s（上游 PBHCache TTL 180s / 配置 15~60min，
   数据内容一致，仅落库时机更早）。
5. `/api/general/status` 的 jvm 段以进程 RSS 近似（无 JVM）；weeklySessions 恒 0。
6. umami 遥测 / Sentry 崩溃上报不移植（衍生项目不回传上游）。
7. transmission 3.00 支持仅用于沙盒验证（`PBH_RS_TR_BYPASS_VERSION_CHECK=1`），生产目标 4.1+。
