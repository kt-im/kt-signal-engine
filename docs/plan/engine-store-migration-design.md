# M6 D 阶段：engine 明文 `store.db` → 加密库迁移器设计（只读）

> **状态**：设计稿 v1。**D1（正向）与 D2（反向）已按本文落地**（工作区未 commit，落点与契约增量见 §15）；
> 正文保持设计口径，§13.5/§13.8 已标注 D2 的实测结论。
> **日期**：2026-11-11
> **本文所在仓库**：kt-signal-engine（HEAD `c4a96f4`，工作区含未提交的 `docs/plan/*`；本文 `file:line`
> 以**本机当前工作区**为准）
> **上游/前置文档**：`docs/plan/engine-store-encryption.md`（M6 评估稿，P0 已落地）、
> `docs/plan/engine-pool-per-account-design.md`（每账号引擎进程）、ADR 0002/0004
> **交叉引用仓库**：kt-signal-connector（`fix/delete-sync-conversation` @ `d491396`，本文引用的迁移实现
> 以该 commit 为准）、kt-desktop（key 保管链）
> **边界**：本文只出设计与验收线。**不写实现代码**；SQL / shell 片段是设计记法，不是可执行交付物。
> 所有"验收命令"都是**待执行**；标"未实测"的是推断，不是实测数据。跨仓改动（connector 编排）走各自仓库流程。

---

## 0. 一页结论

| # | 问题 | 结论 |
|---|---|---|
| 1 | 正向迁移怎么做 | 照 connector 已落地的五步（`src/store.rs:11752-11806`）：清残留 → `wal_checkpoint(TRUNCATE)` → `VACUUM INTO` 备份 → `ATTACH … KEY` + `sqlcipher_export` → 删 sidecar + `rename` 原子替换。**但不照抄三处细节**：参考实现没有校验层、checkpoint 的 busy 结果被丢掉、日志用 INFO（引擎滚动日志钉 `WARN`，`src/logfile.rs:57`）。D 必须补校验矩阵、读取 checkpoint busy、证据行走 WARN。 |
| 2 | 迁移器放哪 | **独立 argv 模式 `storeMigrate`（同一二进制）**，由 connector 在该槽引擎**停机之后、spawn 之前**调用。**不放进 `open_store`**（72 处调用点、接收路径 90s 超时，`src/service/mod.rs:9122`），**也不做引擎启动内联迁移**（连接器请求 15s / watchdog 30s 会把它当卡死并重启，正好制造"迁移中被杀"）。 |
| 3 | 幂等与崩溃恢复靠什么 | **靠文件头分类，不靠状态文件**：`store.db` 前 16 字节是否 `SQLite format 3\0` 是唯一权威（connector 同款判定 `src/store.rs:11698-11717`）。`store.format` 只做审计/指纹，**永不参与正确性判断**——这样"rename 成功、marker 未写"这个窗口直接消失（本文对 C 文档 §4.6 的明确修正）。 |
| 4 | 反向迁移为什么必须有 | ① **二进制回退**：旧引擎无 key 能力，加密库读不了，回退不能要求用户重扫；② **kill switch**：加密上线出问题时需要"回到明文"的可执行路径，而不是现场写代码；③ 口径/合规变化时把数据迁回明文登记；④ 正向迁移自身的最后退路（备份已过期时）。**它不解决"key 丢了"**——没有 key 就没有反向，这条必须写进文案。 |
| 5 | 与 C 阶段 env key | C 已落地（工作区未 commit）：env **`KT_SIGNAL_STORE_KEY`**、`StoreKey`/`StoreKeyState`、`store_open::classify`、`store_connect_options_with_key`。D **不产生、不派生、不落任何 key**，只复用这四个面，并给出**一进程一把 key**的两条边界（同一 data-dir 多账号共用 key；隔离靠进程边界）。硬不变量：**加密用的 key 必须就是之后打开用的 key**，靠 marker 指纹（域分离哈希前 8 字节）在启动时比对，不一致终态拒绝、不重建。D 只要求 C 改一处（**已落地**）：明文库的「warn 后无 key 打开」改成**默认 off 的开关**——`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on` 时明文库（与有无 key 无关）走终态 `StorePlaintextRequiresMigration`，off 时维持 legacy 打开（§7.4 C-1）。 |
| 6 | 与 per-account 引擎进程 | 池化后一个 data-dir 恰好一个账号（池设计 §3.2 I3），D 的"每账号独立"**由结构保证**；时序是"停槽 → 迁移 → 起引擎"，迁移并发上限 1（I/O 放大 2×）。引擎启动**只检测不迁移**：开关 off（默认）时明文库按 legacy 打开（有 key 时 warn 一次），开关 on 时**该账号**终态停环——两条都不会在启动段迁移，避免两条迁移路径。 |
| 7 | 工作量 | **engine 侧 7.5–9.0 人日 + connector 编排 1.0–1.5 + 灰度演练 1.0 ≈ 9.5–11.5 人日**（C 文档 P2 的 5–8 人日没含反向、崩溃矩阵、跨仓编排，见 §11）。 |
| 8 | 磁盘峰值 | 需要 **约 3.0× 库体积**（原库 + 明文备份 + 加密 staging），不是 C 文档 M9 写的 2.2×。预检按 `2.1×L + 32 MiB` 的**新增**空间判（§9 D-01）。 |

---

## 1. 事实基线（已核实，全部带 file:line）

### 1.1 engine（本仓）

| 事实 | 证据 |
|---|---|
| store 是 bundled SQLCipher 构建，运行时能力门已落地 | `Cargo.toml:9-23`（feature 门）、`src/store_cipher.rs:209`（进程唯一门）、`:233`（`ensure_cipher_capable`）、`:256`（启动证据行） |
| 启动顺序：日志 → 能力证据 → perf 钩子 → 清扫 → 注册表 → 单账号护栏 | `src/main.rs:483,489,493,497,501,508` |
| store 唯一打开点（门 → 建目录 → 建库 → 跑迁移） | `src/service/mod.rs:14700`（`open_store`）、`:14718`（`open_store_gated`）、`:14730-14731`（`dir.join("store.db")`） |
| 连接选项：`create_if_missing` + `foreign_keys` + `cache_size=-1024`，**无 key** | `src/service/mod.rs:14686-14692` |
| `open_store` 调用点 72 处（含内联测试），非测试文件 7 个：`service/mod.rs`、`profile_refresh`、`groups`、`own_capabilities`、`snapshot_import`、`send_warmup`、`backup_task` | `grep -rn 'open_store(' src` |
| 接收建流上限 90s（迁移塞进打开路径会被判 Recoverable 反复重试） | `src/service/mod.rs:9122`（`RECEIVE_CONNECT_TIMEOUT`）、终态枚举 `:8478`、`Recoverable` 分支 `:9194` 一带 |
| 账号目录/文件名常量、隔离区 | `src/state.rs:63,64,67`（`ACCOUNTS_DIR`/`STORE_DB_FILE`/`QUARANTINE_DIR`） |
| 注册表原子落盘范式（同目录 tmp + rename） | `src/state.rs:156-185`（`persist`） |
| 启动清扫只在注册表缺失时跑，且要**打开** store 才能判定"未注册" | `src/service/mod.rs:14796-14831`（`cleanup_abandoned_link_stores`）、`:14744`（`remove_unregistered_link_store`） |
| 严格 argv：未知参数退出 2 | `src/main.rs:123-165`（`USAGE` / `LaunchArgs` / `parse_launch_args`） |
| 滚动日志只收 `WARN` 及以上（INFO 不落盘） | `src/logfile.rs:57`（`MAX_LEVEL`） |
| 指标词表封闭、有漂移单测 | `src/service/perf_metrics.rs:30-70`（常量）、`:227`（`record_us`）、`:258`（`snapshot_json`） |
| 进程级真进程夹具（迁移的命令级验收可直接复用） | `tests/contract.rs:28-49`、`:258`（`CARGO_BIN_EXE_kt-signal-engine`） |
| 引擎 store 里有身份私钥/会话/棘轮/prekey/群 sender key/联系人/消息 | `vendor/presage/presage-store-sqlite/migrations/20250112201436_init.sql:1,4,12,20,27,34,42,52,74,110,116` |
| 库内 schema 迁移是 `_sqlx_migrations` + `sqlx::migrate!`（加密不改变这套） | `vendor/presage/presage-store-sqlite/src/lib.rs:86` |
| **C 阶段已并发落地（工作区未 commit）**：key 三态 + 文件分类 + 错误码 + 打开矩阵 | `src/store_key.rs:37,70-81,95-105,121-150`、`src/store_open.rs:56-66,91-125,207-268`、`src/service/mod.rs:14750-14755,14788-14850` |
| 现状：明文 + 有 key = **warn 一次后按无 key 打开**（C-1 已落成默认 off 的开关：on 才终态，见 §7.4） | `src/service/mod.rs:14926-14931`、`src/store_open.rs:186-200`、`:94-158`（开关解析） |

### 1.2 参考实现（connector @ `d491396`）

| 事实 | 证据 |
|---|---|
| 明文/加密分类只看文件头 | `src/store.rs:11698-11717`（`db_file_state`）、`:95`（`SQLITE_PLAINTEXT_HEADER`） |
| 大小 0 的文件当"不存在"处理 | `:11705-11709` |
| 带 key 打开 + 强制首页读，`NotADatabase` 归一成 `StoreKeyRejected` | `:11720-11745` |
| 迁移五步（本设计的参照物） | `:11752-11806`：清残留 `:11758-11760`；unkeyed 打开 `:11762`；checkpoint `:11766`；`VACUUM INTO` `:11769`；`ATTACH … KEY ?2` `:11776-11779`；`sqlcipher_export`+`DETACH` `:11782`；`remove_sidecars` `:11788`；`rename` `:11789`；失败只清 staging `:11798-11804` |
| 打开决策表（无 key+明文 → 拒；有 key+明文 → 迁移；加密+错 key → 拒） | `:2450-2465` |
| 备份后缀 `.plaintext-backup`、staging 后缀 `.encrypting`、保留期 7 天 | `:62`、`:64`、`:66` |
| 备份到期清理（mtime 判定） | `:2578-2601`（`prune_expired_plaintext_backup`） |
| 副作用夹具：`secure_delete=ON`、WAL 上限 | `:2474-2479` |
| 迁移与失败收敛的测试 | `:22809`（同数据迁移）、`:22911`（失败保原文）、`:22895`（错 key 拒绝）、`:22931`（保留期） |
| 错误码 | `:564`（`StoreKeyRejected`）、`:568`（`MigrationFailed`） |

### 1.3 SQLCipher 行为（amalgamation，本机 registry）

| 事实 | 证据 |
|---|---|
| `sqlcipher_export` 是 SQL 函数：copy 表定义 → copy 数据 → **copy `sqlite_sequence`** → copy view/trigger | `libsqlite3-sys-0.36.0/sqlcipher/sqlite3.c:112014-112170`；schema 复制排除 `sqlite_sequence` `:112072`；数据复制 `:112103`；序列复制 `:112113-112117` |
| `sqlcipher_export` **不复制** `user_version`/`application_id`（是文件头字段，不是 schema 对象） | 同上函数体全程无该项 |
| `PRAGMA cipher_plaintext_header_size` 存在（>0 会让加密库保留明文头） | `sqlite3.c:111124-111134` |
| `PRAGMA cipher_integrity_check` 按连接/库校验页 HMAC | `sqlite3.c:111453-111457` |
| raw key 规格 `x'<64hex>'`，跳过 PBKDF2；经 sqlx `.pragma()` 时要**带双引号** | 引擎 P0 测试实测口径 `src/store_cipher_tests.rs:136-145`；connector 形态 `src/store.rs:640`、`:11724-11727` |

> **口径修正**：C 文档 §4.3 把 `kv` 等表列成固定清单。实际 schema 随 10 个迁移漂移（`kt_outbox` 在
> `20261107000000` 建、在 `20261110120000` 删），**固定表清单会随上游迁移腐坏**。D 的校验必须
> **从库里读 schema 集合再比**（§4）。

---

## 2. 迁移单元、放置位置与命令表面

### 2.1 三条被否掉的位置

| 候选 | 为什么否 |
|---|---|
| A. 塞进 `open_store`（connector 就是这么做的，`src/store.rs:2455-2458`） | 引擎的 `open_store` 有 72 处调用点（含测试），接收路径套着 90s 建流上限（`src/service/mod.rs:9122`）且失败被映射成 `Recoverable`（`:9194` 一带）——大库迁移超时后会被**反复重试迁移**，每次都把 `VACUUM INTO` 再跑一遍。参考实现能这么干是因为 connector 自己拥有进程与打开时机，引擎不是。 |
| B. 引擎启动内联迁移（`main.rs:483` 之后、`:501` 之前） | 引擎"启动完成"是连接器的 spawn 判据：请求超时 15s、watchdog 拍 30s（池设计 §6.6）。把一次 10–120s 的 I/O 放进启动段 = 让 watchdog 在导出中途重启引擎，**亲手制造本文最想消灭的崩溃窗口**。 |
| C. 独立可执行文件（第二个 bin） | 常量（`ACCOUNTS_DIR`/`STORE_DB_FILE`/`QUARANTINE_DIR`）、能力门、key 解析、连接选项全在 bin 内部（本仓无 lib target），拆 bin 等于复制一套判定源，长期必然漂移。 |

### 2.2 决策：同一二进制的新 argv 模式

~~~text
kt-signal-engine --data-dir <DIR> storeMigrate [--account <lk-hex>] [--direction to-encrypted|to-plaintext]
                                                  [--dry-run] [--json]
~~~

- 与现有 `jsonRpc` 模式**互斥**；未知参数/两种模式同时出现仍 exit 2（`src/main.rs:123-165` 的严格口径不变）。
- **子命令只认非取值位置**：`--data-dir` / `--receive-mode` / `--account` / `--direction` 的取值位置不参与子命令识别——
  一个叫 `storeMigrate` 的 data-dir（`--data-dir storeMigrate jsonRpc`）必须照常按 `jsonRpc` 启动，否则合法启动被误判成迁移模式 exit 2。
- 模式内**不读 stdin、不写 stdout 协议帧**；`--json` 时 stdout 只输出一份结果 JSON（一行），其余证据走滚动日志与 stderr。
- 同一份 `main.rs` 装配：先 `logfile::init`（`:483`）→ 能力门证据（`:489`，SQLCipher 构建断言，防"feature 没开、`PRAGMA key` 静默 no-op"）→ 再进迁移器。**能力门在迁移器里是硬门**，不是提示。

`--json` 的结果契约（连接器只解析这一份，不解析人读文案）：

| 字段 | 类型 | 含义 |
|---|---|---|
| `ok` | bool | 全部账号成功才 true |
| `dryRun` | bool | 本次是不是干跑（顶层镜像，与 `accounts[].action=dry-run` 同一事实；连接器不必逐账号推断） |
| `accounts[]` | array | 每个账号一条：`lk`（目录名）、`action`（`migrate`/`dry-run`/`noop`/`absent`/`failed`）、`code`、`bytes`、`elapsedMs`、`keyid`、`backup`（文件名或 null；**干跑恒 null**）、`checks`（本次实际通过的校验项名） |
| `freeBytes` / `needBytes` | number | 预检读数（失败时是证据，不是猜测） |
| `plan[]` / `did[]` | array | **步骤名数组**（`checkpoint`/`backup`/`export`/`verify`/`swap` 的规范顺序）。多账号取并集；`--dry-run` 只有 `plan`（且 `action=dry-run`、`code=store_migration_planned`、`backup=null`），实跑两者都有，便于对账 |
| `error` | object（**仅跑不起来时在场**） | `{code, retryable}`：data-dir 不存在、迁移锁被占等**账号级之前**的整体失败。`retryable=false` ↔ 终态 11，`retryable=true` ↔ 可重试 10；成功路径整键缺席（不投 `null`） |

> **`plan[]`/`did[]` 的口径（别把它读成「每账号恒五步」）**：顶层两个数组只从五个规范步骤名里取
> （实现里按 `STEPS` 过滤后对多账号求并集），连接器按字面量逐项对账不会看到第六个名字。
> **逐账号的 `did` 已经不是纯五步**：幂等/收敛路径另记 `cleanup`（清孤儿 staging）、
> `marker`（补/改审计 marker）、`prune_backup`（过期备份清理）——这些标签只留在实现的
> 账号结果里（干跑用例断言 `did` 为空就是靠它），**既不进顶层数组也不进 `accounts[]`**，
> `--json` 里读不到它们。

为什么单独一个模式而不是启动内联：迁移器要能被**连接器按槽单独计时/单独失败/单独重试**，也要能被支持人员拿同一个二进制在副本上演练（`--dry-run`）。它不该继承引擎的请求超时、watchdog 与重启预算。

### 2.3 谁在什么时候调它

~~~text
connector.supervisor.start_slot(account)
  1) 取 DataDirLock(engines/<accountId>)              # 池设计 §6.5：datalock.rs:9-27
  2) 槽内引擎已在跑 → 直接起（`already_running`，不迁移）；否则先停槽再迁：
     stdin EOF → exit 0；停不下来 = **可重试 10**（`store_busy`，不锁存），必要时 SIGKILL 兜底
  3) 开关 off（默认）→ 跳过；dry-run / apply → 跑迁移器：
       spawn: kt-signal-engine --data-dir <dir> storeMigrate [--dry-run] --json
       env: KT_SIGNAL_STORE_KEY=<该账号派生、与随后引擎完全相同的值>
       budget: 迁移预算（缺省 120s/账号，独立于 15s 请求超时）
       # `--account` 是**可选**参数（只迁一个目录，人工演练 / repro 用）；
       # 连接器**不使用**它：池化后一个 data-dir 恰好一个账号，枚举整个目录
       # 就是那个账号；传错 lk 只会把「迁了这个、漏了那个」变成静默成功。
  4) 解析退出码（唯一可编程判据，§2.4）：0 继续；10 → 本次不起引擎、下次起槽重试；
     11 → **锁存** `StoreMigrationBlock`，该槽永不再起，对外一律
     `CAPABILITY_UNAVAILABLE(retryable=false)` + 可执行文案；2 → 集成 bug
     （不锁存、本轮不重试）
  5) spawn: kt-signal-engine --data-dir <dir> jsonRpc ...
~~~

**三档，一个开关（`KT_ENGINE_STORE_MIGRATE`，连接器侧 env，引擎不读它）**：

| 值 | 行为 | 用在哪 |
|---|---|---|
| `off`（**默认**，也是**认不出的值的归宿**） | 完全不动：不 spawn 迁移器、不读 store 一个字节、账号状态零变化；引擎照旧启动 | 未上车的部署；事故熔断 |
| `dry-run`（别名 `dry_run`/`dryrun`） | 起槽前（引擎没在跑时）跑一次迁移器 `--dry-run --json`（只出计划、零字节写入），**无论它答什么都照常起引擎**（观察模式；计划失败只改日志标签，绝不用它拦服务） | 灰度前出计划、迁移前对账 |
| `apply` | 槽内引擎**没在跑**时才迁移：用该账号派生的 key 跑一次迁移器，成功（exit 0）再起引擎 | 存量明文库收敛期 |

- 取值解析严格：`off` / `dry-run` / `apply`（去空白、大小写不敏感）；**拼错 = off + 一条 WARN**
  （"什么都不做"是唯一安全的失效方向——这条路径有破坏能力，笔误不该把连接器带下线，
  也不该把一批账号打成终态）。
- `apply` 的三种成功结局：`migrate`（真发生了一次替换）/ `noop`（已是密文）/ `absent`（无库）。
  只有 `migrate`/`noop` 记「本进程内该槽已迁移」，后续起槽跳过整库校验收口（那是迁移里最贵的一半）；
  `absent` **不记**——新库由引擎首起时自己建。
- `apply` 的两条跳过（不是失败）：账号目录还不存在（新账号，别让 `data_dir_missing` 把它一上来就锁死）；
  以及拿不到**该账号派生**的 key（遗留多账号进程 / 无 root key）——用别人的派生去开这个库正是 key 模型禁止的跨账号解锁。
- 闸的顺序固定，别随便重排：**off 早退**（一个文件都不读）→ 引擎已在跑（`already_running`）→
  本进程已迁移（`already_migrated`）→ 锁存（`blocked`，直接拒、不 spawn）→ 目录还没有 / 没有该账号的
  派生 key（`skipped`）→ **停槽 → 迁移 → 起引擎**。"没有引擎持库"与"没有引擎打开迁移器还在写的库"
  是这条顺序要同时保证的两件事。
- 迁移预算独立：`KT_ENGINE_STORE_MIGRATE_BUDGET_MS`（缺省 120s，下限 200ms，非法值回缺省 + WARN），
  刻意不继承 15s 请求超时（§8.2）。

**「lazy」不是本开关的档位，是一套独立编排**：开关里没有"收敛完成后切成 lazy"这一档。若要让引擎先起、
由引擎自己报「明文库要迁移」来触发迁移，那是另一条路径——它要求引擎侧
`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on`（默认 off，见 `engine-store-encryption.md` C.12），连接器见
`code=StorePlaintextRequiresMigration` 后跑迁移器再重起。这条路径今天**没有落地**，写在这里只是把旧稿的
`lazy` 归位：它是"引擎先起 + 另开一个开关"的组合，不是 `KT_ENGINE_STORE_MIGRATE` 的第四个值。

### 2.4 退出码（连接器唯一可编程判据）

| 码 | 含义 | 连接器动作 |
|---|---|---|
| 0 | 成功 / 无事可做（`migrate`/`noop`/`absent`），或干跑（`action=dry-run`） | 继续起引擎；`apply` 的 `migrate`/`noop` 还记「本进程内该槽已迁移」，后续起槽跳过整库校验收口 |
| 2 | 用法错误（引擎不认 `storeMigrate` 或参数形状不对 = 连接器与引擎版本不齐） | 不锁存、本轮不重试；对外 `CAPABILITY_UNAVAILABLE(retryable=false)` + 版本不齐文案（属集成 bug） |
| 10 | **可重试**：空间不足、库被占用（含 swap 前 WAL 折不进去、**引擎停不下来**）、瞬时 I/O 失败、**权限/只读 IO 读失败**（D-15 口径，与 C 一致） | 本次**不起引擎**，下一次起槽重试（同一次起槽内至多 3 次、200ms/400ms 递增退避；超预算不重试）；对外 `CAPABILITY_UNAVAILABLE(retryable=true)` + 「not ready yet」文案 |
| 11 | **终态**：缺 SQLCipher 能力（`StoreCipherUnavailable`）、key 缺失/不匹配（`StoreKeyRequired`/`StoreKeyInvalid`/`store_key_mismatch`/`store_key_rejected`）、源库损坏、校验不过、主库状态违规（`store_state_invalid`：加密库完整性/页 HMAC 不过）、data-dir 不存在、账号目录消失 | **锁存** `StoreMigrationBlock{code, exit, message}`（进程内永不清，清理 = 换进程），该槽不再起引擎；对外一律 `CAPABILITY_UNAVAILABLE(retryable=false)` + 可执行文案（`StoreCipherUnavailable` 的文案**不**建议重链——那是构建/版本问题，数据没坏） |

> 干跑不看终态语义：`dry-run` 无论答 exit 0 / 2 / 10 / 11 都照常起引擎，结论只进日志标签
> （`planned` / `planned_failed`）。"观察模式绝不拦服务"是刻意的：拦服务的判据只能是实跑。
>
> 锁存的边界：`StoreMigrationBlock` 是**进程内**的锁存（换进程重来一遍），不是落盘状态；
> 磁盘上的事实仍只有文件头（§5.2）。清锁存的唯一动作是修好人要修的东西（补齐/恢复 key、
> 换正确构建、重链），重启连接器不会让它自己消失。

---

## 3. 正向迁移：五步（对照参考实现逐段）

### 3.0 前置条件（任一不满足 → 不写一个字节）

| # | 条件 | 判据 |
|---|---|---|
| P1 | SQLCipher 能力 | 能力门通过（`src/store_cipher.rs:233`）；否则 exit 11 |
| P2 | store key 可得 | `StoreKeyState::process()`（`src/store_key.rs:132-150`）为 `Present`；`Absent`/`Invalid` → exit 11，**零写入** |
| P3 | 引擎不在跑 | 由连接器保证（§2.3）；迁移器再取 `<DIR>/store-migrate.lock` 非阻塞 flock，取不到 exit 10 |
| P4 | 库可独占 | `PRAGMA wal_checkpoint(TRUNCATE)` 的 **busy 列为 0**（见 §3.1 的偏离 D-2） |
| P5 | 源库健康 | `PRAGMA integrity_check` = `ok`；不过则 exit 11，**不迁移**（避免把损坏库导出后再删备份） |
| P6 | 磁盘空间 | 空闲 ≥ `L × 2.1 + 32 MiB`（L = `store.db` + `-wal` 的当前大小）；不足 exit 10，零写入 |
| P7 | 同文件系统 | staging 与备份都是 `store.db` 的同目录兄弟文件（`rename` 原子性前提）；不满足直接拒绝（不提供跨盘模式） |
| P8 | 头分类为明文 | 非明文 → 走 §5.3 的幂等分支，不做迁移 |

### 3.1 Step 1：一致性快照 + 明文备份

~~~sql
-- 连接：无 key（明文库直接读）；create_if_missing=false；synchronous=FULL
PRAGMA wal_checkpoint(TRUNCATE);          -- 必须读回 (busy, log, checkpointed)，busy≠0 → 中止
PRAGMA integrity_check;                    -- 必须 'ok'
VACUUM INTO '<acct>/store.db.plaintext-backup';   -- 目标已存在会失败：先删（但只在本步之前的校验全过之后才删）
~~~

对应参考实现 `src/store.rs:11766`（checkpoint）、`:11769`（`VACUUM INTO ?1`）。

**偏离 D-1（连接选项）**：迁移器的连接必须 `create_if_missing(false)`。引擎现成选项是 `create_if_missing(true)`
（`src/service/mod.rs:14686-14692`），拿它做迁移会在路径打错时**悄悄建一个空库**，然后把它当成"源库"导出。
迁移器要一个独立的、只差这一个 flag 的选项构造；keyed 侧同理。

**偏离 D-2（checkpoint 的返回值）**：参考实现用 `execute_batch` 跑 checkpoint（`:11766`），**丢掉了 busy 列**。
`PRAGMA wal_checkpoint(TRUNCATE)` 在"另一连接还在读/写"时不会报错，只回 busy=1 —— 这正是"库被占用"的唯一
廉价信号。D 必须 `query` 出三列并断言 busy=0，否则 §9 的 D-04/D-05 无法在预检阶段拦住。

**偏离 D-3（fsync 纪律）**：`VACUUM INTO` 完成后显式 fsync 备份文件与父目录，再进入下一步。理由：备份是
唯一回滚物，rename 语义在 APFS/NTFS 上都要求内容先落地；参考实现依赖"SQLite 关库即 sync"，没有显式栅栏。

**备份是明文**：这是刻意保留的回滚窗口（参考实现同款 `.plaintext-backup`，保留 7 天，`:62,:66`）。
文案口径必须是"**新数据静态加密**"，不能写成"历史明文已安全擦除"（C 文档 §4.7 已钉）。

### 3.2 Step 2：导出加密库（ATTACH KEY + sqlcipher_export）

~~~sql
-- 仍用 Step 1 的同一个（无 key）连接
ATTACH DATABASE '<acct>/store.db.encrypting' AS encrypted KEY "x'<64hex>'";
SELECT sqlcipher_export('encrypted');
-- 头字段不在 sqlcipher_export 覆盖范围内，显式补齐：
PRAGMA encrypted.user_version = <源库 user_version>;
PRAGMA encrypted.application_id = <源库 application_id>;
DETACH DATABASE encrypted;
~~~

对应参考实现 `src/store.rs:11776-11779`（`ATTACH … KEY ?2`，路径与 key 规格都用绑定参数，绝不拼 SQL）、
`:11782`（`sqlcipher_export('encrypted'); DETACH DATABASE encrypted`）。

要点：

1. **raw key 形态** `x'<64hex>'`，与 connector/桌面同源（`src/store.rs:640`）；跳过 256000 轮 PBKDF2。
   经 sqlx 时要写成带双引号的 pragma 值，引擎 P0 已实测（`src/store_cipher_tests.rs:136-145`）。
2. `sqlcipher_export` 的行为边界（amalgamation `112014-112170`）：复制表定义（排除 `sqlite_sequence`）、
   索引、逐表 `INSERT … SELECT`、**再补 `sqlite_sequence`**（`:112113-112117`）、最后搬 view/trigger。
   所以 `threads.id`（`migrations/20250112201436_init.sql:111`，AUTOINCREMENT）的高水位是保留的——但仍要
   在 §4 里**比对**，不能假设。
3. `user_version`/`application_id` 是文件头字段，不在复制范围 → 必须显式补（上面两条 pragma）。
   presage 今天不依赖它们，但"迁移后与迁移前逐项相等"是本文的验收口径，不能留这种静默降级。
4. 导出期间 `synchronous=FULL`，`DETACH` 之后对 staging 显式 fsync，才允许进入替换步骤（否则断电可能在
   rename 后留下撕裂的加密库，把"可回滚"变成"回滚到备份 + 重做"）。
5. **不设 `cipher_plaintext_header_size`**（默认 0，amalgamation `:111124`）。一旦 >0，加密库会保留
   `SQLite format 3` 头，§5 的"头分类"就会把加密库判成明文，然后拿它当源库再导出一遍——D 要在启动时断言
   该 pragma 为 0（C 侧也必须有这条断言，列进 §7 的 G-D0）。

### 3.3 Step 3：校验（gate，详见 §4）

校验在 rename **之前**、对 staging 独立连接执行。任一项不过：删 staging + 其 sidecar，保留原库与备份，
exit 11，**不替换**。

### 3.4 Step 4：替换（sidecar → rename → marker）

~~~text
wal_checkpoint(TRUNCATE) + busy=0 # 删 sidecar 之前再收口一次源库（裁决 #10）
remove_sidecars(store.db)         # -wal / -shm / -journal（明文残留会让加密库下次打不开）
rename(store.db.encrypting → store.db)
fsync(父目录)
write store.format（tmp + rename；内容见 §5.2）
~~~

对应参考实现 `:11788`（`remove_sidecars(path)`）与 `:11789-11790`（`rename`）。顺序写死，**不能先删后写**：
任何时刻 `store.db` 要么是完整明文库、要么是完整加密库（§5.5 的可执行判据）。

### 3.5 Step 5：保留窗口与清理

| 项 | 规则 |
|---|---|
| `store.db.plaintext-backup` | 保留 **7 天**（对齐 connector 常量 `src/store.rs:66`）；每次迁移器运行按 mtime 清理过期项（参考 `:2578-2601`） |
| 清理前置 | **只在主库被判定为"加密且可打开"时才允许删备份**；主库状态不明 → 永不删备份 |
| 账号删除 | 顺序写死：删 key 文件 → 删 `store.db` + sidecar → 删备份/staging → 删目录（C 文档 F14 口径） |
| 观察窗 | 灰度期每档 24h，看 C 文档 M1–M6 |

**保留期同时是"密钥事故"的补救窗口**：7 天内若发现 key 丢失/错配，把 `.plaintext-backup` 换回 `store.db`
即可回到可用状态（§6.2）。这是保留期定 7 天而不是 1 天的实际理由，不只是"习惯"。

### 3.6 回滚优先级（别一上来就用反向迁移）

| 优先 | 手段 | 适用 | 数据风险 |
|---|---|---|---|
| ① | `rename(store.db.plaintext-backup → store.db)` + 删 `store.format` + 删加密库 | 备份还在（7 天内） | 无（备份是 checkpoint 后的完整快照，字节级还原） |
| ② | 反向迁移（§6） | 备份已过期/丢失 | 无（导出等价），但耗时与磁盘需求同量级 |
| ③ | 删库重链 | 以上都不可用 | **丢历史消息**，只在用户知情下做 |

### 3.7 参考实现 vs 本设计的差异清单（评审重点）

| # | 参考实现 | D 的做法 | 理由 |
|---|---|---|---|
| D-1 | 直接用现成打开选项（connector 只有一条路） | `create_if_missing(false)` 的独立选项 | 引擎现成选项会建空库（`src/service/mod.rs:14686-14692`） |
| D-2 | checkpoint 结果丢弃（`:11766`） | 读 busy 列并断言 0 | 被占用的唯一廉价信号 |
| D-3 | 靠关库隐式 sync | 显式 fsync 备份/staging/父目录 | rename 的原子性只在内容落地后成立 |
| D-4 | **无校验层**（导出成功即替换） | §4 全矩阵，不过不替换 | 参考实现能容忍"导出成功即正确"，引擎不能：一旦替换后才发现行数不对，只剩备份 |
| D-5 | `tracing::info!`（`:11795`） | WARN 级证据行 | 引擎滚动日志钉 `WARN`（`src/logfile.rs:57`），INFO 等于没记 |
| D-6 | 无反向迁移 | `--direction to-plaintext` | 二进制回退与 kill switch（§6） |

---

## 4. 校验矩阵（rename 之前的 gate）

全部针对 **staging 独立连接**执行（不用 `ATTACH` 上的 schema 前缀 pragma 去校验已挂载库，语义容易踩坑）。

| 层 | 检查 | 通过标准 | 反例含义 |
|---|---|---|---|
| 加密能力 | `PRAGMA cipher_version` | 非空 | 构建不是 SQLCipher → 整批拒绝 |
| 头 | staging 前 16 字节 | ≠ `SQLite format 3\0` | 等于 → 根本没加密（`PRAGMA key` 静默 no-op 或 ATTACH 没带 KEY） |
| 头（负例） | 无 key 连接打开 staging 并 `SELECT count(*) FROM sqlite_master` | **必须失败** | 成功 → 明文伪装成加密，直接中止 |
| 结构 | `PRAGMA integrity_check` | `ok` | 页结构损坏 |
| 结构 | `PRAGMA foreign_key_check` | 空结果 | 外键断裂（导出顺序/触发器副作用） |
| 加密完整性 | `PRAGMA cipher_integrity_check` | 空输出 | 页 HMAC 不符（写坏/撕裂） |
| schema 集合 | 源与 staging 各查 `SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'` | **集合相等** | 缺表（导出中断）或多表（脏 staging）。**不写死表名**：10 个迁移已增删过表 |
| 数据 | 对上面每个表 `COUNT(*)` | 逐表相等 | 行丢失/重复 |
| 数据（大表抽样） | `thread_messages` 按 `rowid` 取 min/max/sum(length(内容列))` | 相等 | 截断/编码漂移 |
| 迁移账本 | `SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version` | **逐行相等** | checksum 变 → 下次开库会重跑迁移或直接失败 |
| 序列 | `SELECT name, seq FROM sqlite_sequence ORDER BY name` | 相等 | AUTOINCREMENT 高水位回退（`threads.id` 复用风险） |
| 头字段 | `PRAGMA user_version`、`PRAGMA application_id` | 相等 | 见 §3.2 第 3 条 |
| 语义 | 用引擎真实代码路径打开 staging 并 `load_registration_data()` | `Some`，ACI/设备名与迁移前一致 | schema 对但语义坏（presage 反序列化失败） |
| key 归属（负例） | 用**另一个 data-dir 进程**的 key 打开 staging（同一 data-dir 内多账号共用一把 key，见 §7.2，不能这样测） | 必须 `StoreKeyRejected` | 打开成功 → 连接器把错误 profile 的 key 注入了这个槽 |
| 指纹 | marker 里待写的指纹 == 本次解析出的 key 指纹 | 相等 | key 在迁移过程中被换过（轮换竞态） |

> **为什么不比 `PRAGMA page_count`**：页布局因加密 reserve（每页 +80B）与 VACUUM 重排必然不同，比了就是假阳性。
> 参考实现连校验都没有，所以这一层没有任何可抄的先例——它是 D 的净新增（也是工作量的大头之一）。

---

## 5. 每账号独立、幂等、崩溃恢复

### 5.1 单元与并发

| 维度 | 规则 |
|---|---|
| 单元 | 一个 `accounts/lk-*/store.db`。迭代集合 = 文件系统上的 `accounts/lk-*`（**不是注册表**：注册表可能缺失/损坏，且未注册的链接库也会被服务） |
| 排除 | `accounts/_quarantine/`（证据区，改字节会破坏取证；恢复后由运维显式再迁移一次） |
| 顺序 | 按目录名排序，**串行**，同一时刻只持有一个库连接（页缓存与峰值磁盘可控） |
| 失败隔离 | 单账号失败不阻断其余账号：继续下一个，最后汇总；有任一终态失败则整体 exit 11，否则 0 |
| 跨 data-dir | 连接器限 **并发 1**（每次迁移 = 2× 写放大 + 2 份临时文件；10 个并发 VACUUM 会打爆磁盘） |
| 锁 | `<DIR>/store-migrate.lock`（0600）非阻塞 flock；data-dir 级而不是账号级——池化后二者等价，多账号 data-dir 本来就串行 |

### 5.2 状态判定：文件头是权威，`store.format` 只是审计

~~~text
state(account) :=
  Absent      store.db 不存在或长度 0                      → 不迁移（新建加密库是 C 的路径）
  Plaintext   前 16 字节 == "SQLite format 3\0"             → 迁移候选
  Encrypted   其余（SQLCipher 写随机盐在偏移 0）             → 已迁移，幂等跳过
~~~

`store.format`（原子写，tmp+rename，范式抄 `src/state.rs:156-185`）：

~~~text
magic=KTSF ver=1 state=sqlcipher-v1 keyid=<8hex> fp=<8hex> at=<unixms> tool=<engine 版本>
~~~

用途只有三条：① 审计（什么时候、哪个工具版本迁的）；② **key 指纹**（启动时比对，见 §7.2）；③ 让连接器
不打开文件就能跳过。**任何正确性判断都不许读它**。

> **对 C 文档 §4.6 的修正**：C 文档写"断点信息用 `store.format` 的原子性就够表达是否已 cutover"。
> 这句话有一个漏洞：`rename` 成功与写 marker 之间是一个真实的窗口，此时"库已加密、marker 说没有"就是
> 第三态。D 把权威改成文件头之后，这个窗口**不存在**了——marker 缺失最多丢审计，不会误判。这不是洁癖：
> 误判成明文会让迁移器把一个加密库当源库再导出一次，产出的是垃圾。

### 5.3 幂等（三种输入 → 三种结局）

| 输入 | 结局 | 证据行 |
|---|---|---|
| 明文（`--dry-run`） | 只出计划，exit 0，零字节改动 | `action=dry-run` / `code=store_migration_planned` |
| 明文 | 迁移 → 加密 | `code=store_migrated` |
| 加密、key 打得开且§5.5 谓词成立 | **no-op**，exit 0，零字节改动 | `code=store_migration_noop` |
| 加密但 key 打不开 | 终态 exit 11，**不**重建、**不**迁移 | `code=store_key_rejected` |
| 加密、打得开但完整性不过（`integrity_check`≠ok 或 `cipher_integrity_check` 非空） | 终态 exit 11，保留全部现场（不 prune 备份） | `code=store_state_invalid` |
| 明文头正常但**内容坏**（无 key 打不开） | 终态 exit 11，保留全部现场（零改名、零删除） | `code=store_state_invalid` |
| 不存在/长度 0 | no-op（新建是 C 的事） | `code=store_migration_absent` |

"零字节改动"要能被验收断言（§10.2 的 `snap` 前后 diff 为空），否则"幂等"只是口号。

### 5.4 崩溃断点与收敛（无第三态）

| 断点（kill -9 / 断电） | 现场 | 下次运行的收敛动作 |
|---|---|---|
| B0 清残留后、checkpoint 前 | 原库明文完整；staging/备份可能为空壳 | 判明文 → 重跑（先删残留） |
| B1 备份写完、导出前 | 原库明文完整；备份完整 | 判明文 → 先删备份再重做（备份只是上一次的快照，`VACUUM INTO` 会因目标存在而失败） |
| B2 导出中途（staging 半截） | 原库明文完整；staging 撕裂 | 判明文 → 删 staging + 其 sidecar → 重做 |
| B3 导出完成、删 sidecar 前 | 原库明文完整 + 空 `-wal`；staging 完整 | 判明文 → 删 staging → 重做（**不"收编"半成品**：没有提交标记就无法证明它完整） |
| B4 rename 后、marker 前 | **主库已加密可打开**；备份在；无 marker | 判加密 → 幂等跳过；补写 marker（仅审计） |
| B5 marker 写一半（tmp 残留） | 主库加密；`*.tmp` 残留 | 判加密 → 跳过；删 `.tmp` |

两条硬规则：

1. **只要主库判成明文，就先删 staging 再重做，绝不"接管"上次的 staging。** 参考实现同款（`:11758-11760`）。
2. **marker 的 `.tmp` 与 staging 一律视为可抛弃物**；备份只有在"主库加密且可打开"时才可删。

### 5.5 "无第三态"的可执行判据（验收与自愈共用）

~~~text
对任意时刻的 account：
  若 头=明文 → 必须能以无 key 连接打开 且 integrity_check=ok
  若 头≠明文 → 必须能以该账号 key 打开 且 integrity_check=ok 且 cipher_integrity_check 空
  若两者都不成立 → 违规（必须能从备份/上一态恢复）
~~~

这条谓词同时是**属性测试的 oracle**（§10.4 的随机 kill 循环）和**运行期自愈条件**：迁移器每次运行先判一次，
不满足就报 `store_state_invalid`（exit 11）并保留全部现场，绝不"猜一个方向修"。

---

## 6. 反向迁移（加密 → 明文）

### 6.1 为什么必须有（四条，按重要性排序）

1. **二进制回退**。发布矩阵是三平台（macOS×2 + Windows）。任一平台出问题就要回退引擎版本；P0 之后、C 之前的
   二进制不会 `PRAGMA key`，它打不开加密库。没有反向迁移，"回退"就等于"让用户重新扫码并丢历史"——这不是
   可接受的回退方案。
2. **kill switch**。加密上线后若出现性能/构建/口径问题，需要一条**演练过的**回到明文的路径。注意：加密库在
   明文模式下也打不开，所以"关开关"必须伴随反向迁移，二者是一个动作。
3. **口径/合规变化**。若结论变成"引擎库不做静态加密"，数据要能退回明文状态，而不是只能删库重建。
4. **正向迁移自身的最后退路**：备份过期后才发现语义问题（例如 presage 某表反序列化异常），唯一可走的就是
   用 key 把库转回明文再排查。

### 6.2 它明确不解决什么

- **不解决 key 丢失**。反向迁移需要**同一把 key**；env 里的 store key 丢了就没有反向（C 的落地形状没有可重算的包裹层，见 §7.1）。文案必须写"key 丢 = 历史
  永久不可读"，不能暗示"还有办法转回来"。
- **不是擦除**。转回明文只是把数据重新写成明文；之前加密期间的临时文件、APFS 快照不在承诺内。

### 6.3 步骤

~~~text
1) 前置：引擎停、flock、key 可得、空间预检（同 §3.0 P1–P7）
2) 打开加密库（keyed）→ PRAGMA wal_checkpoint(TRUNCATE)（断言 busy=0）→ PRAGMA integrity_check
3) 备份 = 字节拷贝 store.db → store.db.encrypted-backup（加密文件可安全按字节复制；
   不依赖 VACUUM INTO 的"输出加密参数继承"这种实现细节）→ fsync
4) 导出明文 staging（同一个 keyed 连接）：
      ATTACH DATABASE '<acct>/store.db.decrypting' AS plain KEY '';
      SELECT sqlcipher_export('plain');
      PRAGMA plain.user_version = <源>; PRAGMA plain.application_id = <源>;
      DETACH DATABASE plain;
   KEY '' 显式禁用加密（SQLCipher 官方反向 recipe）；不要靠"省略 KEY 子句"这种实现相关行为。
   若该形态在本构建上不成立，等价退路是反向指定源库：
      ATTACH DATABASE '<acct>/store.db' AS enc KEY "x'<64hex>'";
      SELECT sqlcipher_export('main','enc'); DETACH DATABASE enc;
   （main 是未加密连接 = 明文目标；sqlcipher_export 的第二参数就是源库）
5) 校验（§4 矩阵镜像）：头 == "SQLite format 3\0"；**无 key** 能打开且 integrity_check=ok；
   行数/账本/sqlite_sequence/user_version 与加密侧逐项相等
6) 替换：删加密 sidecar → rename(staging → store.db) → fsync 目录 → marker 置 state=plaintext
7) 保留：store.db.encrypted-backup 保留 7 天——**且这 7 天内 key 也不能删**，否则备份等于废纸
~~~

崩溃收敛与正向同构（§5.4）：头判明文 → 幂等完成；头判加密 → 重做。反向的"B4 等价断点"是
"rename 后、marker 置 plaintext 前"，同样靠头吸收。

**残留清理的位置**：反向的 step 0（staging / marker tmp / 上一次的 `.encrypted-backup`）与正向同款，
**必须排在 key / marker / integrity 全部通过之后、拷贝之前**。理由不是洁癖：终态 11 的验收面是「账号
目录逐文件不变」，`.encrypted-backup` 是反向唯一的回滚物——在校验之前删它，等于一次错 key 就毁掉回滚能力。
同理，验真/完整性闸开的是读写连接，WAL 模式下 SQLite 会物化空的 `-wal`/`-shm`；失败收口时只回收
**本次新建的空 sidecar**（非空 WAL 与本来存在的文件一律不动）。

### 6.4 往返一致性（验收必测）

`明文 → 加密 → 明文` 之后，§4 的矩阵必须**再次全绿**（行数、账本、序列、user_version 全等）。
这条是反向迁移唯一的"正确性"证明方式，不能只看"文件头变回去了"。

---

## 7. 与 C 阶段 env key 的关系

> C 阶段（key 通道 + 打开矩阵）在本机工作区**已并发落地但未 commit**：`src/store_key.rs`（207 行）、
> `src/store_open.rs`（309 行）、`src/service/mod.rs:14724-14860`。本节按**实际代码形状**写，不按评估稿的
> 设想写——两者不一致的地方在 §7.1–§7.2 写清。

### 7.1 C 已落地的形状：D 直接复用，不另造

| 事实 | 位置 | D 怎么用 |
|---|---|---|
| key 通道 = env `KT_SIGNAL_STORE_KEY`（64 hex 或 32B base64；与 connector 同名） | `src/store_key.rs:37`、`:70-81` | 迁移器读**同一个** env；不新造变量名 |
| 进程内 key 三态 `Absent`/`Present`/`Invalid`，`OnceLock` 缓存 | `src/store_key.rs:121-150` | 迁移器启动读一次；`Invalid` → exit 11、零写入 |
| raw key pragma 字面量 `"x'<64hex>'"`（含 sqlx 要求的双引号） | `src/store_key.rs:95-105` | ATTACH KEY 与打开都用它，**禁止自拼** |
| 文件分类 `Absent`/`Plaintext`/`Opaque`（只看 16 字节头 + 长度） | `src/store_open.rs:56-66`、`:207-268` | 迁移器的分类器**复用同一函数**，不写第二份（否则两处判定迟早分叉） |
| 错误码字面量 + 终态判定 | `src/store_open.rs:91-125`（`code()`/`is_terminal()`） | 迁移器的 exit code 映射到同一批 `code=` |
| 带 key 的连接选项 | `src/service/mod.rs:14750-14755` | 正向/反向/校验共用 |
| 打开矩阵 a–d + 加密库先验真 | `src/service/mod.rs:14788-14850`（`probe_keyed_store`） | D 只要求改 c 分支（§7.4 C-1） |

**关键事实：C 没有 KEK→DEK 包裹层，一个进程一把 key。** 评估稿 §3.3 设想的是"env KEK → 每账号 DEK → `store-key.v1`"，
落地版本是"env 直接一把 32B store key"。D 按落地版本设计（少一层文件、少一处时序），但要把由此产生的两条
边界写清楚。

### 7.2 "一把 key"带来的两条边界

1. **账号间密钥隔离 = 进程边界，不是密钥派生。** 池化后一个账号一个 data-dir、一个进程、一份 env → 天然隔离；
   遗留多账号 data-dir 里所有 `lk-*` **共用一把 key**，A 的库拿 B 的 key 是打得开的。所以 §4 那条
   "用别的账号的 key 打不开"的负例只能在**两个 data-dir/两个进程**之间测，不能在同一 data-dir 内测。
2. **key 指纹仍然必须有**：桌面重新生成 store key（换机/safeStorage 重置/profile key 轮换）后，同一份密钥
   材料不再成立，marker 里的指纹是唯一能把"换 key"和"库损坏"分开的证据；否则运维看到的是同一个
   `StoreKeyRejected`。

### 7.3 三条硬不变量

| # | 不变量 | 违反后果 | 落点 |
|---|---|---|---|
| K1 | **加密用的 key 必须就是之后打开用的 key**（同一个 env 值、同一次槽启动） | 库变成永久不可读的密文 | 迁移后立刻用同一 key 重开校验；marker 指纹；引擎启动比对指纹 → 终态 `StoreKeyMismatch`（不重建、不迁移） |
| K2 | 迁移器与引擎**不得各自读一次 env 后得出不同结论** | 同 K1 | 迁移器与引擎由同一次槽启动注入同一个值（§2.3）；迁移器是独立进程，所以 env 在两次 spawn 之间不能变 |
| K3 | 迁移器不得出现 key 的任何持久形态（argv、日志、metrics、marker 明文） | 加密意义被绕过 | 只允许 `keyid` + 指纹；证据行字段白名单（C 的 `StoreKey` 已打码 Debug；`Drop` 只覆盖自己那份副本，**不是**「进程内清零」保障——进程退出由 OS 回收，见 engine-store-encryption.md §3.5/C.5） |

### 7.4 D 要求 C 改一处、补一处

| # | 变更 | 现状 | 为什么必须 |
|---|---|---|---|
| C-1（硬依赖，**已落地为默认 off 的开关**） | 明文库（**与有没有 key 无关**）：开关 `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED` **on** → 终态 `StorePlaintextRequiresMigration`；**off（默认）= C 阶段原样**（`warn_plaintext_legacy_once()` 后按无 key legacy 打开） | `src/service/mod.rs:14905-14919`（开关 on 的两道闸）、`src/store_open.rs:94-158`（解析：on/1/true/yes，off 族与认不出的值都归 off + 一条滚动日志 WARN）；off 路径仍是 `:14926-14931` 的 `keyed=false` + legacy warn | 落地形态改了这条请求的"无条件终态"口径：C-1 落地时连接器还不注入 key，无条件终态等于把存量账号一次性打成"起不来"。做成**默认 off** 的开关后，"改终态"变成**按批打开的发布决策**；on 之后任何漏迁移仍是可见的启动失败，不是静默明文 |
| C-2（纯新增） | `StoreKey::fingerprint()`：域分离哈希（如 `SHA-256("kt-store-fp" ‖ key)` 前 8 字节） | 无 | marker 指纹（§5.2）；缺省只是少一条诊断，不阻塞 D |

C-1 是唯一会改变 C 已落地行为的请求（已按上表落地为默认 off 的开关，见 `engine-store-encryption.md` §7 C.12）；C-2 可后置。

### 7.5 `off` 模式的真实代价（按落地语义）

`KT_ENGINE_STORE_MIGRATE=off`（默认）的定义是"**连接器不跑迁移器**"，**不是**"引擎不服务明文"——
后者归引擎侧另一个开关：`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED` **默认 off**，明文库照旧按 legacy 打开
（有 key 时 warn 一次，不迁移、不改字节，矩阵 c）；只有把它显式开成 on，明文库才是
`StorePlaintextRequiresMigration` 终态（停**该账号**接收管线，不是进程退出）。

两个开关都不能让加密库回到可用明文：一旦库已加密，没有 key 就打不开（`StoreKeyRequired`），要回到
明文服务必须先跑 §6 的反向迁移（`--direction to-plaintext`）。所以事故 runbook 要写两条，别只写一条：
① `KT_ENGINE_STORE_MIGRATE=off` 只停"自动迁移"，加密库仍需 key；② 真要退回明文，走反向迁移，并让
`KT_SIGNAL_STORE_REQUIRE_ENCRYPTED` 保持 off。

---

## 8. 与 per-account engine 进程（池化）的关系

### 8.1 结构上就"每账号独立"

池设计的布局是 `engines/<accountId>/accounts/lk-<hex>/store.db`（池设计 §3.3），且 I3 要求"一个 data-dir
恰好一个账号"（§3.2）。于是：

- D 的迭代集合在池化布局下**恒为 1 个账号** → 爆炸半径 = 1 个账号，与池的 I1/I2 同构；
- 迁移的"停引擎"窗口 = 该槽的停槽窗口，不影响其他账号的收发；
- 迁移失败只会让**一个**账号进锁存块（`StoreMigrationBlock`），不会连坐。

**多账号遗留布局**（`proxy-groups/<groupId>`，迁移期仍在）里，一个 data-dir 有 N 个账号：D 串行逐个迁，
**允许部分成功**（账号 A 迁完、B 失败），因为每个库的合法性彼此独立；连接器只需按"整体退出码"决定是否放行
起引擎（建议：有任一终态失败就不起，避免半加密 half-serve）。

### 8.2 时序与预算

| 步 | 预算 | 依据 |
|---|---|---|
| 停槽 → 引擎退出 | 12s（建议把 `shutdown_grace` 从 5s 提到 ≥12s） | 池设计 §6.2：引擎管线收口 10s（`src/service/mod.rs:2058`）> 连接器 5s 预算 |
| 迁移 | 120s/账号（独立预算，可配） | C 文档 M7：>120s 中止 |
| 迁移 → 起引擎 | 立即 | 无额外等待 |

### 8.3 三个必须一起改的冲突点

1. **启动清扫依赖"能打开 store"**：`cleanup_abandoned_link_stores` 要 `open_store` 才能判"未注册"
   （`src/service/mod.rs:14796-14831`、`:14744`）。引擎开关 on 之后明文库打不开 → 清扫会把明文孤儿库**永久保留**
   （开关 off（默认）时明文库照旧能打开，这条只在「已经逐批打开终态开关」的 data-dir 上成立）。
   所以"先迁移、后清扫"是硬顺序：迁移器在 spawn 前跑（§2.3），引擎启动时的清扫面对的就都是加密库。
2. **重启风暴（只在引擎开关 on 时存在）**：开关 on 的明文库让**该账号**接收管线停环（进程不退、退出码仍只有
   0/1/2）；连接器若把这个终态当成"崩溃"重启整条引擎，就变成 1 秒级重启循环。所以
   `code=StorePlaintextRequiresMigration retryable=false` 必须被连接器识别为**不可重启**（跨仓依赖，写进 §11
   的依赖清单）。开关 off（默认）时明文库按 legacy 打开，没有这条。
3. **并发迁移**：池化启动是"并发 2 + 间隔 200ms"（池设计 §6.1 ④），而迁移是 I/O 放大 2× 的重活。
   迁移必须**串行**，且与引擎启动的并发池分开调度。

### 8.4 D 之后引擎启动：明文库按开关分流，**没有 exit 11**

~~~text
① 能力门（已有，src/main.rs:517）+ key 形状证据（:522）
② 每个 accounts/lk-*：读 16 字节头（open_store_with_requirement）
     明文 + 开关 off（默认） → **按 legacy 打开**（不设 key；**有 key 时**每进程一条 warn），
                               不迁移、不改一个字节（矩阵 a/c；**今天生产就是这条**）
     明文 + 开关 on          → **该账号**终态 StorePlaintextRequiresMigration：
                               接收管线停环（StreamOutcome::StoreTerminal），
                               进程不退、其余账号不受影响
     加密 → 交给 open_store（C 的 key 路径；错 key = StoreKeyRejected 终态）
     缺失 → 交给 open_store 新建（C 的路径；开关 on + 无 key = StoreKeyRequired）
③ 其余启动动作不变
~~~

**进程退出码只有 0/1/2**：0 = stdin EOF 干净退出，1 = 致命启动/运行态（注册表加载失败、
清扫失败、S5 单账号护栏、stdout writer 死亡），2 = 用法错误/非法代理 env。**11 是迁移器的码，
不是引擎的码**——引擎遇到打不开的库不会 `exit 11`，它要么按矩阵打开、要么把**那个账号**的接收
管线停掉（开关 on 时），进程继续服务其余账号。旧稿「明文 → 终态 exit 11」是设计期口径，
落地为「逐账号终态 + 可重试/不可重试码面」，见 §2.4 与 `engine-store-encryption.md` C.12。

---

## 9. 失败矩阵

| ID | 触发 | 检测点 | 行为 | 磁盘终态 | 退出码 |
|---|---|---|---|---|---|
| D-01 | 磁盘空间不足 | 预检（P6） | 不写任何文件 | 原样 | 10 |
| D-02 | 备份阶段磁盘满（`VACUUM INTO` SQLITE_FULL/ENOSPC） | `VACUUM INTO` 报错 | 删半个备份 | 原库明文完整、无 staging | 10 |
| D-03 | 导出阶段磁盘满 | `sqlcipher_export` 报错 | 删 staging + 其 sidecar | 原库明文完整 + 备份 | 10 |
| D-04 | 库被占用（引擎还在跑 / 第三方持有） | checkpoint busy≠0，或 rename 报 EBUSY/共享冲突 | 不动任何文件 | 原样 | 10 |
| D-05 | checkpoint 无法截断（有长读事务） | busy 列 | 同上（不"尽力而为"继续） | 原样 | 10 |
| D-06 | 迁移进程被 kill -9（B0–B5） | 下次运行的头分类 | 按 §5.4 收敛 | 两态之一 | 下次 0 |
| D-07 | rename 后 marker 前被杀 | 头=加密 | 幂等跳过 + 补 marker | 加密可用 | 0 |
| D-08 | store key 缺失（env 未注入） | `StoreKeyState::Absent` | 零写入 | 原样 | 11 |
| D-09 | store key 与 marker 指纹不符（换机/重新生成） | 指纹比对 | **既不迁移也不打开**；不重建 | 原样 | 11 |
| D-10 | 错 key 打开已加密库（跨账号/换机） | 首页读 `NotADatabase` | 归一 `store_key_rejected` | 原样 | 11 |
| D-11 | 源库损坏 | 预检 `integrity_check` ≠ ok | 不迁移（否则备份会被后续清理） | 原样 | 11 |
| D-12 | staging/备份残留 | 每次运行第一步 | 删 staging + sidecar；备份按"主库已加密"才删 | 收敛 | 0 |
| D-13 | sidecar 残留（`-wal/-shm`） | 替换前 | 与 rename 同一步删除 | 加密库可打开 | 0 |
| D-14 | 跨文件系统 rename（EXDEV） | P7 断言同目录 | 拒绝 | 原样 | 11 |
| D-15 | 权限/只读文件系统 | 打开或写失败 EACCES/EROFS | 零写入 | 原样 | **10**（裁决 #14：与 C 的 `StoreOpenFailed` 同口径；这类失败多为槽还没 chown/挂载未就位，退避重试能自愈，归终态会把槽误标成锁存终态） |
| D-16 | Windows 句柄未释放（sharing violation） | rename 失败 | 保留明文，等下次 | 原样 | 10 |
| D-17 | 两个迁移器并发 | flock 非阻塞失败 | 立即退出 | 由先到者收敛 | 10 |
| D-18 | 迁移中账号目录被删（并发 deleteAccount） | ENOENT | **不重建目录**，报 `account_vanished` | 无（已删） | 11 |
| D-19 | 校验不过（任一项） | §4 矩阵 | 删 staging，保留原库与备份 | 原样 | 11 |
| D-20 | data-dir 不存在（路径打错/槽没建/挂载丢） | `run()` 的第一条前置 | **不代建目录**、不建锁/日志 | 不变（目录仍不存在） | 11（`store_migrate_data_dir_missing`） |
| D-21 | 加密库打得开但完整性/页 HMAC 不过（§5.5 违规） | no-op 分支的 `integrity_check` + `cipher_integrity_check` | 保留全部现场，**不** prune 备份、**不**重建 | 原样 | 11（`store_state_invalid`） |

> 分类原则：**10 = 等一等还能成；11 = 等人**。连接器只需要认这两个数，不需要解析文案。

---

## 10. 验收命令与验收线（全部待执行）

> 占位：`[DIR]`=data-dir，`[LK]`=`lk-<hex>` 账号目录，`[KEK]`=`KT_SIGNAL_STORE_KEY` 的 64 hex 值（名字沿用占位，
> 实际就是 C 的 store key），`[KEY_HEX]`=同一个 64 hex 值（用来让 `sqlcipher` CLI 打开），`[BIN]`=`./target/debug/kt-signal-engine`。
> `sqlcipher` CLI 需 `brew install sqlcipher`（本机未核实）。

### 10.1 分类与前检

~~~bash
xxd -l 16 "[DIR]/accounts/[LK]/store.db"            # 期望：非 "SQLite format 3"
[ -e "[DIR]/store-migrate.lock" ] && echo "lock present"
ls -l "[DIR]/accounts/[LK]/"                        # 期望：无 .encrypting / *.tmp 残留
~~~

### 10.2 干跑 + 正向 + 幂等

~~~bash
KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" storeMigrate --dry-run --json
# 期望：exit 0；JSON 里 accounts[0].action=dry-run、code=store_migration_planned、backup=null、bytes、keyid，
#       plan=[checkpoint,backup,export,verify,swap]（顶层步骤名数组），did=[]
#      （干跑**不许**报 action=migrate / code=store_migrated / backup，否则连接器会以为替换已完成）

KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" storeMigrate --account [LK] --json
# 期望：exit 0；code=store_migrated

xxd -l 16 "[DIR]/accounts/[LK]/store.db" | grep -q 'SQLite format 3' && echo BAD || echo ENCRYPTED
sqlcipher "[DIR]/accounts/[LK]/store.db" \
  "PRAGMA key = \"x'[KEY_HEX]'\"; PRAGMA cipher_integrity_check; PRAGMA integrity_check; \
   SELECT count(*) FROM _sqlx_migrations; SELECT name,seq FROM sqlite_sequence ORDER BY name; \
   PRAGMA user_version;"
# 期望：cipher_integrity_check 空；integrity_check=ok；账本/序列/user_version 与迁移前记录逐项相等

snap() { (cd "$1" && find . -type f -exec shasum -a 256 {} + | sort); }
snap "[DIR]/accounts/[LK]" > /tmp/before
KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" storeMigrate --account [LK] --json
snap "[DIR]/accounts/[LK]" > /tmp/after
diff /tmp/before /tmp/after        # 期望：空（幂等零字节改动），exit 0 + code=store_migration_noop
~~~

### 10.3 三类失败必须"零写入"

~~~bash
# (a) 空间不足：装一个小盘，把账号目录放进去
hdiutil create -size 40m -fs APFS -volname ktmig /tmp/ktmig.dmg && hdiutil attach /tmp/ktmig.dmg
cp -a "[DIR]/accounts/[LK]" /Volumes/ktmig/ && KT_SIGNAL_STORE_KEY=[KEK] [BIN] \
  --data-dir /Volumes/ktmig storeMigrate --json      # 期望 exit 10，且 snap 前后 diff 为空

# (b) 被占用：真的起一个引擎占着库
KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" jsonRpc &   # 等它开好库
KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" storeMigrate --account [LK] --json
# 期望 exit 10（store_busy）；kill %1 后重跑 exit 0

# (c) 错 key / 缺 key
env -u KT_SIGNAL_STORE_KEY [BIN] --data-dir "[DIR]" storeMigrate --json          # 期望 exit 11、零写入
KT_SIGNAL_STORE_KEY=[另一把 key] [BIN] --data-dir "[DIR]" storeMigrate --json   # 期望 exit 11、零写入
~~~

### 10.4 崩溃注入（确定性断点 + 固定种子的崩溃点）

~~~bash
# 确定性断点（仅 debug 构建存在；release 里必须编译掉——评审项）
for p in after_backup after_export after_sidecar after_rename before_marker; do
  KT_ENGINE_STORE_MIGRATE_FAULT=$p KT_SIGNAL_STORE_KEY=[KEK] [BIN] \
     --data-dir "[DIR_COPY]" storeMigrate --account [LK] --json
  # 期望：进程在断点退出（模拟 kill）；然后
  KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR_COPY]" storeMigrate --account [LK] --json
  # 期望：exit 0，且 §5.5 谓词成立
done

# 崩溃注入（属性测试）：**固定种子的 50 个 kill 点**（0–200ms 伪随机延迟），
# 每个点 SIGKILL 后重启一次，断言 §5.5 谓词。实现里每轮还要断言子进程**真的**被
# SIGKILL 收走（signal=9）；延迟比迁移还长的轮次不算数，恢复起点后重杀。
for i in $(seq 50); do
  KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR_COPY]" storeMigrate --account [LK] --json &
  pid=$!; sleep 0.0$((RANDOM % 9))$((RANDOM % 9)); kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null
  KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR_COPY]" storeMigrate --account [LK] --json || exit 1
  # 断言：明文可无 key 打开且 integrity ok   XOR   密文可 keyed 打开且 integrity ok
done
~~~

### 10.5 反向迁移 + 旧二进制

~~~bash
KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" storeMigrate --account [LK] \
  --direction to-plaintext --json              # 期望 exit 0；header == "SQLite format 3\0"
sqlite3 "[DIR]/accounts/[LK]/store.db" "PRAGMA integrity_check; SELECT count(*) FROM _sqlx_migrations;"
# 用上一个 release（P0 之后、C 之前，无 key 能力）的二进制启动同一 data-dir，收一条消息
# 期望：能启动、能打开 store、能收；这条是"回退可用"的唯一硬证据（未实测，需发布产物）
~~~

### 10.6 每账号独立性

~~~bash
# 两个账号的 data-dir，人为破坏 A（头对、内容垃圾），B 正常
KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" storeMigrate --json
# 期望：B 迁成功；A 终态 11；整体 exit 11；A 的备份/原库都不动（snap 前后一致）
~~~

### 10.7 指标与证据行

~~~bash
printf '{"jsonrpc":"2.0","id":1,"method":"engine.perfMetrics"}\n' \
  | KT_SIGNAL_STORE_KEY=[KEK] [BIN] --data-dir "[DIR]" jsonRpc
# 期望快照含：timings.store_migrate_total / store_migrate_copy / store_migrate_export / store_migrate_verify
#             counters.store_migrate_ok / store_migrate_failed{reason=...}
grep -c 'event=store_migrate' "[DIR]/logs/engine-stderr.log"   # 每账号恰好若干条，不含 key/号码
~~~

### 10.8 验收线（DoD）

1. pilot 账号（建议 1 → 3 → 全量三档）迁移后：§4 矩阵**全绿**，且 `_sqlx_migrations` 逐行相等。
2. 幂等：第二次运行 exit 0 + `noop` + 目录 `snap` 前后 **diff 为空**。
3. 崩溃：5 个确定性断点 + **固定种子的 50 个 kill 点**，**每次**都能收敛，且 §5.5 谓词恒真（不存在第三态）。
4. 失败三类（空间/占用/错 key）**零写入**（snap diff 为空）且退出码分别是 10/10/11。
5. 反向：往返后 §4 矩阵再次全绿；旧版二进制能启动并收一条。
6. 引擎启动：已迁移库正常 open（`store_open` 计时出现）；未迁移库 **+ 引擎开关 off（默认）** 按 legacy 打开
   + 恰一条 warn、**零字节改动**；未迁移库 **+ 开关 on** 则该账号终态 `StorePlaintextRequiresMigration`
   （接收管线停环、进程不退）且连接器侧观察 5 分钟无重启循环。
7. 证据行：每账号一行 WARN，字段只有 `code/stage/account_tag/bytes/elapsed_ms/keyid`，无 key、无号码。
8. 磁盘：迁移期峰值 ≤ **3.0×L**；迁移后库体积 ≤ L×1.03（C 文档 M4）。
9. `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test --all-targets` 全绿；
   新增 `store_migrate` 单测与 `tests/contract.rs` 一条命令级用例。

---

## 11. 工作量

| # | 工作项 | 人日 | 说明 |
|---|---|---|---|
| 1 | 与 C 对齐接口（C1–C3）、退出码、错误码、argv 模式 | 0.5 | 纯契约，先落文档再写 |
| 2 | 头分类器 + 幂等分支 + flock + `--dry-run`/exit code | 1.0 | 含"零写入"断言 |
| 3 | 正向五步（checkpoint / 备份 / export / 头字段补齐 / sidecar / rename / marker） | 1.5 | 参考实现可对照，但要加 D-1~D-3 的偏离 |
| 4 | 校验矩阵（schema 集合 / 行数 / 账本 / 序列 / 负例 / 语义 probe） | 1.5 | 净新增，参考实现没有 |
| 5 | 反向迁移 + 往返测试 | 1.0 | `KEY ''` recipe + 矩阵镜像 |
| 6 | 崩溃注入（5 断点 + 随机 kill 属性测试） | 1.5 | 测试基建占一半 |
| 7 | 失败矩阵复现（空间/占用/错 key/损坏/只读/Windows 句柄） | 1.0 | Windows 需实机 |
| 8 | 指标词表 + 证据行 + 漂移单测 | 0.5 | `perf_metrics` 封闭词表要改测试 |
| 9 | **connector 侧编排**（spawn 迁移器、退出码策略、不重启闸、并发 1） | 1.0–1.5 | 跨仓，走 connector 流程 |
| 10 | 灰度演练（1→3→全量）+ runbook + 文档 | 1.0 | 含回退演练 |
| | **合计** | **9.5–11.5** | engine 7.5–9.0 / connector 1.0–1.5 / 演练 1.0 |

与 C 文档 P2 "5–8 人日"的差额解释：P2 的估法只覆盖"五步 + 一版校验"，没算反向迁移（第 5 项）、崩溃矩阵
（第 6 项）、失败矩阵实机（第 7 项，Windows 是大头）、跨仓编排（第 9 项）。本文按"能上线并敢回退"的口径重估。

关键路径：第 1 项（等 C 的接口）→ 3 → 4 → 6。第 9 项可并行，但**没有它不能进灰度**。

---

## 12. 明确不做

1. 不做 `PRAGMA rekey` 原地轮换（崩溃安全性未验证；轮换走"导出到新 staging"或 C 的重包路径）。
2. 不做"接管上次 staging"的断点续传（没有提交标记就无法证明完整；重做一次的成本可接受）。
3. 不做跨文件系统迁移（`rename` 原子性前提）。
4. 不做媒体附件、导出文件、connector 库的加密（C 文档 §0 的口径不变）。
5. 不做"2.0× 磁盘"的优化（把原库 rename 成备份再导出）。它省一份全量拷贝，但会让 `store.db` 在一段时间内
   **不存在**，放大崩溃窗口、破坏"任何时刻主库都完整"的判据。留作未来优化，前提是有 clonefile/ReFS 之类的
   零拷贝原语可用。
6. 不做加密库的 key 轮换编排（属 C/ADR 0006 的 catalog 范畴）。
7. 不把迁移器做成第二个可执行文件（理由 §2.1 C）。
8. 不在 `store.format` 里写任何可用于正确性判断的状态（理由 §5.2）。

---

## 13. 未核实项（别当结论用）

1. **未跑构建/未跑迁移**：本文全部命令是设计，没有一条在本机执行过。
2. **Windows 的 rename 语义**：`MoveFileEx` 在目标被其他句柄持有时的共享冲突行为、以及 `fsync` 目录在 NTFS 上
   的可行写法，都需实机验证（D-16 的退出码归 10 是推断）。
3. **磁盘峰值 3.0×L**：是"原库 + 备份 + staging"的算术，不是实测；`VACUUM INTO` 对 freelist 的收缩会让备份
   小于 L，实际可能 2.6–3.0×。
4. **`sqlcipher_export` 对 `WITHOUT ROWID` 表/生成列的行为**：本仓 schema 目前没有这类表，但上游迁移可能引入；
   §4 的 schema 集合比对能发现"表没了"，不能发现"约束变了"——需要在上游迁移引入新特性时补检查。
5. **sqlx 对 `ATTACH … KEY ?2` 的绑定**：connector 用 rusqlite 验证过；sqlx 侧只推理过（`VACUUM INTO ?1` 同理），
   落地要先写一条探针测试。**D2 已实测**：正向 `ATTACH … AS encrypted KEY ?` 与反向 `ATTACH … AS plaintext KEY ''`
   在 sqlx 0.9 上均按预期工作（`src/store_migrate.rs:1325`、`tests/store_migrate.rs` 的往返用例）。
6. **连接器 15s/30s 预算的当前值**来自池设计文档转引 `kt-signal-connector/src/engine.rs:158-159,2777-2812`，
   本文没有在 connector HEAD 上复核这两处。
7. **C 的落地形状可能还会动**：§7 写的是本机工作区当前代码（未 commit）。若 C 改成"KEK→每账号 DEK + 包裹文件"，D 只需把 P2 换成「解包 DEK」，其余步骤不变（key 来源是唯一耦合点）。
8. **`ATTACH … KEY ''` 的明文语义**：这是 SQLCipher 文档的反向 recipe，本文没有在本机构建上实测；§4 的头检查与「无 key 能打开」负例会把它变成硬失败（而不是静默产出加密库），所以风险是"反向迁移当场失败"而不是"错数据"。等价退路见 §6.3。
   **D2 已实测**：本机构建上 `KEY ''` 产出的是真明文（头 = `SQLite format 3\0`、无 key 能打开、
   带 key 打开报 `not a database`），因此等价退路（§6.3 的 `sqlcipher_export('main','enc')`）没有启用。

---

## 14. 证据索引

**kt-signal-engine（本仓，工作区 HEAD `c4a96f4`）**

- `Cargo.toml:9-23`（`sqlcipher` feature 门）、`:158-180`（target 拆分依赖）
- `src/main.rs:123-165`（USAGE / LaunchArgs / 严格 argv 解析）、`:438-465`（单账号护栏）、`:483`（`logfile::init`）、`:489`（能力证据）、`:493`（perf 钩子）、`:497`（清扫）、`:501`（`EngineState::load`）、`:508`（护栏）
- `src/service/mod.rs:9122`（`RECEIVE_CONNECT_TIMEOUT=90s`）、`:8478`（`StreamOutcome`）、`:9194` 一带（Recoverable）、`:14682`（cache pragma）、`:14686-14692`（`store_connect_options`）、`:14700`（`open_store`）、`:14718`（`open_store_gated`）、`:14730-14731`（`store.db` 路径）、`:14744`（`remove_unregistered_link_store`）、`:14767`（`remove_link_store`）、`:14796-14831`（`cleanup_abandoned_link_stores`）、`:2058`（管线收口 10s）
- `src/state.rs:63,64,67`（目录/文件/隔离区常量）、`:156-185`（`persist` 原子写）、`:235`（`account_dir`）
- `src/store_key.rs:37`（`KT_SIGNAL_STORE_KEY`）、`:70-81`（`parse`）、`:95-105`（`pragma_literal`）、`:121-150`（三态 + `process`）
- `src/store_open.rs:56-66`（`StoreFileState`）、`:91-125`（`StoreOpenError` 码/终态）、`:186-200`（`warn_plaintext_legacy_once`）、`:207-268`（`classify`）
- `src/service/mod.rs:14750-14755`（`store_connect_options_with_key`）、`:14788-14850`（`open_store_with` 矩阵 a–d）、`:14813-14819`（矩阵 c 现状）
- `src/store_cipher.rs:46`（pragma 常量）、`:209`（`gate`）、`:233`（`ensure_cipher_capable`）、`:256`（`report_startup_capability`）
- `src/store_cipher_tests.rs:58`（能力断言）、`:136-200`（keyed store / 无 key 必败 / `pragma` 引号口径）
- `src/service/perf_metrics.rs:30-70`（封闭词表）、`:227`（`record_us`）、`:258`（`snapshot_json`）
- `src/logfile.rs:57`（`MAX_LEVEL=WARN`）
- `tests/contract.rs:28-49`（真进程夹具）、`:258`（`CARGO_BIN_EXE_kt-signal-engine`）
- `vendor/presage/presage-store-sqlite/src/lib.rs:42-47`（`open`）、`:49-65`（`open_with_passphrase`）、`:67-92`（WAL/Full/池 2/migrate）
- `vendor/presage/presage-store-sqlite/migrations/20250112201436_init.sql:1,4,12,20,27,34,42,52,74,110,116`（表清单）、`:111`（`threads.id` AUTOINCREMENT）
- `docs/plan/engine-store-encryption.md`：§3.3（KEK→DEK，落地改成了 env 单把 key，见 §7.1）、§3.4（fail-closed 决策表，需按 §7.4 C-1 改一行）、§4.2（五步）、§4.3（校验）、§4.5（回滚）、§4.6（断点，本文修正）、§5.2 M7–M9（指标）、§7 P2（工作量）
- `docs/plan/engine-pool-per-account-design.md`：§3.2（I1–I5）、§3.3（`engines/<accountId>` 布局）、§6.1–6.5（启动/停槽/崩溃/锁）、§6.6（超时总表）

**kt-signal-connector（@ `d491396`）**

- `src/store.rs:62,64,66`（后缀与保留期）、`:95`（明文头）、`:564,568`（错误码）、`:640`（`raw_key_spec`）、`:2450-2465`（打开决策表）、`:2474-2479`（`secure_delete`/WAL 上限）、`:2578-2601`（备份过期清理）、`:11698-11717`（`db_file_state`）、`:11720-11745`（`open_encrypted`）、`:11752-11806`（`migrate_plaintext_store` 五步）、`:11823,11834`（备份路径/sidecar 清理）、`:22809,22895,22911,22931`（四条测试）

**SQLCipher amalgamation（本机 registry `libsqlite3-sys-0.36.0`）**

- `sqlcipher/sqlite3.c:112014-112170`（`sqlcipher_export`：schema `:112072` / 数据 `:112103` / 序列 `:112113-112117`）
- `:111124-111134`（`cipher_plaintext_header_size`）、`:111453-111457`（`cipher_integrity_check`）

---

## 15. 落地记录：D2 反向迁移（2026-11-11）

> 本节是**实现记录**，不是新设计：上文的契约（五步、`--json` 形状、退出码 0/2/10/11）一条都没改，
> 只把 §6 的反向迁移落到 `src/store_migrate.rs` 并补上它需要的两个落盘名字。
> 行号以本机当前工作区为准（未 commit）。

### 15.1 契约增量（连接器只需要认这几条）

| # | 项 | 值 | 落点 |
|---|---|---|---|
| 1 | 方向开关 | `--direction to-plaintext`（合法取值，默认 `to-encrypted`） | `src/main.rs:128-129`（USAGE）、`src/store_migrate.rs:220-232`（`Direction::parse`） |
| 2 | 成功码 | `store_plaintext_restored`（`action=migrate`：确实发生了一次替换） | `src/store_migrate.rs:156`（码面）、`:779`（回填点） |
| 3 | 回滚物 | `store.db.encrypted-backup`（**字节拷贝**，保留 7 天；这 7 天内 key 也不能删） | `src/store_migrate.rs:2206`、`:2331` |
| 4 | staging | `store.db.decrypting`（与正向的 `.encrypting` **刻意分开**：同一路径上的半成品无法判断方向） | `src/store_migrate.rs:1059` 起 |
| 5 | `plan[]`/`did[]` | 仍是 `checkpoint/backup/export/verify/swap`（反向不改步骤名，只改导出目标） | `src/store_migrate.rs:199` |
| 6 | `store.format` | `state=plaintext`（只作审计；正确性仍只读文件头） | `src/store_migrate.rs:2292`、`:2315` |
| 7 | 退出码 | 不变：错 key → **11** `store_key_rejected`；marker 指纹不符 → **11** `store_key_mismatch`；占用/空间/瞬时 IO → **10** | `src/store_migrate.rs:1059-1100` |
| 8 | 干跑 | `action=dry-run` / `code=store_migration_planned` / `backup=null`（与正向同一形状） | `src/store_migrate.rs:792` |

### 15.2 五步落点与「不碰原库」的证明方式

| 步 | 实现 | 对原加密库的动作 |
|---|---|---|
| checkpoint | `probe_keyed_store` 验真 → 带 key `wal_checkpoint(TRUNCATE)` 断言 busy=0 → `integrity_check` + `cipher_integrity_check` | 只读（WAL 折入是 SQLite 的既有语义，不改内容） |
| backup | 收口 WAL（断言 `-wal` 为空）→ `fs::copy` → fsync → `probe_keyed_store` 验真 | 只读 |
| export | `ATTACH … AS plaintext KEY ''` → `sqlcipher_export('plaintext')` → 补 `user_version`/`application_id` → `DETACH` | 只读 |
| verify | 头 == `SQLite format 3\0` + 无 key 能打开 + `integrity_check`/`foreign_key_check` + 逐表行数/账本/序列/头字段 + `load_registration_data` 与源侧一致 + **带 key 打不开** | 只读 |
| swap | keyed checkpoint 收口 → `remove_sidecars(store.db)` → `rename` → fsync 目录 → 写 marker | 唯一的写动作是 `rename`（原子） |

失败路径的可执行判据（与 §5.5 同一条）：导出/校验失败只删 `store.db.decrypting`（+sidecar）；
**终态 11**（错 key / 指纹不符 / 完整性不过 / 明文头但内容坏）一个文件都不动——原库、备份、staging、marker tmp
全部保留取证，`store.db` 逐字节不变；因此「第三态」不可能出现——主库要么是完整密文、要么是完整明文。

### 15.3 测试覆盖（本机已执行）

**单测（`src/store_migrate.rs::tests`）**：

- `reverse_dry_run_plans_without_touching_the_store`（目录快照逐字节不变）
- `reverse_noop_on_plaintext_store_changes_nothing`（幂等 no-op 零字节改动）
- `marker_state_tracks_the_direction`（`sqlcipher-v1` ↔ `plaintext`，marker 不含 key）
- `backup_retention_is_bucketed_by_suffix`（正向清理不碰反向备份，反之亦然）

**真进程（`tests/store_migrate.rs`）**：

- `cli_unmigrates_encrypted_store_to_plaintext`（`--json` 契约 + 五步 plan/did + 回滚物可用）
- `cli_round_trip_preserves_rows_ledger_and_registration`（明文→加密→明文，事实逐项相等，含注册行）
- `cli_reverse_dry_run_plans_without_writing`
- `cli_reverse_on_plaintext_store_is_a_noop`
- `cli_reverse_verify_failure_leaves_encrypted_source_untouched`（注入行数不符：原库逐字节不变 + 下次可收敛）
- `cli_reverse_fault_points_converge`（`after_backup/after_export/after_sidecar/after_rename/before_marker`）
- `cli_reverse_wrong_key_leaves_the_store`（11 + 字节不变）
- `cli_reverse_terminal_failures_leave_the_account_dir_byte_identical`（错 key / 指纹不符：**目录**快照逐文件不变）
- `cli_corrupt_plaintext_is_state_invalid_and_touches_nothing`（明文头+内容坏 → `store_state_invalid`，两个方向都零改动）
- `cli_reverse_random_kill_converges_to_one_state`（**固定种子的 50 个 kill 点**：0–200ms 伪随机延迟，每点断言 signal=9、§5.5 谓词与无孤儿 staging）

### 15.4 未决项（不属本次交付）

1. **旧二进制实测**（§10.5 第 3 条）：需要「P0 之后、C 之前」的发布产物，本次只证到「无 key 可打开且
   `integrity_check=ok`」，没有真的用旧版引擎收一条消息。
2. **connector 编排**：`KT_ENGINE_STORE_MIGRATE` 三档、`StoreMigrationBlock` 锁存与 exit 10/11 的对外
   错误面（`CAPABILITY_UNAVAILABLE` 的 retryable 分档）已按 §2.3/§2.4 落在 connector 侧
   （分支 `fix/signal-runtime-state-authority`：`d87ff46` 按账号注入 store key、`963cee6` 迁移编排 + 槽锁存）。
   本仓只需守住退出码与 `--json` 形状。
3. **Windows**：`rename` 共享冲突与目录 fsync 仍未实机验证（§13.2 原样）。
4. **磁盘峰值**：反向同样是「原库 + 加密备份 + 明文 staging」≈3.0×L，未实测（§13.3 原样）。

