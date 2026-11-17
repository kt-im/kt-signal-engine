# 链接时历史迁移（backup5）现状：密钥丢失后能否「从手机转移回来」

日期：2026-11-11 ｜ 性质：只读调查（未改任何代码、未跑构建、未跑真机）｜ 范围：kt-signal-engine / kt-signal-connector / kt-desktop

问题：**密钥丢失后，用户能否从手机把历史转移回来？**
本文只回答证据能支撑的部分；每条结论后附 file:line，未能取证的一律标「未取证」。

---

## 0. 方法与边界

- 只读：`git log/ls-tree/grep/sed` + 阅读 ADR、契约、parity 矩阵、handoff 文档。
- **未执行**：任何 `cargo build/test`、任何真机扫码、任何链路复跑。凡「已验证」均指仓内已记录的验收，不是本次跑的。
- 「在 origin/main」= `git merge-base --is-ancestor` / `git ls-tree origin/main` 实查；「只在分支」同样实查。

---

## 1. 官方 transfer / backup5：本仓研究到哪一步？

**结论：不是只有阅读笔记——引擎侧已实现并进了 origin/main；connector 已实现并进了 origin/main；桌面侧已实现但只在功能分支，不在 kt-desktop origin/main。**

### 1.1 研究/取证（笔记层）

| 项 | 证据 | 状态 |
|---|---|---|
| 官方机制逐点核实（QR 声明 backup5、信封带 ephemeralBackupKey、下载全量档案解密导入） | `kt-signal-engine/adr/0005-link-history-transfer-backup5.md:14-22` | 已记录（2026-10-03） |
| 三个未知数（真机配合性 / 下载端点 / presage 是否暴露字段）全部落定 | `adr/0005…md:35-43`、`:55-61` | S1 真机 spike PASS（`README.md:52-54`） |
| 解密与帧解析原语在依赖树内（libsignal v0.99.0 `message-backup` + `account-keys/backup.rs`） | `adr/0005…md:24-33` | 已核实 |
| 官方字段：`GET /v1/devices/transfer_archive?timeout=60` → `{cdn,key}` → CDN 下载 | `adr/0005…md:57-59`、实现 `kt-signal-engine/src/service/mod.rs:13470-13545` | 已实现 |

真机取证只覆盖了**首次链接的一次会话**：档案 6576 字节、sha256 记录在 `adr/0005…md:63-64`。**没有第二次链接（重链）的取证记录**（见 §3.2）。

### 1.2 实现层（ADR 自报阶段）

| 阶段 | ADR 自报 | 实查（本次） |
|---|---|---|
| S1 spike | ✅ 2026-10-03 | `kt-signal-engine/src/bin/backup5-spike.rs:1-10` 在树内；spike 结论见 ADR |
| S2 引擎（下载+解密+逐帧导出+链接集成） | ✅ 2026-10-04，`0eedc8c` | 代码在 `origin/main`（`0eedc8c` 是 origin/main 祖先）；`mod.rs:13470` / `history.rs:77` / `transfer_progress.rs` 均在 origin/main 树内 |
| S3 connector（契约 + 导入） | 「已落地（契约 1.39）」`adr/0005…md:130-132`、`README.md:54` | `kt-signal-connector/src/history_import.rs` 在 `origin/main` 树内（4371 行），消费链见 §2.2 |
| S4 桌面 UI + 真机验收 | 「下一步 S4」`README.md:83` | 桌面代码已落但**真机验收待做**（parity 矩阵 §6.5 标 🟡：`docs/plan/signal-official-parity-matrix.md:104`）；且桌面模块**不在 kt-desktop origin/main**（§2.3） |

**一句话**：研究不只是笔记；S1–S3 已是产品代码（引擎、connector 在主线），S4 是「代码在分支、真机未验收」。

---

## 2. 三仓是否有 transfer 的接收 / 存储 / 导入代码路径

有，且是完整链路：引擎下载解密 → 落 NDJSON → connector 消费入库 → 桌面观测/遮罩。

### 2.1 引擎（kt-signal-engine，origin/main）

| 环节 | 符号 | file:line |
|---|---|---|
| QR 声明 `backup5`（收敛成单个 `capabilities` 参数，防 Android 吞参数） | `qr_link_url` | `src/service/mod.rs:13117-13153` |
| 链接时截获 `ephemeral_backup_key` | provisioning 直调 + `ephemeral_backup_key` | `src/service/mod.rs:13259`、`:13348` |
| 链接收口后后台起下载任务（严格在旧任务清理之后） | `download_transfer_archive` spawn | `src/service/mod.rs:4037` |
| 长轮询 + CDN 下载 + 解密导出 | `download_transfer_archive` | `src/service/mod.rs:13470`；预算常量 `:13360` |
| 200 体四分类（ready / RELINK_REQUESTED / CONTINUE_WITHOUT_UPLOAD / malformed） | `parse_transfer_archive_body` / `handle_transfer_archive_body` | `src/service/mod.rs:13431`、`:13457` |
| 失败码词表（`no_archive`/`network`/`cdn_unreachable`/`decode_failed`/`io`/`relink_requested`/`continue_without_upload`） | `transfer_progress::failure` | `src/service/transfer_progress.rs:29-60` |
| 落盘产物 | `history-import.ndjson`（账号目录） | `src/service/snapshot_import.rs:13-15`、`:38`；清理名单 `transfer_progress.rs:198` |
| 解密派生（AEP → BackupKey → backupId → MessageBackupKey） | `derive_keys` | `src/backup/keys.rs:38`（派生链说明 `:6-14`） |
| 帧 → NDJSON 导出 | `export_archive` / `ExportStats` | `src/history.rs:77`、`:42` |
| 只读状态面 | `transfer.downloadStatus` | `src/methods.rs:3774`、`src/service/mod.rs:4314`、`docs/jsonrpc-compat.md:243` |
| 取消面 | `history.importCancel` | `src/methods.rs:3753`、`docs/jsonrpc-compat.md:244` |
| 云备份恢复腿（另一条恢复通道） | `backups.restoreFromCloud` / `backups.cloudStatus` | `src/methods.rs:3643`、`:3667`；`src/service/cloud_backup.rs:842`；`docs/jsonrpc-compat.md:274` |

**引擎没有的东西**：没有任何「重新拉一次 transfer archive」的方法——只有 `transfer.downloadStatus`（只读）、`backups.restoreFromCloud`（云备份腿）、`history.importCancel`。transfer 档案的唯一触发点是**链接握手**（`mod.rs:4037` 只在 finish_link 收口时 spawn）。

### 2.2 connector（kt-signal-connector，origin/main）

| 环节 | 符号 | file:line |
|---|---|---|
| 模块职责（消费引擎 NDJSON，两遍有界导入） | 模块头 | `src/history_import.rs:6-10` |
| 导入主入口 | `run_import` | `src/history_import.rs:271`；起跑落行 `:278` |
| 档案/云档案文件名 | `ARCHIVE_FILE_NAME` / `CLOUD_ARCHIVE_FILE_NAME` | `src/history_import.rs:54`、`:64` |
| 等待窗 60min（对齐引擎 ~1h 下载预算）+ 「无档案」语义 | `LINK_WAIT_MS` | `src/history_import.rs:74-88` |
| 失败重试预算 | `MAX_ATTEMPTS = 3` | `src/history_import.rs:62` |
| 探针（判「等/做/完」） | `probe_history_import` | `src/supervisor.rs:9683`；**行 completed 即 Done**：`:9716`；attempts 用尽即 Done：`:9717` |
| 单据导入取槽执行 | `claim_and_run_history_import` / `run_history_import` | `src/supervisor.rs:9736`、`:9761` |
| 链接后 watcher（5s 一探，最长 60min） | `spawn_history_import_watch` | `src/supervisor.rs:1584-1620` |
| 启动后台 sweep | 账号遍历探针 | `src/supervisor.rs:1328` |
| 状态投影（pending → no-archive/unavailable） | `history_import_status_projection` | `src/supervisor.rs:10034-10105` |
| 持久化 | `history_imports` 表 + 两张 staging 表 | `src/store.rs:417`、`:430`、`:441` |
| 账号删除时连坐清理 | `ACCOUNT_DELETE_TABLES` 含 `history_imports` | `src/store.rs:2363-2365` |
| 导入行起跑/收尾 | `begin_history_import` / `complete_history_import` | `src/store.rs:7150`、`:7191` |
| host 方法面 | `history.importStatus` / `importRetry` / `importCancel` | `src/host.rs:1854`、`:1875`、`:1896` |
| lane/能力位 | `history-import` 能力位、三方法 lane 声明 | `src/methods.rs:967`、`:211`、`:219`、`:228` |
| 导入历史的附件字节下载（step 3，transit CDN 链） | 模块头 + `HistoryAttachmentLocator` | `src/history_attachment.rs:3-8` |
| 契约 | §4.38 / §4.50 / §4.51 / §4.146 | `docs/implementation-plan.md:4679`、`:6144`、`:1346`、`:2687` |

### 2.3 桌面（kt-desktop）

**位置事实**：`electron/main/signal` **不在 kt-desktop `origin/main`**（`git ls-tree origin/main --name-only electron/main/signal` 空；origin/main = v3.6.59）。实现存在于 worktree/branch：`kt-desktop/.worktrees/signal-download-tiers`（HEAD `93ae9466`，`relinkBackfill.ts` 提交 `7cb0b282`），这类分支共 44 个（含 `fix/integration-next`）。

| 环节 | 符号 | file:line（相对 `kt-desktop/.worktrees/signal-download-tiers/`） |
|---|---|---|
| 重链成功后自动回填的状态机（武装 → 观察 → 云恢复 → 交接导入链） | `createRelinkBackfillController` | `electron/main/signal/relinkBackfill.ts:152`；头注 `:5-11`、`:13-22` |
| 武装来源恰两条清数据恢复流（KEY_MISSING / DATA_CORRUPT） | `arm` / `armSessions` | `relinkBackfill.ts:138`、`:161-172`；调用 `sessionHost.ts:3663-3668` |
| link 成功入口 | `onLinkEstablished` | `sessionHost.ts:1093`、`:1172` |
| 转移判定窗：`history.importStatus` = pending 等、running/completed/failed 跳过、unavailable → 起云恢复 | `stepWatch` | `relinkBackfill.ts:251-301`（判据分支 `:268-292`） |
| 云恢复启动与错误分类（`BACKUP_NOT_PROVISIONED` → 诚实 skip） | `attemptRestore` | `relinkBackfill.ts:307-346`（skip 分支 `:330-334`） |
| 清钥清数据（key_missing 唯一出口） | `purgeLostKeyLocalState` | `connectorSupervisor.ts:784`；删 `signal-data` 全目录 `:802-803` |
| 数据损坏重置 | `resetCorruptLocalData` | `connectorSupervisor.ts:828` |
| host 面三方法透传 | `history.importStatus/Retry/Cancel` | `hostAdapter.ts:7223`、`:7247`、`:7278` |
| IPC 白名单 | 三方法 | `types.ts:412-427`、`registerIpc.ts:1497`、`:2306-2324`、`:2372-2409` |
| 全遮罩导入屏 + 终态卡 + 重试钮 | UI | `src/view/signal/SignalWorkspace.vue:1077-1114`、`:2958` |
| 迁移问询（官方 MaybeTransferModal 对位） | 八键文案/双钮 | `docs/plan/signal-official-parity-matrix.md:1388-1391`、`:1401` |

**桌面没有的东西**：没有「手动重新传输」入口。桌面只有观测权（矩阵明写「桌面**无触发权只有观测权**」，`signal-official-parity-matrix.md:3470`）；重新触发的唯一动作是**重链扫码**（`registerIpc.ts:2306-2324` 清数据 → 扫码）。

### 2.4 一句话数据流

```
手机（链接并同步上传） → engine: GET /v1/devices/transfer_archive → CDN 下载 → 解密分帧
  → <account_dir>/history-import.ndjson
  → connector: probe/watch → run_import（两遍有界，稳定 id 去重）→ connector.sqlite3
  → 桌面: history.importStatus 轮询 / 遮罩 / 终态卡 / 重试
另一条腿：engine backups.restoreFromCloud（云备份）→ history-import-cloud.ndjson → 同一条导入链
```

---

## 3. 关键限制（决定「能不能转移回来」的东西）

### 3.1 transfer 是「链接握手的一次性通道」，没有独立重下发

- 引擎只在 `finish_link` 收口时 spawn 下载（`src/service/mod.rs:4037`），档案钥匙来自该次 provisioning 信封（`:13259`）。
- 引擎方法表只暴露 `transfer.downloadStatus`（`src/methods.rs:3774`）——只读。没有「再传一次」RPC。
- 因此「再拿一次历史」= **再链接一次**（新扫码），不是点个按钮。

### 3.2 重链能否再传：仓内**自相矛盾且未取证**

| 说法 | 出处 | 方向 |
|---|---|---|
| 「仅首次链接可导；重链/已注册安装不可再导（官方同款语义）」 | `kt-signal-engine/adr/0005…md:137` | 重链**不可**再导 |
| 「引擎恒声明（ADR 0005 每次新链接恒导入）」 | `kt-desktop/.worktrees/signal-download-tiers/docs/plan/signal-official-parity-matrix.md:3425`（同引 ADR 0005） | 每次新链接**恒**导入 |
| 官方文案（en/zh 逐字）："When you re-link this desktop, you can transfer your phone's message history to this device."／「重新链接此桌面电脑后，您可以将手机里的消息记录转移到此设备。」 | `signal-official-parity-matrix.md:1389`（Signal-Desktop `_locales` v8.31.0-alpha.1 实读） | 重链**可**转移 |
| 官方流程本身：解绑横幅 → MaybeTransferModal → Transfer → 清数据重链 | `signal-official-parity-matrix.md:1388`、`:1397`、`:1408` | 重链**可**转移 |
| 「密钥丢失……不可逆、不可恢复、**不能靠重扫码救回历史**」 | `kt-signal-engine/docs/plan/engine-store-encryption.md:424-432`（同 `:24`） | 重链**不能**救回 |

本次调查**没有找到任何真机/日志证据**证明「第二次链接时手机确实再次上传档案」。S1 只做了首链一次（`adr/0005…md:53-64`）。
→ **判定：重链再传属「代码按每次链接请求 + 官方文案支持，但本仓未取证」**，不能当既定事实写进对外承诺。

### 3.3 桌面侧真实约束：必须先清本地数据（key_missing 唯一出口）

- 清钥清数据只允许在 `key_missing` 态（`connectorSupervisor.ts:788-803`），且**绝不自动调用**（`:780-783` 纪律注释；入口仅 `registerIpc.ts:2306-2324`）。
- 清的是整个 `signal-data` 目录：connector 库（历史 + 草稿/备注/未读等本地缓存）+ 引擎数据目录一起没（`connectorSupervisor.ts:564`、`:802-803`）。
- 回填只对**两条清数据流**武装（`relinkBackfill.ts:5-11`）：KEY_MISSING、DATA_CORRUPT。**首次关联 / device_unlinked 横幅重链 / 迁移问询「Transfer」都不武装**（同段注释：未武装零动作；两者走的是「导入面观测」而非自动云回填）。
- 即：用户「丢 key → 清数据 → 重链」后，能否拿回历史取决于**这次链接手机是否上传档案**（§3.2）或**是否开通了 Signal 云备份**（§3.4）。

### 3.4 云备份腿有前置门，未开通就什么都没有

- 桌面在 `history.importStatus` 为 `unavailable`（`no-archive`，即手机没传/没得传）时才起云恢复（`relinkBackfill.ts:286-292`）。
- `BACKUP_NOT_PROVISIONED` → `skipped(no_cloud_backup)`，不出错误面（`relinkBackfill.ts:330-334`）。**没买/没开 Signal Secure Backups 的用户这条腿等于不存在。**
- 云恢复本身需要引擎侧 AEP 派生备份密钥（`src/backup/keys.rs:9-12`），引擎侧实现存在（`cloud_backup.rs:842`），但真机验收同样未见记录（矩阵 §6.5 仍 🟡）。

### 3.5 拿回来的历史不是「原样」

| 缺口 | 证据 |
|---|---|
| 媒体：transit 指针有 45 天窗，窗外只有 metadata，无字节 | `contracts/signal-host-adapter.md:1034`；官方文案同口径（「未订阅 Secure Backups 则只含最近 45 天媒体」）`signal-official-parity-matrix.md:1389` |
| reactions 不投（v1 未立项） | `kt-signal-engine/docs/jsonrpc-compat.md:2906`、`:2933` |
| 若干 item 类型帧级/整 item 跳过（未知帧、身份残缺、零信息量等），只计数不致命 | `src/history.rs:130`、`:145`、`:1599`、`:2037`（贴纸/联系人/群更新/通话已在 B1 批补齐，见 compat §5） |
| KT 自有本地数据（草稿、备注、未读等）不随 transfer 回来，随 purge 一起清掉 | `connectorSupervisor.ts:775-803`（清理整个 signal-data） |

### 3.6 导入行「已完成」会挡住新档案（重链不清库时）

`probe_history_import` 见 `row.state == "completed"` 直接 Done（`kt-signal-connector/src/supervisor.rs:9716`）。purge 会连库带行一起删（`store.rs:2363-2365` 账号删除连坐），所以 purge 路径没问题；但「不清库、只重链」的情形下，新档案不会被二次导入——这是设计使然（同一 Signal 消息稳定 id 去重），但**没有产品文案说明**。

---

## 4. 缺口清单与工作量（估算，非承诺）

> 估算依据：改动面（几仓/几文件）+ 仓内既有同类批的规模（例如 G2b 迁移问询单批 = 桌面 1 组件 + 12 spec）。人日为**工程估算**，真机等待另计。

| ID | 缺口 | 证据 | 动作 | 估算 |
|---|---|---|---|---|
| B1 | 重链再传从未取证（首链之外零记录） | `adr/0005…md:53-64`、`:137` vs `matrix:1389` | 复用 `src/bin/backup5-spike.rs` 做「同号第二次链接」取证（信封是否有 key、手机是否上传、档案是否第二次可得），结论回写 ADR | 0.5–1 人日 + 真机 |
| B2 | S4 真机验收未做（首链端到端导入也没真机记录） | `matrix:104` 🟡「真机验收待做」、`docs/plan/signal-acceptance-runbook.md:204` | 按 runbook 走 L-1 真机：扫码 → 全遮罩 → 计数卡 → 重启续导 | 1–2 人日 + 真机 |
| B3 | 文档口径三方矛盾（ADR 137 / matrix 3425 / 加密计划 424-432） | 见 §3.2 | 定一份权威口径（建议：按「每次新链接请求，手机是否上传以真机取证为准」），改 ADR 0005 §风险、矩阵 §6.5、加密计划 §6.1 | 0.5 人日 |
| B4 | 无「重新拉历史」入口（只有重链） | `mod.rs:4037`、`methods.rs:3774` | 若产品要按钮：engine 增 transfer 重下发 RPC（需手机侧愿意配合，实际仍等价重链）→ 三仓契约 + UI + 编排 | 3–5 人日（且多半被 B1 结论否掉） |
| B5 | 云备份腿前置门未产品化（未开通用户无兜底） | `relinkBackfill.ts:330-334`、`engine-store-encryption.md:447` | 桌面把 `no_cloud_backup` 变成可解释文案 + 引导（开通 Signal 备份 / 用官方桌面） | 1 人日 |
| B6 | 45 天媒体窗与 reactions 缺口的诚实 UI | `signal-host-adapter.md:1034`、`compat:2906` | 超出窗的媒体显示「已过期，不可下载」占位；reactions 缺位文案或立项补齐 | 1–2 人日（媒体文案）；reactions 补齐 3–5 人日/仓批 |
| B7 | 桌面模块不在 origin/main | `git ls-tree origin/main electron/main/signal` 空；44 个分支含实现 | 确认集成分支（`fix/integration-next` 等）→ 合流 → 回归门禁 | 1–2 人日集成 + 回归 |
| B8 | 「已 completed 的导入行挡住新档案」无说明 | `supervisor.rs:9716` | 文案/诊断说明，或在重链时显式清行（需契约） | 0.5–1 人日 |

**合计量级**：把「能不能转移回来」从「代码写好了」推进到「可对外承诺」，最小闭环 = B1 + B2 + B3 + B5 + B7 ≈ **5–8 人日**（不含真机排队与 reactions 补齐）。B4 不建议先做。

---

## 5. 结论：「G-A 里『可从手机转移回来』」当前是否成立

### 5.1 G-A 原文是什么

- G-A 定义：`kt-signal-engine/docs/plan/engine-store-encryption.md:737`
  > **G-A** ｜ 合规/产品口径：谁需要「引擎库静态加密」；**是否接受「KEK 丢 = 历史不可读」**；是否接受「历史明文残留不可安全擦除」
- 同文 §6.1 写得比 G-A 更硬（`:424-432`）：
  > 该账号历史数据**永久不可读**，且不可逆、不可恢复、**不能靠重扫码救回历史**。
- 同文 `:24`：「密钥丢失 = 数据永久不可读」。

**本次在仓内没有找到 G-A 处「可从手机转移回来」这句原文**；现有原文方向相反（不可恢复、不能靠重扫码）。若这句话来自评审/讨论而非该文档，则它**与文档现文冲突**，不能同时成立。

### 5.2 判定

**作为无条件承诺：不成立。** 依据（全部实查）：
1. transfer 没有独立重下发入口，只能重链（`mod.rs:4037`、`methods.rs:3774`）。
2. 重链能否再传**本仓零取证**，且 ADR 0005:137 与官方文案/矩阵互相矛盾（§3.2）。
3. 真机验收（S4）未做（`matrix:104` 🟡）。
4. 桌面侧实现不在 origin/main（§2.3）。
5. 即便成功，媒体 45 天窗外、reactions、KT 本地缓存都不会回来（§3.5）。

**作为有条件路径：结构上成立，条件全部满足才有戏。** 条件 = ①用户愿意清本地数据并**重新扫码链接**；②手机在线、app 支持 backup5、**且确实再次上传档案**（未取证）；③若手机不传，用户**已开通 Signal Secure Backups**（否则 `BACKUP_NOT_PROVISIONED` 诚实跳过）；④用户接受媒体/反应等缺口。

### 5.3 丢 key 之后，用户真实可选项（按现场分）

按「丢的是哪把钥匙」分，选项不同——这是本文最实用的部分：

| 现场 | 现状事实 | 用户可做什么 | 拿得回什么 |
|---|---|---|---|
| **connector store key 丢**（`key_missing`；桌面 keychain 重置/换机） | 桌面唯一出口 = 清钥清数据（`connectorSupervisor.ts:784-803`），然后重链 | 设置页「重新链接并迁移」→ 扫码 → 等导入屏 | 若手机再传：Signal 历史（文本/贴纸/联系人/群更新/通话行等，见 compat §5）；媒体仅 45 天窗内；**草稿/备注/未读不回** |
| **手机没再传档案**（`no_archive`，`transfer_progress.rs:38`） | `history.importStatus` → `unavailable(no-archive)`（`supervisor.rs:10086`） | ① 若已开通 Signal 备份：桌面自动走 `backups.restoreFromCloud`（`relinkBackfill.ts:286-292`）② 否则回到「手机里还有历史」这条现实兜底（下一行） | 云备份里的历史（含媒体视套餐）；否则空 |
| **没开通 Signal 备份 + 手机不传** | 桌面 `skipped(no_cloud_backup)`（`relinkBackfill.ts:330-334`），不出错误面 | **用官方 Signal Desktop 链接同一台手机**：官方 link-and-sync 会把手机历史传到官方桌面（这正是 `matrix:1388-1391` 逐字取证的官方机制）。KT 侧不参与此路径 | 官方桌面里的完整历史；KT 本机仍空 |
| **手机端历史本身没有了**（消失消息到期、保留策略、换号） | 无证据表明 KT 能凭空取回 | 无 | 无（诚实告知） |
| **只是引擎 store 坏、connector 库完好** | 历史仍在 connector.sqlite3；重链后旧导入行若为 completed 会被探针跳过（`supervisor.rs:9716`），不会重复导入 | 重链恢复收发即可（是否影响 connector 账号行未在本次调查范围，标未取证） | 历史仍在（无需转移） |

**明确不可恢复/不可替代的部分**（写文案时别许愿）：
- KT 本地缓存层：草稿、备注、未读等（随 purge 删，transfer 不补）。
- 超出 transit 指针 45 天窗的历史媒体字节（只有 metadata）。
- reactions 等 v1 未投影行（`compat:2906`）。
- 「不重链就想把历史拉回来」：没有任何入口。

---

## 6. 复盘：本次调查**没能**证明的事（不要当结论用）

1. 第二次链接时手机是否上传档案 —— 无真机记录、无日志、无 spike 复跑。
2. 重链（同号）对 connector 账号行/导入行的实际影响 —— 只读到 purge 路径删整目录的行为，未实跑。
3. 云备份 restore（`backups.restoreFromCloud`）在真机的成功率与耗时 —— 只有代码与契约，无验收记录。
4. 45 天媒体窗在 KT 链路里的实测边界 —— 依据是契约注释（`signal-host-adapter.md:1034`）与官方文案，未实测。
5. 桌面 Signal 模块最终合入哪个分支/哪个版本 —— 只确认 `origin/main` 没有、44 个分支有。

---

## 7. 证据索引（本次实际执行过的只读命令）

- `git -C kt-signal-engine merge-base --is-ancestor 0eedc8c origin/main` → 0（是祖先）
- `git -C kt-signal-engine ls-tree origin/main -- src/service/transfer_progress.rs …` → 在树
- `git -C kt-signal-connector ls-tree origin/main -- src/history_import.rs src/history_attachment.rs` → 在树
- `git -C kt-desktop ls-tree origin/main -- electron/main/signal` → 空（v3.6.59）
- `git -C kt-desktop branch -a --contains 93ae9466 | wc -l` → 44 个分支
- `grep -rn 'backup5|transfer_archive|ephemeral_backup_key|history.import' `（三仓）→ §2 各表
