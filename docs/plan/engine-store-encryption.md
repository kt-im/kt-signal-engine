# M6：engine 的 `accounts/lk-*/store.db` 静态加密评估（只读）

> **状态**：评估稿 v1（**只做评估**：未改任何代码、未跑构建、未 commit）
> **日期**：2026-11-11
> **本文所在仓库**：kt-signal-engine（分支 `fix/admin-write` @ `ea9d75f`，工作区含未提交改动
> `src/methods.rs`/`src/service/mod.rs`/`src/state.rs` —— 本文 `file:line` 以**本机当前工作区**为准）
> **交叉引用仓库**：kt-signal-connector（`fix/delete-sync-conversation` @ `d491396`）、
> kt-desktop（`.worktrees/signal-security/electron/main/signal/storeKey.ts`）
> **前置阅读**：`adr/0001`、`adr/0004`、`docs/jsonrpc-compat.md` §1/§6、
> 仓根 `SIGNAL-ADR-0006-PER-ACCOUNT-STORE-KEY.md`（connector 侧 per-account key 草案）
> **边界**：本文不改代码、不改 Cargo.toml、不改 connector/desktop。所有"验收命令"都是**待执行**，
> 不是已执行结论；标注「未实测」的地方是推断，不是实测数据。

---

## 0. 一页结论

| # | 问题 | 结论 |
|---|---|---|
| 1 | 换 `presage-store-sqlcipher` 可行吗 | **可行，且比预期便宜**：vendor 里的 `presage-store-sqlite` 已经有 `open_with_passphrase` + `PRAGMA key` 注入（`vendor/presage/presage-store-sqlite/src/lib.rs:49-65`），引擎只是用 **`default-features=false` 把它关掉了**（`Cargo.toml:25`），真正要动的是引擎自己的 `libsqlite3-sys` feature（`Cargo.toml:33-35`）。唯一硬骨头是 **Windows + crt-static** 必须走 vendored OpenSSL。 |
| 2 | 密钥从哪来 | connector **已有** 32B store key（bootstrap 第二行，`src/auth.rs:19-21`、`src/store.rs:98,647-656`），但**引擎这条通道是空的**：connector 起引擎时显式把 key 环境变量剔掉（`src/engine.rs:2695-2697`）。推荐 **KEK（env 传入）→ 每账号随机 DEK → 包好后落 `store-key.v1`（0600）**，引擎侧只做解包，零新增 crate（`aes-gcm`/`hkdf`/`sha2`/`rand` 已在图：`Cargo.toml:53,100-103`）。 |
| 3 | 明文库怎么迁 | 逐账号、离线、一次一账号：`wal_checkpoint(TRUNCATE)` → `VACUUM INTO` 备份 → `ATTACH … KEY` + `sqlcipher_export` 到 staging → 三重校验 → 清 sidecar + `rename` 原子替换 → 留备份可回滚。**connector 已经实现过同一套**（`src/store.rs:11752-11820`），引擎照抄即可，不要发明。 |
| 4 | 性能/内存影响 | 确定性成本三条：页格式开销 **≈+2.0% 体积**（IV 16 + HMAC-SHA512 64 / 4096）、`SQLITE_TEMP_STORE=2`（临时表强制进内存，重排序会抬 RSS）、连接池每 10–30 分钟回收 → **密钥必须常驻堆**（`sqlx-core/src/pool/inner.rs:26`）。指标表与阈值见 §5。 |
| 5 | 失败模式 | **密钥丢失 = 本机历史不可读**（本文 §6.1；G-A 已拍板，「重链再导」未取证，见 §6.1.1）。次严重的是「半实施」：`PRAGMA key` 在纯 SQLite 上是**静默 no-op**，feature 没翻也不会报错 —— 测试全绿而库仍是明文。 |
| 6 | 分阶段 | P0 构建门禁（1–2 人日）→ P1 密钥通道 + fail-closed（3–5）→ P2 迁移器（5–8）→ P3 灰度与文案（2–3）→ P4 清理（1–2）。**合计 12–20 人日**，不含合规等待。**另有一套跨仓批次命名 A/B/C/D**（同一 M6 工作流）：A = 能力门（原 P0，已完成）、B = 失败语义与观测（原 P1 指标面）、**C = engine 半边 key 通道 + 五态矩阵 + 错误码（原 P1 引擎侧，已完成，见 §7「C 阶段」）**、D = 迁移器 + connector 接线（原 P2）。 |
| 7 | 现在做/不做 | **做**（判定：做），但切成「P0+P1 立刻做；P2 迁移等下一个 release 窗口」。**三个前置门**任一不过就停在 P0（能力就绪、不迁移、不对外宣称加密）：G-A 合规口径、G-B Windows 构建、G-C fail-closed 可区分。 |

**明确不做**（写在这里防止后续被塞进来）：

1. **不做半实施发布**：feature 翻了一半、或没有 `cipher_version` 断言就上线，等于"文档说加密、磁盘是明文"。
2. **不做原地 `PRAGMA rekey`** 作为默认轮换/迁移路径（崩溃安全性未验证；走 export 到 staging）。
3. **不做混合格式长期共存**：每账号要么明文要么加密，靠 `store.format` 标记显式判定，不靠"打得开就明文"。
4. **不把引擎这次改动包装成"账号本地数据全加密"**：媒体附件、明文导出、connector 侧 key 域各自独立（与 ADR 0006 §1.4 同一口径）。
5. **不为加密引入新 crate**：现有图已够（见 §3.3）。

---

## 1. 事实基线（已核实，全部带 file:line）

| 事实 | 证据 |
|---|---|
| 引擎 store 是**纯 bundled SQLite**，无 SQLCipher、无 openssl | `Cargo.toml:33-35`（`libsqlite3-sys = { version = "0.36", features = ["bundled"] }`，注释原文"无系统依赖、无 sqlcipher/openssl"） |
| vendored store crate 的**默认 feature 就是 SQLCipher**，但被引擎关掉 | `vendor/presage/presage-store-sqlite/Cargo.toml:8`（`default = ["libsqlite3-sys/bundled-sqlcipher"]`）vs `Cargo.toml:25`（`default-features = false`） |
| 真正的打开点只有**一个函数** `open_store` | `src/service/mod.rs:14695-14707`；调用面：`src/service/mod.rs` 内 57 处（含大量内联测试），另有散点 `profile_refresh.rs`(3)、`groups.rs`(2)、`own_capabilities.rs`(1)、`send_warmup.rs`(2)、`snapshot_import.rs`(3)（同样含测试） |
| 连接选项只钉 `create_if_missing` + `foreign_keys` + `cache_size=-1024`，**没有任何 key** | `src/service/mod.rs:14682-14688` |
| store 包里是什么：**身份私钥、会话/棘轮状态、prekey、群 sender key、联系人、消息** | schema 表清单 `vendor/presage/presage-store-sqlite/migrations/20250112201436_init.sql`（`identities`/`sessions`/`pre_keys`/`signed_pre_keys`/`kyber_pre_keys`/`sender_keys`/`contacts`/`profiles`/`thread_messages`…，均为 `record BLOB`） |
| 静态保护的**唯一**手段是目录 0700，且是 **connector 给 data-dir 根目录**打的 | connector `src/engine.rs:7929-7955`（`prepare_data_dir` → `set_permissions(0o700)`）；引擎自己只 `create_dir_all`（`src/service/mod.rs:14687`、`src/state.rs:98`），`accounts/lk-*` 子目录靠父目录 0700 兜 |
| 引擎**没有** store 加密 / key / 格式标记的任何实现 | 源码零处 `PRAGMA key`/`cipher_version`/`cipher_integrity`（`grep -rn` 无命中）；无 store key 文件、无格式标记；`sqlcipher` 在 `docs/`+`adr/`+`src/`+`README.md` 的唯一命中是 `docs/jsonrpc-compat.md:5665` 一句"不触碰 sqlcipher" |
| connector 有 32B store key，走 bootstrap 第二行，只在 `Zeroizing` 内存 | connector `src/auth.rs:19-21`（`PAYLOAD_BYTES = 64*2+1`）、`src/store.rs:608-641`（`StoreKey`/`raw_key_spec` → `x'<64hex>'`）、`:647-656`（`KT_SIGNAL_STORE_KEY` 仅 dev 覆盖） |
| connector **故意不把 key 传给引擎** | connector `src/engine.rs:2695-2697`：`.env_remove(crate::store::STORE_KEY_ENV)`，注释"keys travel the bootstrap channel only" |
| 桌面侧有成熟先例：key 生成、safeStorage 密封、KEY_MISSING 反静默重建 | `storeKey.ts:6-33`（保管链头注）、`:65-69`（"store 存在但 key 丢 → 直接抛，绝不新建"）、`:352`（fail closed 分支） |
| 引擎启动**先开库、后收 RPC**，所以"运行时再给 key"来不及 | `src/main.rs:472`（`cleanup_abandoned_link_stores`）、`:476`（`EngineState::load`）都在主循环前 |
| store 打不开时接收管线**只当网络故障重试** | `src/service/mod.rs:9179-9191`（`Ok(Err(())) => StreamOutcome::Recoverable` + `warn!`）、终态枚举 `:8472-8483` |
| 引擎已有观测面可挂指标 | `src/service/perf_metrics.rs:33-60`（封闭词表）、`:258-287`（`snapshot_json`）、`src/service/mod.rs:3598-3601`、`src/methods.rs:4820` |
| 引擎**没有** `secure_delete`、也没有 `store.db` 的拷贝/备份逻辑（备份不搬 store） | `src/` 内 `store.db` 只出现在 open/存在性判定/测试；`src/backup/`、`src/service/cloud_backup.rs` 零命中 |

> **口径修正（顺手记一笔）**：`src/service/mod.rs:14668-14674` 的注释说"sqlx 池默认最多 10 连接、
> 单账号缓存面可达 ~20 MiB"。实际 vendored store 把池钉死在 **2** 连接（`vendor/…/src/lib.rs:80-84`），
> 所以 1 MiB/连接 × 2 = 2 MiB。这是注释与代码的漂移，不影响结论，但排性能预算时按 **2** 算。

---

## 2. 问题 1：换 `presage-store-sqlcipher` 的可行性

### 2.1 Cargo 依赖：改哪一行、代价落在哪个平台

**不需要**新增 `presage-store-sqlcipher` 这个 crate（上游本来就是 `presage-store-sqlite` 带 feature，
本仓 vendor 已经 copy 过来）。当前链路是：

```text
presage-store-sqlite (vendor) --default-features = false--> libsqlite3-sys: []      (Cargo.toml:25)
引擎自己                ---------> libsqlite3-sys: ["bundled"]                      (Cargo.toml:35)
=> 全图归一：plain sqlite（无 SQLITE_HAS_CODEC）
```

要打开 SQLCipher，把**引擎自己那行**换成 SQLCipher 的捆绑 feature 就够（`presage-store-sqlite` 保持
`default-features = false`，避免它的默认值再回来）。因为 feature 是全图归一的，按目标拆开写：

```toml
# Cargo.toml 替换 33-35 行
[target.'cfg(windows)'.dependencies]
# Windows/MSVC 没有可用的 CommonCrypto，libsqlite3-sys 在 windows 上
# 见不到 OPENSSL_DIR 会直接 panic（build.rs:171-173），必须 vendored OpenSSL。
libsqlite3-sys = { version = "0.36", features = ["bundled-sqlcipher-vendored-openssl"] }

[target.'cfg(not(windows))'.dependencies]
# macOS 走 CommonCrypto（-DSQLCIPHER_CRYPTO_CC + Security/CoreFoundation，build.rs:220-223）
libsqlite3-sys = { version = "0.36", features = ["bundled", "bundled-sqlcipher"] }
```

> 落地形状与本节一致（非 Windows 段显式写出 `bundled`，Windows 段照抄；`sqlcipher`
> 留作纯门）。实际 diff 与目标解析证据见 §7 P0「Windows 目标修复」。

平台代价（`~/.cargo/registry/src/…/libsqlite3-sys-0.36.0/build.rs`）：

| 目标 | 加密实现 | 新增系统/工具依赖 | 构建增量 |
|---|---|---|---|
| `aarch64-apple-darwin` / `x86_64-apple-darwin` | CommonCrypto（`-DSQLCIPHER_CRYPTO_CC`） | 无 | 编 SQLCipher amalgamation（≈ +1 个 C 文件，几分钟） |
| `x86_64-pc-windows-msvc`（`crt-static`，`release.yml:65-66`） | vendored OpenSSL（`openssl-src`） | **新增 `openssl-sys`/`openssl-src` 入图**，Windows runner 需 Perl/CMake | **最大风险点**：crt-static + 静态 OpenSSL 组合，需实测（见 G-B） |
| Linux（当前 release matrix **没有**） | 系统 `libcrypto` 或其 vendored | 系统 OpenSSL 头/库 | — |

补充事实（会影响"要不要顺便做 Linux"的判断）：release matrix 只有 macOS×2 + Windows×1（`.github/workflows/release.yml:33-41`），
Linux 现在没有发布面。

**测试面已经分叉**：vendor crate 是独立 workspace，自带 `Cargo.lock`，默认可开 SQLCipher
（`vendor/…/Cargo.toml:8`、`:40-51`）。也就是说 **vendor 自己的测试跑在 SQLCipher 上、引擎的测试跑在纯 SQLite 上**，
今天没有任何引擎测试覆盖加密路径。

依赖图/许可影响：不新增自研 crate；Windows 会引入 `openssl-sys`+`openssl-src`（MIT / MIT+Apache-2.0，内含 OpenSSL 3.x），
按 AGENTS.md「依赖最小化」需在 PR 里报备用途与许可——报备表已落在 §7 P0「Windows 依赖报备」。

### 2.2 `PRAGMA key` 注入点：位置已经被上游留好了

**好消息**：sqlx 0.9 是 SQLCipher-aware 的，键顺序是硬约束的：

- `SqliteConnectOptions::new()` 就预留了 `key` 槽位在 **IndexMap 第 0 位**（`sqlx-sqlite-0.9.0/src/options/mod.rs:127`），
  后面依次是 `cipher_*`（`:129-170`）→ `page_size`→`locking_mode`→`auto_vacuum`→`journal_mode`→`foreign_keys`→`synchronous`（`:167-194`）。
- 连接建立时按这个顺序拼成一句多语句执行（`options/connect.rs:38`、`:69-80`）。
- 所以 **`.pragma("key", …)` 无论什么时候调，都会排在第一条**（IndexMap 替换值不换位置）。

两个可选注入面：

| 面 | 位置 | 评价 |
|---|---|---|
| A. 引擎自己的 options | `src/service/mod.rs:14682-14688` 追加 `.pragma("key", raw_key_spec)` | **推荐**。单点、能保留 `cache_size`、与 `open_with_options` 一致 |
| B. vendor 的 `open_with_passphrase` | `vendor/presage/presage-store-sqlite/src/lib.rs:49-65` | 可用但**丢 `cache_size`**（它自己从 URL 重建 options），且走口令转义 + PBKDF2 |

**用 raw key，别用口令**：`raw_key_spec()` 的形状是 `x'<64hex>'`（connector 同款，`src/store.rs:640-641`）。
理由：
1. 跳过 KDF。SQLCipher 默认 `PBKDF2_ITER = 256000`（amalgamation `sqlite3.c:108441`），
   而池每 10–30 分钟就回收连接（`sqlx-core/src/pool/options.rs:161-162`），口令形态会把 256k 轮 KDF
   变成周期性阻塞；raw key 没有这一步。
2. 没有引号转义风险（vendor 的 `:55` 只处理 `'` 双写，raw key 里根本没有 `'`）。
3. 与桌面/connector 的 key 形态同源，跨仓对账简单。

**打开时必须立刻验真**，否则错 key 会等到第一次查询才炸：
`PRAGMA cipher_version`（纯 SQLite 返回空集/无名错误，SQLCipher 返回版本串）+ 一次强制首页读
（connector 的写法：`SELECT count(*) FROM sqlite_master` → `NotADatabase` 归一成 `StoreKeyRejected`，
`src/store.rs:11720-11750`）。

### 2.3 连接池与 WAL

现状（vendored `open_with_options`，`:67-92`）：

| 项 | 值 | 加密后的影响 |
|---|---|---|
| `journal_mode` | WAL（`:72`） | WAL 页同样被加密；`-wal`/`-shm` 与主库必须同生命周期 |
| `synchronous` | Full（`:73`） | 不变；SQLCipher 每页多一次 HMAC 计算，提交路径变长 |
| `busy_timeout` / `acquire_timeout` | 5s / 5s（`:74,82`） | 不变 |
| 连接数 | `max_connections(2)`（`:80-84`） | 每连接一份加解密上下文 + 页缓冲（与 page_size 同阶，约 4–8 KiB，**估算值，未实测**） |
| 连接回收 | sqlx 默认 idle 10min / max_lifetime 30min（`sqlx-core/…/pool/options.rs:161-162`） | **每次重建都要重新 `PRAGMA key`** → key 必须留在 pool options 里 |

关键结论：**sqlx 池把 options 常驻在 `RwLock<Arc<Options>>`**（`sqlx-core-0.9.0/src/pool/inner.rs:26`），
而 options 里就存着 key 字符串，**sqlx 不做 zeroize**。所以"用完即焚"在这个技术栈上做不到，
方案里不要写这种承诺（见 §3.4）。

WAL 细节：SQLCipher 加密的是页内容；`-shm` 只是 WAL 索引，不含消息/密钥明文。但**换文件时必须删 sidecar**：
明文 `-wal` 挨着加密主库会让下一次打开直接坏掉（connector 把 `remove_sidecars` 放进替换步骤里，
`src/store.rs:11785-11790`）。

### 2.4 schema 迁移

- presage 的 schema 迁移是 **DB 内** 的 `_sqlx_migrations` + `sqlx::migrate!()`（`vendor/…/src/lib.rs:86`）。
  加密不改变这套：连接带 key → 迁移照跑，checksum 与迁移记录都在库里。
- **不需要**为加密新增任何 SQL 迁移。唯一的新"迁移"是**文件格式迁移**（明文→加密），且它是**每账号一次性**的（§4）。
- 迁移实现的坑：不要把格式迁移塞进 `open_store`。`open_store` 被大量后台路径调用
  （`src/service/profile_refresh.rs:268,350`、`src/service/own_capabilities.rs:186`、
  `src/service/send_warmup.rs:124`、`src/service/mod.rs:9078` 证书巡检），而接收路径还给它套了 90s 超时
  （`src/service/mod.rs:9116`、`:9179`）。大库迁到 90s 外会被判 `Recoverable` → 反复重试迁移。
  应该做成**启动前置阶段**或显式子命令（§7 P2）。

### 2.5 三个"看起来能做、其实会翻车"的坑

1. **`PRAGMA key` 在纯 SQLite 上是静默 no-op**。SQLite 对未知 pragma 不报错，所以 feature 没翻时：
   加 key 的代码跑得好好的、测试全绿、库还是明文。**必须**把 `PRAGMA cipher_version` 非空写成断言（§7 P0）。
2. **`SQLITE_TEMP_STORE=2`**：SQLCipher 构建分支强制临时表进内存（`build.rs:147-152`），纯 SQLite 默认是落盘（`1`）。
   大 `ORDER BY`/`VACUUM`/`sqlcipher_export` 的 RSS 会变，这是内存影响最容易被漏掉的一条。
3. **测试面分叉**：vendor 独立 workspace 的默认是 SQLCipher，引擎是纯 SQLite；翻 feature 后
   vendor 的测试语义也变了，得两个 workspace 都跑一遍（`cargo test` 在 `vendor/presage/presage-store-sqlite` 里单独跑）。

### 2.6 可行性判定

**可行**。工作量集中在：feature 切换（小）、Key 通道（中）、格式迁移（中）、失败语义（中）。
技术上没有未知项，风险全部是**工程与构建**风险（Windows OpenSSL、fail-closed 完备性），不是协议风险。

---

## 3. 问题 2：密钥从哪来

### 3.1 现有通道盘点

| 链路 | 现状 | 证据 |
|---|---|---|
| 桌面 → connector | **通**：32B store key 作为 bootstrap payload 第二行（严格 129 字节 `64hex + \n + 64hex`） | connector `src/auth.rs:19-21,140-165`；桌面 `storeKey.ts:6-33` |
| connector 内部 | **通**：`StoreKey`（`Zeroizing`）+ `raw_key_spec()` → `x'…'` | connector `src/store.rs:608-641` |
| connector → engine | **断的，而且是故意的**：`.env_remove(STORE_KEY_ENV)` | connector `src/engine.rs:2695-2697` |
| engine 内部 | **不存在** | 全仓零命中 |
| engine argv | 只有 `--data-dir` 等（严格解析，非法 exit 2） | `src/main.rs:95-126` |

所以 M6 的本质是：**给 connector → engine 补一条 key 通道，并把"没有 key 就不工作"变成硬语义**。

### 3.2 三条候选

| 方案 | 做法 | 优点 | 致命缺点 |
|---|---|---|---|
| **A. env 传 KEK** | connector 起引擎时 `.env("KT_ENGINE_STORE_KEK", hex)` | 启动前就可用（引擎先开库后收 RPC）；零新文件；引擎已有 env 读取先例（`src/main.rs:133`、`:428`） | 同 UID 进程可读（`/proc/<pid>/environ`、`ps e`）；崩溃转储可能带上 |
| B. 账号目录 key 文件 | connector 往 `accounts/lk-*/store-key` 写 0600 | 不依赖进程环境 | **和库放一起 = 拷目录就解密**，等于没加密；且引擎自己创建 `lk-*` 目录（`src/service/mod.rs:14687`），connector 抢不到时序 |
| C. 运行时 RPC 给 key | 加一个 `storeKey` 握手方法 | key 不过环境 | **时序不成立**：`src/main.rs:472,476` 在主循环前就开库/扫库 |

**结论：A 做通道 + B 的"包裹文件"形态做落盘**（不是裸 key）：

### 3.3 推荐结构：KEK（env）→ 每账号 DEK（包裹落盘）

```text
桌面 safeStorage（已有）
  └─ profile store key（32B，bootstrap 第二行，已有）
       └─ connector 起引擎：KT_ENGINE_STORE_KEK=<hex>（新，仅 env，不进 argv/日志）
            └─ engine 启动读入 Zeroizing<[u8;32]>
                 KEK' = HKDF-SHA256(ikm=KEK, salt="kt-engine-store", info="dek-v1" || accountDirName)
                   ├─ 首开新账号目录：DEK = rand 32B（rand 0.9 已在图）→ AES-256-GCM(KEK', DEK) → store-key.v1 (0600)
                   └─ 已有账号目录：读 store-key.v1 → 解包 DEK → PRAGMA key = x'<hex>'
```

为什么是 DEK 而不是"KEK 直接当库 key"：

1. **轮换成本**：桌面 key 换机/轮换时，只重包 `store-key.v1`（几十字节），不用把每个账号的库重新 export。
2. **删除账号 = crypto-erasure 的一半**：先删 `store-key.v1` 再删库（顺序写死）。
3. **域分离**：connector 自己的 `connector.sqlite3` 用 profile key 原值，引擎用 HKDF 派生子钥，同密钥材料不复用。

包裹文件格式（建议钉死，别"随实现漂"）：

```text
magic   "KTEK" (4B)
ver     0x01   (1B)
key_id  8B     (随机/序号，日志只允许出现它和指纹)
nonce   12B
ct      32B + tag 16B
AAD     "kt-engine-store-v1" || accountDirName
```

**零新增依赖**：`aes-gcm 0.11`、`hkdf 0.13`、`sha2 0.11`、`rand 0.9` 都已在图（`Cargo.toml:53,100-103`）。
引擎现有用途是备份导出/存储服务，做这层包裹不需要新 crate，符合 AGENTS.md 的依赖最小化。

### 3.4 fail-closed 决策表（必须逐条落到代码与测试）

对照 connector 的成熟表（`src/store.rs:2444-2467`），引擎侧应等价：

| store 文件 | key（KEK/包裹） | 行为 |
|---|---|---|
| 不存在 | 有 | 新建**加密**库 + 写包裹文件 |
| 不存在 | 无 | **拒绝创建**，报 `StoreKeyMissing`（绝不悄悄造明文库） |
| 明文（头 `SQLite format 3`） | 有 | 走 §4 迁移（或按阶段策略先拒绝并提示迁移） |
| 明文 | 无 | **拒绝打开**（`PlaintextStoreRequiresKey`），不服务明文 |
| 加密 | 有且解得开 | 正常打开 |
| 加密 | 有但解不开/错 key | `StoreKeyRejected`（与"文件损坏"分开） |
| 加密 | 无 | `StoreKeyMissing`（**终态**，不是网络故障） |
| 任意 | 包裹文件损坏 | `StoreKeyCorrupt`，保留现场，不重建 |

判定"是不是明文"用**读文件头**，不要靠"打得开就算明文"（connector 同款：`db_file_state`，`src/store.rs:11696-11717`）。

> **落地名对齐**（2026-11-12，C-1）：上表「明文 + 有/无 key → 拒绝」这一族在 engine 侧落成
> **一个**码 `StorePlaintextRequiresMigration`，由默认 off 的开关
> `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED` 控制（见 §7 C.12）。off 时仍是矩阵 c 的
> legacy 打开（不拒绝、不迁移）；C-1 没有引入 `PlaintextStoreRequiresKey` 这个码名。

### 3.5 内存与日志卫生（诚实版）

- **key 会常驻堆**：sqlx 池持有 `Arc<Options>`（`sqlx-core/…/pool/inner.rs:26`），options 里的 key 字符串不 zeroize，
  连接每 10–30 分钟回收重建还要用它。方案里**不要写"用完清零"**，只能写：
  ① 用 raw key 缩短明文生命周期（无 KDF 中间态）；② 不进日志/argv/metrics；③ 进程退出靠 OS 回收。
- **日志红线**（与现有纪律一致）：key/DEK/包裹内容一律不进 stderr、不进 `tracing` 字段、不进 `engine.perfMetrics`；
  只允许 key_id + SHA-256 前 8 字节指纹（桌面/ADR 0006 同口径）。
- **env 的暴露面要写清**：同 UID 进程可读。这是"防磁盘/备份/同步盘泄露 + 防离线拷贝"的收益，
  不是"防本机同用户恶意进程"。别把威胁模型说大。

### 3.6 与 ADR 0006 的关系（别混）

仓根 `SIGNAL-ADR-0006-PER-ACCOUNT-STORE-KEY.md` 讨论的是 **connector 的 `connector.sqlite3` 拆库 + per-account key**
（53–74 人日，等 G1 合规口径）。M6 是 **engine 的 `store.db`**，两者 scope 不同、可以独立推进：

- ADR 0006 §1.4 第 4 条已经写明"engine 数据不在 connector key 范围"（该文 line 66）——M6 把这句话的现状填上。
- 反过来，M6 也不能借用 ADR 0006 的"per-account DEK"结论来宣称合规：那需要 ADR 0006 的 catalog/迁移状态机。
- 唯一要共享的是**key 形态与包裹格式思路**（`x'<64hex>'`、KEK 包 DEK、key_id+指纹）。

---

## 4. 问题 3：既有明文库的迁移路径

### 4.1 状态机（每账号）

```text
absent ──有 key──> 新建加密
plaintext ──migrating_copy──> migrating_verify ──> active_encrypted ──> plaintext_purged
                  └─任意失败──> plaintext（原文件不动，可重试）
```

标记落在 `accounts/lk-*/store.format`（新建文件，内容如 `sqlcipher-v1 keyid=<hex8>`），
**不要**用"打得开就是明文"这种隐式判定（§3.4）。

### 4.2 逐账号步骤（一次一账号，引擎该账号必须停）

前置：connector 对该账号发送返回可重试错误（或直接停该账号的引擎进程）；确认没有第二个进程持有 `store.db`
（Windows 上文件锁是硬失败）。

```text
1) 一致性快照：对明文库执行 PRAGMA wal_checkpoint(TRUNCATE)，再 VACUUM INTO '<acct>/store.plaintext.bak'
2) 导出加密库：在同一个（无 key 的）连接上
     ATTACH DATABASE '<acct>/store.db.staging' AS encrypted KEY "x'<64hex>'";
     SELECT sqlcipher_export('encrypted');
     DETACH DATABASE encrypted;
3) 校验（§4.3），任一不过 → 删 staging，原库不动，报错退出
4) 删旧 sidecar + 原子替换：rename staging → store.db；写 store.format
5) 起引擎验证（能 load_registration_data、能建流、能收一条）
6) 观察窗后删 store.plaintext.bak（或按保留策略留 7 天）
```

这套流程**不是新发明**：connector 的 `migrate_plaintext_store` 就是这五步（`src/store.rs:11752-11820`），
包括"先删上次残留 staging"、"checkpoint 再 `VACUUM INTO`"、"ATTACH 用绑定参数而不是拼 SQL"、
"替换时一并删 sidecar"、"失败只清 staging、原库原地不动"。

### 4.3 校验矩阵

| 层 | 检查 | 通过标准 |
|---|---|---|
| 文件 | `PRAGMA cipher_version` / 文件头 | 非空版本串；前 16 字节**不是** `SQLite format 3` |
| 结构 | `PRAGMA integrity_check`、`PRAGMA foreign_key_check` | `ok` / 无行 |
| 加密完整性 | `PRAGMA cipher_integrity_check` | 无输出（按页 HMAC 独立校验） |
| 数据 | 关键表 `COUNT(*)` 与明文侧对齐 | `kv`/`identities`/`sessions`/`pre_keys`/`signed_pre_keys`/`kyber_pre_keys`/`sender_keys`/`contacts`/`profiles`/`threads`/`thread_messages` 全等 |
| 迁移账本 | `_sqlx_migrations` 行数与 `success=1` | 与明文侧完全一致（否则会重跑迁移） |
| 语义 | 用引擎真实代码路径打开并 `load_registration_data()` | 返回 `Some`，设备名/ACI 与迁移前一致 |
| 反例 | 用**别的账号**的 DEK 打开 | 必须 `StoreKeyRejected`（不是"成功打开空库"） |
| 反例 | 用**旧明文库**的字节当加密库打开 | 必须失败，且不产生新文件 |

### 4.4 替换与 sidecar

顺序写死，不要"先删后写"：

```text
rename(staging → store.db) 之前：
  - 删 store.db-wal / store.db-shm（明文残留 + 会让加密库打不开）
  - 备份文件先落盘且 fsync（崩溃时它是唯一退路）
rename 之后：
  - 写 store.format（原子写：tmp + rename）
```

### 4.5 回滚

| 时点 | 回滚动作 | 数据风险 |
|---|---|---|
| 校验失败 / 未 rename | 删 staging + sidecar | 无（原库从未被动过） |
| rename 后、删备份前 | 停引擎 → `rename(store.plaintext.bak → store.db)` → 删 `store.format` + 删加密库 | 无（备份是 checkpoint 后的完整库） |
| 已删备份 | **只能靠加密库 + key**；key 没了就是数据不可读 | **高**，所以"删备份"必须是独立、可延迟、可审计的动作 |
| 整版本回滚（engine 二进制退回） | 旧二进制读不了加密库 → **必须先把库转回明文**（反向 `sqlcipher_export` 到无 key 的 ATTACH） | 需要**反向迁移工具**，别只留"删库重链"这条（那会丢历史） |

**反向迁移工具是发布前置**，不是"以后再说"：否则二进制回滚 = 让用户重新扫码。

### 4.6 崩溃断点

- 迁移进行中 kill -9：下次启动看 `store.format` 是否存在 + 主库文件头 + staging/备份残留，收敛到
  "明文继续可用"或"加密已生效"两态之一，**不允许**第三种半态。
- 断点信息不用另起数据库：`store.format` 的原子性（tmp+rename）就够表达"是否已 cutover"。
- `VACUUM INTO` 的目标文件若已存在会失败 → 每次先删（connector 同款，`:11757-11762`）。

### 4.7 残留与 crypto-erasure 的诚实边界

- 删了 `store.db` 不等于介质上没明文：`-wal`/`-shm`、freelist、APFS 快照、Time Machine、同步盘、
  以及迁移前产生的明文 `store.plaintext.bak` 都可能有旧数据。SSD 上"覆写删除"不可靠。
- 所以文案只能是"**新数据静态加密**"，不能写"历史明文已被安全擦除"。
- 纵深防御建议（可与 P3 同批，不阻塞）：开 `PRAGMA secure_delete=ON`（connector 已用，`src/store.rs:2474`）、
  WAL 尺寸上限、删账号时先删 `store-key.v1`。

---

## 5. 问题 4：性能与内存的可测指标

### 5.1 理论成本（可从源码算出来的三条）

| 成本 | 数值 | 来源 |
|---|---|---|
| 页内开销 | reserve = IV 16 + HMAC-SHA512 64 = **80B / 4096B ⇒ ≈ +1.95% 库体积** | amalgamation `sqlite3.c:14374-14376`（page 4096）、`:109457-109475`（reserve 计算）、`:113396-113410`（SHA512 digest 64）、`:108457`（默认开 HMAC） |
| KDF | 口令形态 256000 轮 PBKDF2；**raw key 为 0** | `sqlite3.c:108441`（`PBKDF2_ITER`） |
| 临时存储 | SQLCipher 构建强制 `SQLITE_TEMP_STORE=2`（临时表进内存），纯 SQLite 默认 `1`（落盘） | `libsqlite3-sys build.rs:147-152` |
| 每连接内存 | 加解密上下文 + 页级缓冲（与 page_size 同阶，约 4–8 KiB，**估算**）+ 现有 `cache_size=1MiB`×2 | `src/service/mod.rs:14678`、vendor `lib.rs:80-84` |

### 5.2 指标表（阈值是建议门禁，不是实测）

| # | 指标 | 采集点 | 基线来源 | 阈值 |
|---|---|---|---|---|
| M1 | store 打开耗时 p50/p95/max | 新增 `perf_metrics` 计时键 `store_open`（词表在 `src/service/perf_metrics.rs:33-60`，快照 `:258-287`） | 明文库同机同库 | 增量 ≤ 50ms/账号（raw key）；> 200ms 阻塞发布 |
| M2 | store 打开失败计数（按原因分桶：Missing/Rejected/Corrupt/Io） | 新增 counters | 0 | 任何非 0 都要能解释 |
| M3 | 单账号 RSS | connector `EngineStatus.rssBytes`（`src/engine.rs:272-278`） | 现网 ~64 MB/账号（引擎池设计文档实测） | 增量 ≤ 8 MB/账号 |
| M4 | 库体积 | `du -b store.db*`（含 wal） | 迁移前 | ≤ +3%（理论 +1.95%） |
| M5 | WAL 峰值 | `ls -l store.db-wal` 采样 | 迁移前 | 不超过现有 `journal_size_limit` 语义（引擎未设，等于不加约束前） |
| M6 | 收/发链路 p95 | `receive_envelope` / `send_text_total` / `rpc_dispatch` 既有计时键 | 迁移前同账号 | 回归 ≤ 10% |
| M7 | 迁移耗时 | P2 迁移器自报 | — | 小库(≤10MB) < 2s；单账号 p95 ≤ 30s；> 120s 中止 |
| M8 | 迁移峰值 RSS | 迁移进程采样 | — | 不超过引擎稳态 ×1.5（`VACUUM INTO` + export 双份页缓存） |
| M9 | 磁盘峰值 | 迁移期 `du` | — | ≤ 2.2× 单账号库体积（原库 + 备份 + staging） |

### 5.3 怎么采（待执行的命令）

```bash
# M3/M6：引擎自带快照（wire additive，现有方法）
printf '{"jsonrpc":"2.0","id":1,"method":"engine.perfMetrics"}\n' | ./kt-signal-engine --data-dir <DIR> jsonRpc

# M1/M2：同上快照里的 timings.store_open / counters.store_open_failed_*

# M4/M5
du -b <DIR>/accounts/<lk-*>/store.db*        # 迁移前
du -b <DIR>/accounts/<lk-*>/store.db*        # 迁移后 + 7 天 soak

# 结构校验（离线，需 sqlcipher CLI：brew install sqlcipher）
sqlcipher <DIR>/accounts/<lk>/store.db "PRAGMA key=\"x'<64hex>'\"; PRAGMA integrity_check; SELECT count(*) FROM _sqlx_migrations;"

# 明文/加密判定（最快的一条）
xxd -l 16 <DIR>/accounts/<lk>/store.db | grep -q 'SQLite format 3' && echo PLAINTEXT || echo ENCRYPTED
```

### 5.4 已知的确定性副作用（先写在这里，免得被当成 bug）

1. 库体积 +≈2%（页格式），无法优化。
2. 重排序/大查询的 RSS 可能上升（`TEMP_STORE=2`），与库大小相关。
3. 每次连接重建不做 KDF（raw key），但每页多一次 HMAC 校验 → **顺序扫描**变慢的幅度通常小于随机写。
4. 备份/杀软/第三方工具打不开 `store.db`（预期行为，不是损坏）。

---

## 6. 问题 5：失败模式（含"密钥丢失 = 数据不可读"）

### 6.1 头号失败模式：密钥丢失

> **结论必须写进产品文案、错误码、支持手册三处**：
> **引擎账号库一旦加密，DEK 只能由 `store-key.v1` + KEK 解出。KEK 丢（换机、safeStorage 重置、
> 用户目录被清、profile key 轮换没重包）或 `store-key.v1` 丢/坏 ⇒ **本机**该账号历史不可读，
> 不可逆、不可恢复。**

### 6.1.1 G-A 拍板记录（2026-11-11，**已拍板**）

> 口径来源：`docs/plan/link-history-transfer-status.md`（只读取证，逐条带 file:line）。
> 下面四段与原 G-A 的两问一一对应，措辞按「证据只支撑到哪一步」写，不放大也不缩小。

**① 接受「KEK 丢 = 本机历史不可读」——已接受。**

引擎账号库加密后 key 丢失/损坏：本机该账号历史（引擎库内的身份/会话密钥与会话记录）不可读，
不可逆、不可恢复；不承诺任何「半恢复/部分解密」。这是加密的代价，产品侧明确接受。

**② 不承诺「重扫码一定能恢复历史」——重链不得写成恢复手段。**

| 事实 | 证据 | 状态 |
|---|---|---|
| 首次链接由手机导出历史（transfer archive / backup5，一次性通道）**已实现** | `adr/0005-link-history-transfer-backup5.md`、`src/service/mod.rs:4037`（只在 `finish_link` 收口 spawn）、`docs/plan/link-history-transfer-status.md` §1–§2 | 已实现；首链真机取证一次（档案 6576B，`adr/0005…md:63-64`） |
| 「重链再导」：仓内**两处口径互相矛盾** | ADR 0005 `:137` 称「仅首次链接可导；重链/已注册安装不可再导」；parity matrix（`kt-desktop/.worktrees/signal-download-tiers/docs/plan/signal-official-parity-matrix.md:3425`）称「引擎恒声明（ADR 0005 每次新链接恒导入）」 | **待取证**（`link-history-transfer-status.md` §3.2 / 缺口 B1） |
| 官方文案支持「重链可转移」 | matrix `:1389`（Signal-Desktop `_locales` 逐字） | 只作参照，不是本仓取证 |
| 第二次链接的真机/日志证据 | 无（S1 只做首链一次，`adr/0005…md:53-64`） | **零取证** |

结论：**「重链再导」按未取证处理**，不得写进对外承诺；要写只能写条件句。

**③ UX 口径（对外唯一合法表述）**：

> 「重新配对（重新扫码链接），历史**可能**由手机再导入，取决于手机与备份设置。」

配套硬约束：

- **绝不静默新建明文库**：丢 key 时引擎侧落 `StoreKeyRequired`（C 阶段的代码面，见 §7「C 阶段」），
  终态、不退避重连、不替换文件；清数据/重链是桌面侧的**显式**动作，不自动触发。
- 不写「重链必然恢复历史」（无取证），也不写「永久不可恢复」——手机侧那份历史不因本机 key 丢失而消失。
- 云备份腿有前置门：未开通 Signal Secure Backups 的用户 `BACKUP_NOT_PROVISIONED` 诚实跳过
  （`link-history-transfer-status.md` §3.4），文案不得暗示兜底一定存在。

**④ 明文残留**：现有明文库/备份/文件系统残留**不做安全擦除承诺**（P4 只做常规删除 + 审计留痕）。
这条与 ① 一起构成 G-A 的完整口径。

代码面现状：丢 key 的 fail-closed 语义已就位（C 阶段的 `StoreKeyRequired`/`StoreKeyInvalid`/
`StoreKeyRejected`/`StoreCorrupt`，见 §7「C 阶段」）；**产品文案与支持手册仍是 P3 待办**。
桌面已有同款先例：`storeKey.ts:65-69`（"store 存在但 key 丢了 → 抛，绝不新建"）、`:352`（fail closed 并保留现场）。

### 6.2 失败模式全表

> **命名以 C.3 为准**：下表是评估稿的原口径，落地后的码位统一为
> `StoreKeyRequired`（原 `StoreKeyMissing`）/ `StoreCorrupt`（原 `StoreKeyCorrupt`）/
> `StreamOutcome::StoreTerminal`（原 `StreamOutcome::StoreKeyRejected`），
> 固定 stderr 行是 `event=store_open_failed code=… retryable=…`（不是 `event=store_key_rejected`）。
> 本表保留用于「为什么要有这些码」的推理链。

| # | 场景 | 现状行为（未改时） | 加密后必须的行为 | 落地动作 |
|---|---|---|---|---|
| F1 | KEK 缺失（env 没传） | 无感（明文） | 已有账号库 → `StoreKeyMissing` **终态**；新账号 → 拒绝创建 | 决策表 §3.4 + 单测三态 |
| F2 | KEK 错（换了 profile key） | 无感 | `StoreKeyRejected`，与"文件损坏"分开 | 强制首页读 + `NotADatabase` 归一（connector 先例 `store.rs:11720-11750`） |
| F3 | `store-key.v1` 丢失 | 无感 | 同 F1，**绝不**新建 DEK 覆盖 | 反静默重建闸（存在库 + 无包裹 → 报错） |
| F4 | `store-key.v1` 损坏（截断/位翻转） | 无感 | `StoreKeyCorrupt`，保留文件供取证 | magic/version/AEAD tag 三段校验 |
| F5 | 半实施：feature 没翻但代码加了 key | 全绿、库仍明文 | 不允许出现 | `PRAGMA cipher_version` 断言（P0 门禁） |
| F6 | 缺 key + 明文库（老用户升级） | 直接服务明文 | 拒绝服务，提示迁移 | `PlaintextStoreRequiresKey` + 迁移指引 |
| F7 | 迁移中断电 | — | 收敛到"明文可用"或"加密生效"，无第三态 | §4.6 |
| F8 | 迁移后残留明文备份 | — | 有保留策略 + 审计 | 删除动作独立、可延迟、可审计 |
| F9 | 错 key 但历史管道把它当网络故障 | **无限 `Recoverable` 重连 + 只有 warn**（`src/service/mod.rs:9179-9191`） | 新增终态 `StreamOutcome::StoreKeyRejected`（枚举 `:8472-8483`），stderr 一行 `ERROR event=store_key_rejected`（照 `src/main.rs:444` 的既有形状） | 改 match + 契约测试 |
| F10 | 无人值守环境下 key 丢失 | 用户看到"一直收不到消息" | 错误码可上报 + metrics 计数（M2） | `perf_metrics` counters |
| F11 | 未登记 `lk-*` 目录探测不到注册数据 | `remove_unregistered_link_store` 返回 false → 永远保留（`src/service/mod.rs:14705-14720`） | 行为不变但会**更多**（没 key 就打不开） | 改成"先读文件头分类：明文才尝试打开"，并把"加密但无 key"记一条 warn |
| F12 | 远端/杀软/备份工具打开库失败 | 无 | 预期，写进支持手册 | 文档 |
| F13 | 桌面 profile key 轮换 | 无感 | 只重包 `store-key.v1`，不重写库 | §3.3 的 DEK 设计 |
| F14 | 删账号顺序错（先删库后删 key） | 无 | 先删 `store-key.v1` 再删库/sidecar/备份 | 删除路径加断言 |

---

## 7. 问题 6：分阶段落地与验收命令

> 所有 command 都是**待执行**；本文未执行任何构建/迁移。

### P0 — 构建门禁 + 加密探针（1–2 人日，必须先过）

> **状态：已完成**（2026-11-11，kt-signal-engine `fix/admin-write` 工作区，未 commit）。
> 本节下方「原设计口径」保留评审原文；实际落地形状、偏差与证据见「P0 落地事实」。
> 文档头部那句「本文未改任何代码/未跑构建」只适用于 P1–P4（评估部分）。
>
> **2026-11-11 补丁（Windows 目标拆分，必须项）**：P0 首批的
> `sqlcipher = ["libsqlite3-sys/bundled-sqlcipher"]` 在 `x86_64-pc-windows-msvc`
> 上会让 libsqlite3-sys build.rs panic，而 release matrix 含该 target
> （`.github/workflows/release.yml:39-40`）。现按 §2.1 改为**两段 target 依赖**，
> `sqlcipher` 降级为纯 `compile_error!` 门；macOS 依赖图与产物不变（不引
> OpenSSL）。见「Windows 目标修复」与「Windows 依赖报备」。

#### 原设计口径（评审原文，保留）

改动：
1. `Cargo.toml:33-35` 按 §2.1 换成按目标的 SQLCipher feature。
2. 新增 `tests/cipher.rs`（3 条断言）：
   - `PRAGMA cipher_version` 非空；
   - 用 `x'<hex>'` 建库后，文件前 16 字节 **不是** `SQLite format 3`；
   - 用**错 key** 打开 → 错误可归一到 `NotADatabase`（后续映射成 `StoreKeyRejected`）。
3. CI 加：`cargo test --test cipher`。

验收命令：

```bash
cargo test --test cipher -- --nocapture
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-pc-windows-msvc
# vendor 自己的 workspace 也要跑（它的默认就是 SQLCipher）
(cd vendor/presage/presage-store-sqlite && cargo test)
```

DoD：三平台产物可启动；`cipher_version` 断言绿；现有测试全绿；`cargo clippy --all-targets -- -D warnings` 绿。

#### P0 落地事实（本仓，已执行）

| # | 交付 | 位置 | 实现要点 |
|---|---|---|---|
| 1 | feature + 按目标依赖 | `Cargo.toml:9-23`（feature）、`Cargo.toml:158-180`（target 依赖） | `default = ["sqlcipher"]`、`sqlcipher = []`——feature **只做门**，不转发依赖特性。SQLCipher 的链接按目标声明：非 Windows `features = ["bundled", "bundled-sqlcipher"]`（macOS 走 CommonCrypto，`build.rs:220-223`，**不引 OpenSSL**）；Windows `features = ["bundled-sqlcipher-vendored-openssl"]`（隐含 `bundled-sqlcipher`→`bundled` 与 `openssl-sys/vendored`，libsqlite3-sys 0.36 `Cargo.toml:46-50`）。非 Windows 产物不变；Windows 新增 `openssl-sys`/`openssl-src`（build-time vendored，见「Windows 依赖报备」）。 |
| 2 | 编译期门（每个 bin 一份） | `src/main.rs:42-45`、`src/bin/backup5-spike.rs:15-21` | `#[cfg(not(feature = "sqlcipher"))] compile_error!("engine requires the sqlcipher feature: PRAGMA key is a silent no-op on plain SQLite")`。本仓 bin-only（无 `lib.rs`），**bin target 各自独立编译**，所以门要各写一份：主 bin 覆盖 `kt-signal-engine`，开发 bin 覆盖 `backup5-spike`（它也 `SqliteStore::open`，临时目录、无 key）。`cargo build --no-default-features` 与 `cargo build --no-default-features --bin backup5-spike` 都要 101。 |
| 3 | 运行时门（**前置**） | `src/store_cipher.rs`（`CipherGate` / `NegativeCache`）+ `src/service/mod.rs:14694-14744`（`open_store` → `open_store_gated`） | 顺序钉死为 **门 → 建目录 → 建 `store.db` → 打开并跑迁移**：`open_store_gated` 第一步 `gate.ensure()`，缺能力直接 `Err(())`，**不建目录、不建 `store.db`、不起连接池、不跑 `sqlx::migrate!()`**（这些正是 `open_with_options` 的副作用，vendor `presage-store-sqlite/src/lib.rs:67-92`）。判定结论**正负都缓存**：成功结论 `OnceLock`；失败进 `NegativeCache`（同样 `OnceLock`）→ 后续 open 只回同一错误，不重探 `:memory:`。失败证据行（`error!` + 固定 stderr 行）由 `CipherGate::report_denial_once` 的 `compare_exchange` 闸控，**每进程只落一次**，重试不刷屏。**门失败是终态**：`StoreCipherUnavailable.is_terminal() == true`，接收管线停环、**不进** Fibonacci 退避重连（C 阶段把「store 打不开」从 `Recoverable` 拆成终态/可重试两类）；stderr 行形状与其余六个码一致——`kt-signal-engine: ERROR event=store_open_failed code=StoreCipherUnavailable retryable=false`。**2026-11-12 评审纠正**：本行原文写的「走既有 store 打不开 → 接收管线可重试」是 C 阶段之前的语义；落地后已改为终态，因为缺能力是构建属性、不会自愈，退避重试只会反复碰盘并把「终态」语义在接收层悄悄推翻。 |
| 4 | 启动证据 | `src/main.rs`（`logfile::init` 之后、任何 store 打开之前） | 每进程一次：`WARN engine.startup: store encryption capability verified stage="store_cipher" cipher=SQLCipher 4.10.0 community`。级别取 WARN 是**落盘契约**（`src/logfile.rs:57` `MAX_LEVEL = LevelFilter::WARN`，info 不进滚动日志）；行内无密钥/路径/号码，stderr 纪律不变。**失败分支不再由启动路径打第二行**：门已落唯一一条证据行（同一事实只出现一遍）。 |
| 5 | 断言 | `src/store_cipher_tests.rs`（6 条） | 见「测试」。 |

> **评审回改（2026-11-12）**：第 3 条原来把门放在 `open_with_options` **之后**（建连成功才判），
> 缺能力时磁盘上已经落了明文 `store.db` + schema；现已前置到打开之前，并补「缺能力 → 目标目录
> 零文件」「负缓存 + 证据行每进程一次」两条断言。第 3 条评审同时要求 `backup5-spike` 不得绕门：
> 该 bin 现在有独立 `compile_error!` + `#[path = "../store_cipher.rs"]` 引入同一份门，
> 打开前先 `ensure_cipher_capable()`。

**P0 边界（本批明确不碰）**：不引入 KEK/DEK、不写任何密钥文件、不改 store 迁移
（vendor 的 `sqlx::migrate!()` 原样）、不改连接池参数（`max_connections(2)` /
`acquire_timeout(5s)` / `cache_size=-1024` 全部原状）、不接 `PRAGMA key` 注入面
（P1 才做）。P0 只把「这条构建到底能不能用 SQLCipher」这一步钉死。

#### 测试（6 条，均为进程内单测；文件 `src/store_cipher_tests.rs`）

| 断言 | 名称 | 证明了什么 |
|---|---|---|
| a | `cipher_version_is_present_and_identifies_sqlcipher`（`src/store_cipher_tests.rs`） | `cipher_version` 非空且为 `<major>.<minor>.<patch>[ <build>]` 形状；落盘证据行含 `SQLCipher` 标识。**口径说明**：SQLCipher 自己的 `PRAGMA cipher_version` 实测是 `4.10.0 community`（不含产品名），所以「含 SQLCipher 标识」由 `capability_line()` 的 `SQLCipher <version>` 承担，而非硬拼在 pragma 返回值里。 |
| b | `keyless_store_remains_plaintext_readable_and_writable`（`src/store_cipher_tests.rs`） | 不设 key 时：`open_store` 建库后文件头仍是 `SQLite format 3\0`；无 key 的裸 sqlx 连接可读（`_sqlx_migrations`）可写（自建表 + insert）；再走 `open_store` 打开后数据仍在 → **存量明文库行为不变**。 |
| c | `keyed_store_is_unreadable_without_key`（`src/store_cipher_tests.rs`） | `PRAGMA key = "x'<64hex>'"` 建库写入后：文件前 16 字节是盐（≠ 明文头）；无 key 的 sqlx 连接查询报 `not a database`（code 26）；同 key 重开能读回 → **`PRAGMA key` 不是 no-op**。 |
| d | `missing_capability_refuses_before_touching_disk` | **评审第 1 条**：走生产同一条 `open_store_gated`（只把门换成自建「缺能力」实例）→ 返回 `Err(())`，且目标目录**零文件**：目录不存在或零条目、无 `store.db`、父目录（`accounts/`）都没被建出来 → 证明门在 `create_dir_all`/`open_with_options` 之前。 |
| e | `missing_capability_is_probed_and_reported_once` | **评审第 2 条**：首次判定失败后探测计数停在 1、证据行计数停在 1；再判定 3 次 + 再 `open_store` 2 次后两者仍是 1（负缓存命中，不重探 `:memory:`、不重复刷 error/eprintln），磁盘仍零文件。 |
| f | `capable_gate_probes_once_and_caches_version` | 正缓存对照：能力可用时也只探一次、版本串一致、失败证据行 0 条。 |

执行记录（本机 macOS aarch64 / rustc 1.99.0，全部实跑；2026-11-12 评审回改后重跑）：

```text
cargo fmt --all -- --check                        → 0（无 diff）
cargo clippy --all-targets -- -D warnings         → 0（Finished in 5.77s，零 warning）
cargo test --all-targets -- --test-threads=1      → 0；bin 653 条（652 passed / 1 ignored，原有 ignore）、
                                                    backup5-spike 0 条、tests/contract.rs 94 条全 passed
cargo test --bin kt-signal-engine -- store_cipher  → 6 passed（a–f；其中 d/e/f 为本批新增）
cargo build --no-default-features --bin backup5-spike → 101，唯一错误：
  error: engine requires the sqlcipher feature: PRAGMA key is a silent no-op on plain SQLite (backup5-spike bin)
cargo build --no-default-features                 → 101，两条 compile_error（main.rs + backup5-spike.rs）
启动日志实测（printf '' | ./target/debug/kt-signal-engine --data-dir /tmp/… jsonRpc，exit 0）：
  logs/engine-stderr.log: WARN engine.startup: store encryption capability verified
    stage="store_cipher" cipher=SQLCipher 4.10.0 community
  （仅此一条能力证据行；stderr 只有既有的 network path INFO 行，未新增 stderr 输出）
```

突变验证（把门临时移回 `open_with_options` 之前以外的位置 = 删掉门，跑完已还原）：
`missing_capability_refuses_before_touching_disk` 与
`missing_capability_is_probed_and_reported_once` 立即**红**
（`missing cipher capability must fail closed` / `open_store_gated(...).is_err()`）→
两条断言确实钉住「门在打开之前」而不是「门存在」。

缺能力路径的实机证据目前是**单测级**（d/e：注入门 + 断言零文件/计数），刻意不做成
`--no-default-features` 进程级验证——那条路已被两条 `compile_error!` 挡在编译期，
运行期门只是「feature 开了但链接到的 sqlite 没 cipher_version」（构建脚本被覆写等）
的兜底。

构建耗时（实测）：feature 切换后的增量 `cargo build` = **24.99s**（libsqlite3-sys +
sqlx-sqlite + sqlx + presage-store-sqlite + kt-signal-engine 五个 crate 重编 + 链接）；
其中 SQLCipher 混合体单独重编（`cargo clean -p libsqlite3-sys && cargo build -p
libsqlite3-sys`）= **2.09s**（debug/`-O0`，cc 编 `sqlcipher/sqlite3.c`）。**未跑** clean
全量构建（target/ 已有 66G 缓存），所以「从零构建」的绝对值没有实测数据。

**2026-11-11 补丁验证（Windows 目标拆分；本机 macOS aarch64 / rustc 1.99.0，全部实跑）**：

```text
cargo fmt --all -- --check                     → 0（无 diff）
cargo clippy --all-targets -- -D warnings      → 0（Finished in 2.61s，零 warning）
cargo test --all-targets -- --test-threads=1   → 0；bin 649 passed / 1 ignored（原有 ignore）、
                                                 tests/contract.rs 94 passed
cargo check --no-default-features              → 101，仍由 compile_error! 拦下：
  error: engine requires the sqlcipher feature: PRAGMA key is a silent no-op on plain SQLite
cargo metadata --filter-platform x86_64-pc-windows-msvc --no-deps
  → 本包依赖里 libsqlite3-sys 两条声明：
    target=cfg(not(windows))  features=[bundled, bundled-sqlcipher]
    target=cfg(windows)       features=[bundled-sqlcipher-vendored-openssl]
    （--no-deps 列的是 manifest 声明、不按平台裁剪；裁剪后的真实图看下面两条）
cargo tree --target x86_64-pc-windows-msvc -e normal,build
  → libsqlite3-sys → openssl-sys v0.9.118 → libc / openssl-src v300.6.1+3.6.3
cargo tree --target x86_64-pc-windows-msvc -e features -i libsqlite3-sys
  → feature "bundled-sqlcipher-vendored-openssl" ← kt-signal-engine
cargo tree --target aarch64-apple-darwin -e normal,build
  → libsqlite3-sys 下没有 openssl-sys/openssl-src（图里唯一的 openssl-macros 来自既有的
    boring/libsignal 栈，proc-macro，与本次改动无关）
target/debug/build 下 openssl* 构建目录 = 0（macOS 从未编 OpenSSL）
Cargo.lock：+openssl-src 300.6.1+3.6.3、+openssl-sys 0.9.118，libsqlite3-sys 依赖表 +"openssl-sys"
  （lock 是跨平台并集；macOS 构建不编译它们）
```

**未实跑**：Windows 产物构建与启动（本机无 MSVC/Perl/CMake）。上面证明的是**目标解析**与
非 Windows 编译面不变，不是 Windows 产物行为。

#### 偏差（相对本节原文设计）

- **D1 测试落点**：原文写 `tests/cipher.rs` + `cargo test --test cipher`。本仓是 bin-only
  crate（无 lib target），集成测试无法 import crate 内部（`open_store` 与探测都在 bin 里），
  所以三条断言落成 `src/store_cipher.rs` 的 `mod tests`（`cargo test --bin kt-signal-engine
  store_cipher`），仍由 `cargo test --all-targets` 全量覆盖。错 key 归一 `NotADatabase`
  一项按「**无 key 打开必须报错**」实现：SQLCipher 上错 key 与无 key 同为首页解密失败，
  `StoreKeyRejected` 的语义区分属 P1（`§6` 决策表），P0 只需证明「key 不是 no-op」。
- **D2 Cargo 形状（已修复，见「Windows 目标修复」）**：原文 §2.1 建议按目标拆
  `bundled-sqlcipher` / `bundled-sqlcipher-vendored-openssl`。P0 首批按任务书只加了**一个**
  转发 feature（`bundled-sqlcipher`），Windows 的 vendored OpenSSL 分支未落 → Windows 必挂。
  2026-11-11 补丁改成两段 target 依赖，`sqlcipher` 只留作 `compile_error!` 门（`sqlcipher = []`）。
- **D3 CI**：未改 `.github/workflows/ci.yml`（本批门禁口径是本地 fmt/clippy/test 全绿）。
  `ci.yml:46` 的 `cargo test --all-targets` 已经跑到这三条断言，无需单独加 `--test cipher`。
- **D4 未做**（不计入 P0 完成口径）：三平台 release 产物启动验证、vendor 独立 workspace 的
  `(cd vendor/presage/presage-store-sqlite && cargo test)`。

#### Windows 目标修复（2026-11-11，已落）

原形状 `sqlcipher = ["libsqlite3-sys/bundled-sqlcipher"]` 在 Windows 上的失败链：
`libsqlite3-sys` 0.36 `build.rs:157-175` 判 `is_windows && !bundled-sqlcipher-vendored-openssl`
且 `find_openssl_dir` 找不到 `OPENSSL_DIR` → **编译期 panic**（`Missing environment variable
OPENSSL_DIR …`）。release matrix 含 `x86_64-pc-windows-msvc`（`.github/workflows/release.yml:39-40`），
所以这是必须项而不是可选优化。

现形状（`Cargo.toml:158-180`）：

| 目标 | 声明 | 生效特性 | crypto |
|---|---|---|---|
| 非 Windows（macOS/Linux） | `[target.'cfg(not(windows))'.dependencies]` | `bundled` + `bundled-sqlcipher` | macOS：CommonCrypto（`build.rs:220-223`）；Linux：系统 `libcrypto` |
| Windows | `[target.'cfg(windows)'.dependencies]` | `bundled-sqlcipher-vendored-openssl`（→ `bundled-sqlcipher` → `bundled` + `openssl-sys/vendored`） | vendored OpenSSL 3.6.3，静态链 `libcrypto` |

`sqlcipher` feature 降级为**纯门**（`sqlcipher = []`，仍默认开）：只喂 `src/main.rs` 的
`compile_error!`，不参与依赖特性。语义变化要记一笔：**feature 不再控制"链不链 SQLCipher"**，
它只控制"允许不允许编译"（`cargo build --no-default-features` 仍会在 `compile_error!` 停下，
但链接的仍是 SQLCipher）。这样平台差异只有一处（target 依赖），feature 开关不会再把 Windows
推进 panic 分支。Windows 分支的说明注释写在 `Cargo.toml:171-178`（`cfg(windows)` 段内）。

#### Windows 依赖报备（AGENTS.md：依赖最小化）

`openssl-sys 0.9.118` + `openssl-src 300.6.1+3.6.3` **只在 `cfg(windows)` 入图**，按
AGENTS.md「依赖最小化：新增 crate 需在 PR 描述说明用途与许可」报备：

| 项 | 内容 |
|---|---|
| 为什么必须 | MSVC 无 CommonCrypto；单用 `bundled-sqlcipher` 会 panic（见上）。vendored 是 libsqlite3-sys 给出的唯一支持路径：`bundled-sqlcipher-vendored-openssl = ["bundled-sqlcipher", "openssl-sys/vendored"]`（`libsqlite3-sys-0.36.0/Cargo.toml:47-50`） |
| 用途 | **build-time only**：`openssl-src` 编出 OpenSSL 3.6.3 静态库，`openssl-sys` 通过 `DEP_OPENSSL_INCLUDE` 把头文件交给 libsqlite3-sys 编 SQLCipher（`build.rs:209-212`）；产物静态链 `libcrypto`，运行时不依赖系统 OpenSSL |
| 许可 | `openssl-sys` MIT；`openssl-src` MIT/Apache-2.0（内含 OpenSSL 3.x，Apache-2.0）。与本仓 AGPL-3.0-only 兼容，无 GPL-ink 冲突 |
| 构建前置 | Windows runner 需要 **Perl + CMake**（openssl-src 的构建前置）与 MSVC 工具链；构建时间会明显上升（OpenSSL 全量编译） |
| 非 Windows | 不引入：`cargo tree --target aarch64-apple-darwin` 里没有 `openssl-sys`/`openssl-src`，macOS 编译面与 P0 首批一致。Cargo.lock 会记条目（lock 是跨平台并集），但 macOS 不编译它们 |
| 未验证 | Windows 上 `crt-static`（`release.yml:65-66`）+ 静态 OpenSSL 的组合仍未实测（build.rs 另有 msvc `crt-static` 分支，两条路径叠加无上游背书）→ 仍是 G-B |

其余 Windows 侧未验证项（不因本次拆分消失）：

3. `crt-static` + 静态 OpenSSL 的构建/启动**未实跑**（本机 macOS，无 MSVC 交叉工具链）。
4. `bundled-sqlcipher` 追加 `-DSQLITE_TEMP_STORE=2`（`build.rs:147-152`，临时表/排序强制进内存）：
   非格式变更，但 Windows 上 RSS 曲线与今天不同，G-B 需一并观察。

### P1 — key 通道 + fail-closed 决策表（3–5 人日）

> **C 阶段状态（2026-11-11）**：本节的**引擎侧**已按新契约落地（env 名 `KT_SIGNAL_STORE_KEY`、
> 32B key 直接使用、终态统一为 `StreamOutcome::StoreTerminal`、六码错误面），见下方
> 「C 阶段」小节与 C.5 偏差表；本节原文保留作**原设计口径**。connector 接线与迁移器仍在 D 阶段。

改动（引擎）：
- 启动读 `KT_ENGINE_STORE_KEK`（新 env，名字待定，参考现有 `KT_SIGNAL_SOCKS_PROXY`/`KT_ENGINE_SINGLE_ACCOUNT` 命名，`src/main.rs:133,428`）；
- HKDF 派生子钥 → 解包/新建 `store-key.v1`（0600，tmp+rename）；
- `open_store` 拆成"分类 + 决策 + 打开"：`store_connect_options(path)` 保持纯（测试在用：`src/service/mod.rs:29583-29587`、`:21581,21602,21867`），新增 `store_connect_options_with_key(path, dek)`；
- 新增 `StreamOutcome::StoreKeyRejected` + `store_open` 计时键 + 失败分桶 counters。

改动（connector，走它自己的仓库流程）：`.env("KT_ENGINE_STORE_KEK", …)`（把 `src/engine.rs:2695-2697` 的
`env_remove` 改成"显式剔除旧名 + 显式注入新名"），key 来源 = bootstrap 第二行。

验收：

```bash
# 1) 没有 KEK：不该产生任何新的明文库
env -u KT_ENGINE_STORE_KEK ./kt-signal-engine --data-dir /tmp/e-nokey jsonRpc   # 期望: StoreKeyMissing，不新建 store.db
# 2) 有 KEK：新建的是加密库
KT_ENGINE_STORE_KEK=<64hex> ./kt-signal-engine --data-dir /tmp/e-key jsonRpc
xxd -l 16 /tmp/e-key/accounts/<lk-*>/store.db | grep -q 'SQLite format 3' && echo PLAINTEXT-BAD || echo ENCRYPTED
# 3) 换一把 KEK：必须是 Rejected，不是"新建空库"
KT_ENGINE_STORE_KEK=<另一把64hex> ./kt-signal-engine --data-dir /tmp/e-key jsonRpc
# 4) 指标
printf '{"jsonrpc":"2.0","id":1,"method":"engine.perfMetrics"}\n' | KT_ENGINE_STORE_KEK=<64hex> ./kt-signal-engine --data-dir /tmp/e-key jsonRpc
```

DoD：F1/F2/F3/F9 四条失败模式有单测；fake 模式（`dir=None`，`src/state.rs:7`）与无 store 路径零影响；
`tests/contract.rs:173-177` 的伪 `store.db` 字节仍按"打不开 → 保留"处理（测试语义不变）。

### C 阶段 — engine 半边：key 通道 + 五态矩阵 + 错误码面（**已落地**）

> **状态：已落地**（2026-11-11，kt-signal-engine @ `fix/admin-write` 工作区，**未 commit**）。
> **定位**：原 P1 的**引擎侧子集**——只做「读 key → 分类 → 决策 → 错误码」，不写 §3.3 的
> `store-key.v1` 包裹文件（KEK→DEK 留到下一批：包裹文件会引入第三类失败「包裹丢失/损坏」，
> 与 G-C 的「三类可分辨」冲突），不做迁移器（原 P2 / D 阶段），不改 connector（它仍
> `env_remove` 本变量，`src/engine.rs:2695-2697`）。所以**本阶段上线后生产路径的现实取值
> = 矩阵 a（明文兼容）**；b/c/d 是能力，要等 connector 接线（D 阶段）与迁移器（D 阶段）才会在
> 真机上出现。

#### C.1 交付与落点

| # | 交付 | 落点 | 要点 |
|---|---|---|---|
| 1 | env 形状解析 + 进程三态 | `src/store_key.rs`（`STORE_KEY_ENV` / `StoreKey::parse` / `StoreKeyState::process`） | 64 hex（大小写都收）或 32B base64（标准/url-safe、带不带 `=` 补位都收）；**空串按非法**（不是缺席）；非法 = `StoreKeyState::Invalid`，一个库都不建；key 不进 `Debug`/错误文本，只有 `pragma_literal()` 渲染 `"x'<64hex>'"`；`process()` 用 `OnceLock` 只读一次 env |
| 2 | 文件状态分类 | `src/store_open.rs`（`classify` / `StoreFileState`） | 只读元数据 + 16 字节头：`SQLite format 3\\0` = 明文；非明文头 = `Opaque`（密文与垃圾不可分）；0 字节 = `Absent`；短于 512B = `StoreCorrupt(Truncated)`；`store.db` 位置被目录占住 = `StoreCorrupt(Unreadable)` |
| 3 | 决策 + 打开 | `src/service/mod.rs`（`open_store` → `open_store_gated` → `open_store_with`；`store_connect_options_with_key`） | 顺序钉死：**能力门 → key 形状 → 文件分类 →（密文先验真）→ 建目录 → 打开并跑迁移**；前四步**不创建 `store.db` 主体**（能力门 / key 形状 / 文件分类全程零写入；**密文验真是读写连接**，SQLite 在 WAL 模式下会临时创建 `-wal`/`-shm`，连接收口即回收）——早期「前四步零落盘」的措辞已按实测纠正（见 C.5 与 C.6） |
| 4 | 密文验真 | `src/store_open.rs`（`probe_keyed_store`） | `create_if_missing(false)` + `PRAGMA key` + 强制首页读 `sqlite_master`；`not a database` / code 26 → `StoreKeyRejected`；malformed → `StoreCorrupt`；其余 → `StoreOpenFailed`（可重试） |
| 5 | 结构化错误码 + 固定 stderr | `StoreOpenError`（`code()` / `is_terminal()` / `report_once()`） | 码表：`StoreCipherUnavailable` / `StoreKeyInvalid` / `StoreKeyRequired` / `StoreKeyRejected` / `StoreCorrupt` / `StoreOpenFailed` / **`StorePlaintextRequiresMigration`**（C-1，见 C.12）；stderr 固定形状 `kt-signal-engine: ERROR event=store_open_failed code=<Code> retryable=<bool>`，**每进程每码一条**，七个码**无一例外**（2026-11-12 评审补齐：`StoreCipherUnavailable` 的固定行由 P0 门落 `store_cipher::unavailable_stderr_line()`（与 `StoreOpenError::stderr_line()` 逐字同形），本侧不重复打；可重试类 `StoreOpenFailed` 也落同形行 `retryable=true`——此前缺能力是自造形状、可重试类是自由文本，watchdog 都分类不到） |
| 6 | 不退化成网络重试 | `StreamOutcome::StoreTerminal`（`src/service/mod.rs`，`is_terminal()` 已含）+ 接收建流分支按 `error.is_terminal()` 分流 | 终态停环、不进 Fibonacci 退避；只有 `StoreOpenFailed` 才 `Recoverable`；`ReconnectSchedule::next_delay` 单测钉死返回 `None` |
| 7 | 启动证据 | `src/main.rs`（`store_open::report_startup_key_state`，紧跟 P0 能力证据） | 有效/缺席 = 滚动日志 WARN（`stage="store_key"`、`key=present` 或 `key=absent`）；非法 = **固定 stderr 行**（不必等第一次开库，connector 启动即能识别） |
| 8 | 断言 | `src/store_encryption_tests.rs`（17 条：14 条为 2026-11-12 评审批，3 条为同日**收尾**新增——可重试类两条失败路径的固定行 + 每码一个闸的可失败证据）+ `src/service/mod.rs` 既有断言扩面与 3 条证书维护断言 | 见 C.2 / C.4 / C.10 / C.11 |

#### C.2 五态行为矩阵（C 阶段契约逐条落点）

| # | key（`KT_SIGNAL_STORE_KEY`） | store.db | 行为 | 代码落点 | 断言 |
|---|---|---|---|---|---|
| a | 无 | 无 | 明文创建（**现状兼容，不变**）；**C-1 开关 on 时改判 `StoreKeyRequired` 终态**（没 key 建不出密文库，也就不许再造新的明文库——见 C.12 ⑨） | `open_store_with` 的 `(None, Absent)` → `store_connect_options`（无 key pragma）；开关 on 时第 3.6 步闸先于它 | 既有 `keyless_store_remains_plaintext_readable_and_writable` + `switch_off_absent_without_key_still_creates_plaintext_store` + `switch_on_absent_without_key_refuses_before_creating_plaintext` |
| b | 有 | 无 | 建**加密**库（打开时 `PRAGMA key`） | `(Some, Absent)` → `store_connect_options_with_key` | `keyed_fresh_store_is_created_encrypted` |
| c | 有 | 已是明文库（头 `SQLite format 3`） | **按 legacy 打开、不设 key**，日志 warn 一次说明待迁移（**D 阶段**做）；不重写、不加密 | `(Some, Plaintext)` → `warn_plaintext_legacy_once()` + 无 key 打开 | `plaintext_store_with_key_opens_legacy_and_is_left_intact` |
| d | 有 | 已是加密库 | 正常打开；**错 key = `StoreKeyRejected`**（终态），先验真再跑迁移 | `(Some, Opaque)` → `probe_keyed_store` → keyed 打开 | `wrong_key_is_rejected_and_never_rewrites_the_store` + `keyed_fresh_store_is_created_encrypted`（同 key 重开） |
| e | 任意 | 损坏 | **独立分类** `StoreCorrupt`（不混进 key 类、也不退化成网络重试） | `classify`（截断/不可读）+ `classify_failure_message`（malformed） | `corrupt_store_is_classified_apart_from_key_failures` |
| — | 无 | 已是加密库 | `StoreKeyRequired`（终态，**绝不静默新建明文库**） | `(None, Opaque)` | `encrypted_store_without_key_reports_store_key_required` |
| — | 形状非法 | 任意 | `StoreKeyInvalid`（终态，磁盘零文件） | key 判定在文件分类之前 | `invalid_key_shape_refuses_before_touching_disk` |
| — | 任意 | 任意 | 缺能力时先报 `StoreCipherUnavailable`（**门在最前**） | `open_store_with` 第 1 步 | `capability_gate_still_precedes_key_and_disk_actions` |

#### C.3 固定码面与「不退化」的落点

| 码 | 触发 | 终态 | connector 可区分面 | 断言 |
|---|---|---|---|---|
| `StoreCipherUnavailable` | 链接的 sqlite 不是 SQLCipher（P0 门） | 是 | P0 门落**同形固定行** `event=store_open_failed code=StoreCipherUnavailable retryable=false`（每进程一次，与 `StoreOpenError::stderr_line()` 逐字相等） | 既有 P0 d/e + `capability_gate_still_precedes_...` + `every_store_open_code_emits_the_fixed_line_once` |
| `StoreKeyInvalid` | `KT_SIGNAL_STORE_KEY` 形状非法 | 是 | 启动即落 `event=store_open_failed code=StoreKeyInvalid retryable=false` | `invalid_key_shape_refuses_before_touching_disk` |
| `StoreKeyRequired` | 无 key 却必须走密文：① 加密库 + 无 key；② **C-1 开关 on + 库不存在 + 无 key**（C.12 ⑨，补漏） | 是 | 同上形状，`code=StoreKeyRequired retryable=false` | `encrypted_store_without_key_reports_store_key_required` + `switch_on_absent_without_key_refuses_before_creating_plaintext` + `switch_on_absent_without_key_reports_once_per_process` |
| `StoreKeyRejected` | 加密库 + 错 key（或第 1 页损坏） | 是 | 同上，`code=StoreKeyRejected` | `wrong_key_is_rejected_and_never_rewrites_the_store` |
| `StoreCorrupt` | 截断/不可读/malformed | 是 | 同上，`code=StoreCorrupt` | `corrupt_store_is_classified_apart_from_key_failures` |
| `StorePlaintextRequiresMigration` | **C-1**：明文库 + `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on`（与有无 key 无关） | 是 | 同上，`code=StorePlaintextRequiresMigration retryable=false` | `switch_on_plaintext_*`（九格 ⑤/⑥）+ `switch_on_classifies_plaintext_by_header_before_opening` + `switch_on_plaintext_is_terminal_and_reports_once_per_process` |
| `StoreOpenFailed` | 权限/句柄耗尽/EIO/只读挂载/路径/连接池等其余失败 | **否**（保持既有重试语义） | 同上固定行 `code=StoreOpenFailed retryable=true`（2026-11-12 评审补齐：此前这是唯一没落固定行的码，stderr 是自由文本 `sqlite store open failed: …`）。**同日收尾**：该类在 `open_store_with` 内部的所有出口都经 `StoreOpenError::reported()`——路径非 UTF-8、`create_dir_all` 失败（EACCES/EROFS/ENOSPC）、连接串解析失败三处以前是裸 `return`/`?`，stderr **零行** | `failure_text_classification_maps_not_a_database_to_key_rejection` + `every_store_open_code_emits_the_fixed_line_once` + `classify_keeps_io_failures_retryable` + `create_dir_all_failure_reports_store_open_failed_once` + `non_utf8_store_path_reports_store_open_failed_once` |

接收侧：`serve_account_stream` 命中终态 → `StreamOutcome::StoreTerminal` → 接收循环直接
`return`（不 notify、不 `mark_terminal_account`：账号本身没坏，只是这一进程读不了盘），
**不进** `ReconnectSchedule` 的 Fibonacci 退避；`Recoverable` 只在 `StoreOpenFailed` 时出现。
connector 侧消费（把固定 stderr code 落进 `key_missing` / `data_corrupt` 修复桶）属 D 阶段。

#### C.4 断言（17 条；文件 `src/store_encryption_tests.rs`；其中 3 条是**新进程**用例）

| # | 名称 | 证明了什么 |
|---|---|---|
| 1 | `store_key_accepts_hex_and_base64_and_rejects_every_other_shape` | 64 hex（大小写）/ 32B base64（标准、url-safe、无补位）都收；短一位、长一位、31/33 字节、非 hex、空串全拒；`Debug` 打码、错误文本不回显候选值；raw key 字面量恰 69 字节 `"x'<64hex>'"` |
| 2 | `invalid_key_shape_refuses_before_touching_disk` | 矩阵 1：`StoreKeyInvalid` + 终态 + 账号目录都没建（零文件） |
| 3 | `keyed_fresh_store_is_created_encrypted` | 矩阵 b：新建库头非明文（≥1 页）、无 key 连接读不了、有 key 读得到迁移账本、同 key 可重开 |
| 4 | `wrong_key_is_rejected_and_never_rewrites_the_store` | 矩阵 d：错 key = `StoreKeyRejected` + 终态；头仍是密文、探针行仍在（用对 key 读回）、没有多余文件/半写 journal、无 key 仍读不了；换回对 key 文件照常打开 |
| 5 | `plaintext_store_with_key_opens_legacy_and_is_left_intact` | 矩阵 c：有 key 打开存量明文库成功；头仍是明文（未被原地转换）、存量行与迁移账本可无 key 读取；warn 每进程恰一次（与测试顺序无关） |
| 6 | `encrypted_store_without_key_reports_store_key_required` | 密文 + 无 key = `StoreKeyRequired` + 终态 + 文件字节不变（没有静默新建明文库） |
| 7 | `corrupt_store_is_classified_apart_from_key_failures` | 矩阵 e：明文头 + 垃圾页（带/不带 key）与「非明文头但短于一页」都归 `StoreCorrupt`，与 `StoreKeyRejected` 码不同、现场保留 |
| 8 | `capability_gate_still_precedes_key_and_disk_actions` | 缺能力时即使有合法 key、或 key 形状非法，也先报 `StoreCipherUnavailable` 且零文件（P0 门禁不回退） |
| 9 | `failure_text_classification_maps_not_a_database_to_key_rejection` | sqlx 文本归一：同一句 `not a database` 在带 key 时 → `StoreKeyRejected`、明文路径 → `StoreCorrupt`；`malformed` → `StoreCorrupt`；其余 → `StoreOpenFailed`（`is_terminal()==false`） |
| 10 | `classify_keeps_io_failures_retryable`（**2026-11-12 评审阻塞项 1**） | EACCES / ELOOP / ENOTDIR 三种真实 IO 失败都归 `StoreOpenFailed`（可重试），**不**判 `StoreCorrupt` 终态；端到端同判 |
| 11 | `corrupt_unreadable_layout_is_terminal_and_leaves_the_scene`（**评审 7b**） | `store.db` 位置被目录占住 = `Corrupt(Unreadable)` 终态，占位目录与账号目录条目都不被改动 |
| 12 | `probe_keyed_store_requires_write_access`（**评审 4**） | 验真是读写连接：目录只读 → 验真 `StoreOpenFailed`（可重试）；恢复可写后同一份文件照常打开（证明失败来自挂载/权限，不是内容） |
| 13 | `every_store_open_code_emits_the_fixed_line_once`（**评审 3**） | 七个码逐字同形（C-1 增 `StorePlaintextRequiresMigration`，同形同值） `event=store_open_failed code=<Code> retryable=<bool>`（含缺能力与可重试类）；缺能力行与 `store_cipher::unavailable_stderr_line()` 逐字相等。**闸的证据不在这条里**：它同一次循环里边打边读，`report_once()` 之后 `report_count()` 恒 1，合并闸也是绿（见 #15） |
| 14 | `legacy_warn_is_emitted_by_the_open_not_by_a_manual_gate`（**评审 7a**） | legacy warn **由 `open_store_with` 触发**（此前那条用例先手工打闸，等于把待证事实当前提）：无 key 打开不触发、第一次 legacy 打开恰一次、再开不增 |
| 15 | `each_store_open_code_emits_its_own_line`（**收尾 2 / C-1**） | **新进程**里三个真实 fixture 各触发一个码（密文头 + 无 key → `StoreKeyRequired`；短于一页 → `StoreCorrupt`；明文头 + 开关 on → `StorePlaintextRequiresMigration`），断言 stderr 三行都在、各恰一条、`required_first`/`corrupt_first` 两种顺序都成立；把两码合并到同一个 `AtomicU8` → 少一行 → 必红（变异实跑见 C.11 / C.12） |
| 16 | `create_dir_all_failure_reports_store_open_failed_once`（**收尾 1**） | **新进程** + 只读父目录：`create_dir_all` 失败必须落**恰一条** `code=StoreOpenFailed retryable=true`，且账号目录与 `store.db` 都不存在（root 下自动 SKIP，与 #12 同口径） |
| 17 | `non_utf8_store_path_reports_store_open_failed_once`（**收尾 1**） | **新进程** + 非 UTF-8 账号目录名（`path.to_str()` 为 `None`）同样落恰一条可重试固定行，且不留账号目录 |

另：`src/service/mod.rs` 两条既有断言扩面——`StreamOutcome::StoreTerminal.is_terminal()` 为真、
`ReconnectSchedule::next_delay(StoreTerminal)==None`（不进退避重连）。

#### C.5 与 P1 原设计的偏差（据实登记）

| 原 P1 设计 | 本批实际 | 原因 |
|---|---|---|
| env 名 `KT_ENGINE_STORE_KEK` | **`KT_SIGNAL_STORE_KEY`** | 与 connector 既有同名变量、同一把 32B 材料同源（`kt-signal-connector/src/store.rs:98`），省一次跨仓改名；KEK→DEK 包裹落地时再决定是否另立变量 |
| KEK（env）→ HKDF → 每账号 DEK → `store-key.v1` 包裹落盘 | **本批直接使用 env 里的 32B key**（不做包裹文件） | 包裹文件引入第三类失败（包裹丢失/损坏），与 G-C「KEK 缺失/错误/库损坏三者可分辨」冲突；包裹与轮换属 D 阶段 |
| 明文 + 有 key → 走 §4 迁移（或拒绝） | **legacy 打开 + warn 一次**（不拒绝、不迁移） | 迁移不能塞进 `open_store`（§2.4 的 90s 预算）；现在拒绝会直接打断存量用户，违反矩阵 c 的「不破坏存量」 |
| `StreamOutcome::StoreKeyRejected` 单一终态 | **`StreamOutcome::StoreTerminal`** 一个终态 + 结构化 `code` | 四种终态（缺/错/非法/损坏）都要停环，码面已能区分，不必各开一个枚举变体 |
| 新增 `store_open` 计时键与失败分桶 counters | 未做 | C 阶段只承诺「可区分 + 不重试」；指标属 B/P1 观测面，另批 |
| P0 门失败按「store 打不开 → 接收管线可重试」处理 | **终态**：`StoreCipherUnavailable.is_terminal()==true`，停环、不进 Fibonacci 退避重连 | 门失败是构建属性、不会自愈；退避重试只是反复碰盘，并且会在接收层把「终态」语义悄悄推翻（**2026-11-12 评审第 5 条**纠正 P0 段口径与本行） |
| 「前四步零落盘」 | **前四步不创建 `store.db` 主体**；密文验真是读写连接，会临时创建 `-wal`/`-shm`（连接收口即回收） | 验真要读首页/`sqlite_master`，WAL 模式下 SQLite 必须建 sidecar；改成只读/immutable 探测会漏读 WAL 页、可能把好库判成错 key（错得更贵）。测试白名单（`store.db` 家族）本就含 sidecar，现与文档对齐（**评审第 4 条**） |
| IO 失败判 `StoreCorrupt`（旧实现把所有非 NotFound 错误都归到 `Corrupt(Unreadable)`） | **`StoreOpenFailed`（可重试）**；`Corrupt` 只保留稳定可证的结构事实（短于一页、位置被非普通文件占住、SQLite 报 malformed） | EACCES/EMFILE/EIO/只读挂载判终态 = 账号**永久停接收**，而这类条件外部一恢复就好（**评审阻塞项 1**） |
| 证书维护任务按 60s→600s 无限重开磁盘 | 打开命中终态即返回 `CertTaskExit::StoreTerminal`（不再重开）；管线收口后 `abort` 兜底 | 后台任务用重试把接收层的「终态」判定推翻，错 key 重试一万次还是错 key（**评审阻塞项 2**） |
| 缺能力 / 可重试类不落固定 stderr 行 | 六码**全落**同形固定行（缺能力行由 P0 门以同形同值落；可重试类 `retryable=true`） | connector watchdog 只按 `event=store_open_failed code=… retryable=…` 逐行分类；缺能力原来是自己一种形状、可重试类是自由文本（**评审第 3 条**） |
| `StoreKey` 的 `Drop` 清零算内存保障 | 只覆盖它自己那份 `[u8;32]` 副本；进程内**不保证**清零，进程退出由 OS 回收 | 生产 key 常驻 `StoreKeyState::process()` 的 `OnceLock`，`pragma_literal()` 的 `String` 被 sqlx 移进 `SqliteConnectOptions`（`Cow::Owned`），`Drop` 都覆盖不到；能清的是 env 读入那一份（`process()` 里解析后立刻 `fill(0)`）（**评审第 6 条**） |

#### C.6 已知边界与已知风险（诚实登记）

**边界（分类学限制，不是缺陷）**

- **错 key 与「第 1 页损坏」不可分辨**：SQLCipher 里两者都是 HMAC 失败 → `not a database`。
  本批把「`Opaque` + 有 key + 首页读失败」判 `StoreKeyRejected`，把**结构性**损坏
  （短于一页、明文头但内容非法、SQLite 报 malformed）判 `StoreCorrupt`。第 1 页已损坏的加密库
  会被判成错 key——两者都是终态、都要人工介入，现场靠 `code` 与文件本身区分。
- **「非明文头」不等于「真的是密文」**：SQLCipher 文件头是随机盐、没有 magic；垃圾文件 + 有 key
  会落到 `StoreKeyRejected`。
- 明文库的 legacy warn **每进程一次**：多账号同时命中只留一条（够用、不刷屏）。
- 判定用「短于 512B = 截断」的下界（SQLite 最小页大小），不猜具体页大小（SQLCipher/sqlx 缺省 4096）。
- **「零落盘」的准确口径**：能力门 / key 形状 / 文件分类三步全程零写入；**密文验真不是**——
  它是读写连接，SQLite 在 WAL 模式下必须建 `store.db-wal`/`store.db-shm`（本机实测：正确 key 的
  验真返回后立刻能看到 `store.db-wal`，连接收口后回收）。这也是「只读/immutable 探测」不可行的
  原因：`immutable=1` 会忽略 WAL 内容，把「schema 还在 WAL 里」的健康库读成空的/不可解，
  反而误判 `StoreKeyRejected`（假终态比多一个 sidecar 贵得多）。
- **IO 失败与损坏的边界**（评审阻塞项 1）：`classify` 只有两个 IO 分支——`NotFound` → `Absent`，
  其余一律 `StoreOpenFailed`。`Corrupt` 只保留**稳定可证的结构事实**：长度短于一页、`store.db`
  位置被非普通文件占住（目录/设备/FIFO；不会自愈，等人工）、SQLite 自己报 malformed/not-a-database。
  短读（`UnexpectedEof`）也算 IO 失败：元数据说 ≥512B 而这一刻读不满 16B，多半是并发替换/截断，
  下一次分类会按当时的长度重判。

**风险（本批实测发现，D 阶段必修）**

- **带 key 的 sqlx 池在进程退出瞬间偶发 SIGSEGV**：`SqliteStore`（2 连接池）drop 只发关闭信号，
  SQLite worker 线程仍在释放 SQLCipher codec 上下文时若进程已退出，本机 **1/5** 复现
  `SIGSEGV`（退出码 139）。测试面已用 `quiesce_store_teardown()`（120ms）确定性收口，
  复跑 10/10 稳定；**生产面**（stdin EOF 后 `amain` 返回、局部 `Arc` 析构）共享同一窗口，
  需在 D 阶段加**显式 shutdown drain**（或 vendor 暴露 `SqliteStore::close()`），并配合
  sanitizer/真机退出码观察。这条不是 C 的矩阵项，但**不能当没看见**：退出码被 connector 当作
  「崩溃」会污染 restart 计数。

#### C.7 D 阶段（下一批）待办

1. **迁移器**（原 P2）：`storeMigrate` 子命令或启动前置阶段，把矩阵 c 的明文库一次性转加密
   （§4.2 五步 + §4.3 校验矩阵 + §4.5 回滚）；迁移完成后 `warn_plaintext_legacy_once` 的「待迁移」
   语义即可移除。**正向已落地**（见本文件末尾「D1 状态」）；反向（`--direction to-plaintext`）
   与 C-1 一起留 D2。
2. **connector 接线**（走 connector 自己的仓库流程）：`src/engine.rs:2695-2697` 从 `env_remove`
   改为显式注入 `KT_SIGNAL_STORE_KEY`（key 来源 = bootstrap 第二行）；届时矩阵 b/d 才真正生效。
3. **connector 侧消费固定码**：把 `event=store_open_failed code=…` 落进 `key_missing` /
   `data_corrupt` 修复桶并标 `retryable=false`，不再做 5 次无意义重启
   （`docs/plan/signal-runtime-unified-state-implementation-plan.md` 已登记方向）。
4. **shutdown drain**：见 C.6 的 SIGSEGV 窗口——引擎退出前显式等 SQLite worker 收口（或 vendor 暴露
   `SqliteStore::close()`），并用真机退出码 / sanitizer 复核。
5. 可选观测面：`store_open` 计时键 + 失败分桶 counters（原 P1/M2）。

#### C.8 验收命令（本批实跑）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets -- --test-threads=1
cargo test --bin kt-signal-engine -- store_encryption_tests --test-threads=1
```

启动面手工核对（不建库、不落明文；`printf ''` 立即 EOF，进程干净退出）：

```bash
printf '' | KT_SIGNAL_STORE_KEY=not-a-key ./target/debug/kt-signal-engine --data-dir /tmp/e-key-invalid jsonRpc
# stderr 期望：kt-signal-engine: ERROR event=store_open_failed code=StoreKeyInvalid retryable=false
#             且 /tmp/e-key-invalid 下不出现任何 store.db
```

#### C.9 执行记录（2026-11-11，本机 macOS aarch64 / rustc 1.99.0，全部实跑）

> 下表是 C 阶段首次落地的记录（历史快照，计数已过期）；**评审批之后的当前门禁结果见 C.10**。

| 命令 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | 0（无 diff） |
| `cargo clippy --all-targets -- -D warnings` | 0（零 warning） |
| `cargo test --all-targets -- --test-threads=1` | 0：bin 662 条（661 passed / 1 ignored，既有 ignore）、`tests/contract.rs` 94 条全 passed、backup5-spike 0 条 |
| `cargo test --bin kt-signal-engine -- store_encryption_tests --test-threads=1` | 9 passed（C.4 全部） |

启动面实测（debug bin，stdin 立即 EOF）：

```text
$ printf '' | KT_SIGNAL_STORE_KEY=not-a-key ./target/debug/kt-signal-engine --data-dir /tmp/e-key-invalid jsonRpc
kt-signal-engine: INFO network path: std-env (socks5://127.0.0.1:7890)   # 本机代理环境
kt-signal-engine: ERROR event=store_open_failed code=StoreKeyInvalid retryable=false
exit 0；/tmp/e-key-invalid 下只有 logs/，没有 store.db

$ printf '' | KT_SIGNAL_STORE_KEY=<64hex> ./target/debug/kt-signal-engine --data-dir /tmp/e-key-valid jsonRpc
exit 0；stderr 安静；滚动日志 WARN engine.startup stage="store_key" key=present
         "store key present; new stores are created encrypted"
```

**SIGSEGV 复现与收口（C.6 风险条的证据）**：`keyed_fresh_store_is_created_encrypted` 单独连跑 5 次
出现 1 次 exit 139（其余 exit 0）；加 `quiesce_store_teardown()`（120ms）后连跑 10 次全 0。
对照组 `keyless_store_remains_plaintext_readable_and_writable` 8/8 干净、scratch「仅开 keyed 池」
8/8 干净 → 窗口在「keyed 池 + 进程退出」的组合上，不是 key 本身。

#### C.10 评审批（2026-11-12，7 条）—— 落地与证据

> 触发：C 阶段复审提出 7 条（含 2 条阻塞）。全部落在本仓工作区，**未 commit**；
> 门禁 = `cargo fmt` / `cargo clippy -D warnings` / `cargo test --all-targets`（含
> `tests/contract.rs`）/ `store_encryption_tests` 定向跑。

| # | 评审条目 | 落地 | 证据 |
|---|---|---|---|
| 1（阻塞） | `classify` 把 metadata / `File::open` 的**非 NotFound** 错误判成 `Corrupt` 终态 → 账号永久停接收 | 读盘动作失败统一 `StoreOpenFailed`（可重试）；`Corrupt` 只留**稳定可证的结构事实**（短于一页、`store.db` 位置被非普通文件占住、SQLite 报 malformed）；短读（`UnexpectedEof`）也算 IO 失败——元数据与真相打架时不钉终态 | `src/store_open.rs` `classify`；`classify_keeps_io_failures_retryable`（EACCES / ELOOP / ENOTDIR + 端到端同判） |
| 2（阻塞） | 证书维护任务在 store 终态后仍按 60s→600s **无限重开磁盘** | `certificate_maintenance_tick` 返回 `CertTickOutcome`：终态 → 任务返回 `CertTaskExit::StoreTerminal`；`receive_pipeline` 在管线收口后 `abort` 兜底（终态/Unbound/停机都停手） | `certificate_tick_terminal_store_open_stops_the_task`、`certificate_maintenance_loop_stops_after_terminal_store_open`（终态只碰一次盘）、`certificate_maintenance_loop_keeps_retrying_transient_store_failures`（可重试类按 600s→60s 继续，停旗即收口） |
| 3 | 缺能力 / 可重试类不落固定行，与本文「每进程每码一条」不一致 | 六码**全落**同形固定行：缺能力那条由门落 `store_cipher::unavailable_stderr_line()`（与 `StoreOpenError::stderr_line()` 逐字相等），可重试类落 `retryable=true`；`slot()` 改为**每码一个闸**（缺能力不再与 `Failed` 共用槽） | `every_store_open_code_emits_the_fixed_line_once`（形状逐字 + 幂等 + 互不压制 + 缺能力行与门逐字相等）+ `tests/contract.rs::store_open_failed_line_is_emitted_once_per_process`（**真进程**：非法 key 启动 → stderr 全量只有恰一条 `code=StoreKeyInvalid retryable=false`、exit 0、不建 accounts/） |
| 4 | 「前四步零落盘」与实测不符 | **选「降级文档 + 对齐白名单」**，不做只读/immutable 探测：验真是读写连接，WAL 下必建 `-wal`/`-shm`（实测正确 key 的验真返回后立刻可见 `store.db-wal`）；`immutable=1` 会忽略 WAL 内容，可能把「schema 还在 WAL 里」的健康库读成不可解 → 误判 `StoreKeyRejected`（假终态比多一个 sidecar 贵得多）。文档改为「不创建 `store.db` 主体，可能产生 sidecar」，测试白名单（`store.db` 家族）同步写明理由 | `probe_keyed_store_requires_write_access`（目录只读 → `StoreOpenFailed` 可重试；恢复可写后同一份文件照常打开）+ C.6「零落盘的准确口径」 |
| 5 | 文档登记行为变更 | P0 #3「门失败可重试」→ **终态**（`retryable=false`，停环不重连）；C.5 补 6 行偏差；`store_open.rs` 注释「三类」→「**五类**」；`docs/jsonrpc-compat.md` §1「四类」→「五类」 | 本文 P0 #3 / C.5；`docs/jsonrpc-compat.md` §1 stdio 条目 |
| 6 | 「Drop 清零」被当成内存保障 | 措辞改为「**进程退出由 OS 回收，进程内不保证清零**」：`StoreKey::Drop` 只覆盖自己那份 `[u8;32]` 副本；`pragma_literal()` 的 `String` 随 `.pragma()` 移进 `SqliteConnectOptions`（`Cow::Owned`）常驻，拿不回所有权，复制一份再清只会多一份材料；**能清的确实清了**——`StoreKeyState::process()` 读入的 env 拷贝在解析后立刻 `fill(0)`（`String::into_bytes()`，安全代码、不引 unsafe） | `src/store_key.rs` 模块头「内存卫生」/ `Drop` / `process`；`src/store_key.rs` 的 `pragma_literal` 文档 |
| 7 | 三处缺口测试 | 7a legacy warn **由 `open_store_with` 触发**（不再先自打闸）：配 async 串行闸 + `reset_legacy_plaintext_warn()`，并行用例不再互相推高进程级计数；7b `Corrupt(Unreadable)`（目录占住 `store.db`）终态且不动现场；7c `report_once` 每码一次（槽是进程级的，断言取**幂等 + 互不压制 + 形状逐字**，并用门的 `denial_reports` 覆盖缺能力那条） | `legacy_warn_is_emitted_by_the_open_not_by_a_manual_gate`、`corrupt_unreadable_layout_is_terminal_and_leaves_the_scene`、`every_store_open_code_emits_the_fixed_line_once` |

门禁实跑（本机 macOS aarch64 / rustc 1.99.0，全部实跑）：

```text
cargo fmt --all -- --check                        → 0（无 diff）
cargo clippy --all-targets -- -D warnings         → 0（零 warning）
cargo test --bin kt-signal-engine -- store_encryption_tests --test-threads=1 → 14 passed
cargo test --bin kt-signal-engine -- store_cipher                            → 6 passed
cargo test --bin kt-signal-engine -- certificate_                            → 5 passed
cargo test --all-targets -- --test-threads=1      → 0：bin 670 条（669 passed / 1 ignored，既有 ignore）、
                                                    tests/contract.rs 95 passed（+1 本批新增）、backup5-spike 0 条
```

已知未覆盖（诚实登记）：EMFILE / EIO 没法在单进程里稳定造出来，只由 `classify` 的同一分支覆盖；
Windows 上 `#[cfg(unix)]` 的三条 IO 用例不编译（权限位/符号环语义不同），Windows 面仍只靠编译与
`store_cipher` 断言。

#### C.11 收尾（2026-11-12，2 条）—— 可重试类的每条出口 + 每码一闸的可失败证据

> 触发：C.10 落地后的两条尾巴。同样落在工作区、**未 commit**；门禁同 C.10。

| # | 问题 | 落地 | 证据 |
|---|---|---|---|
| 1 | `open_store_with` 里三处裸 `StoreOpenError::Failed`（路径非 UTF-8 / `create_dir_all` 失败 / 连接串解析失败）直接 `return`/ `?`，**不走** `report_once`：建目录 EACCES/EROFS/ENOSPC、路径非 UTF-8 这两条真实失败路径的 stderr **零行**——同一函数里只有 `classify` / `probe_keyed_store` 的失败有固定行，watchdog 逐行分类等于看不见「盘写不了」 | 新增 `StoreOpenError::reported()`（落一次证据再把自己交回，与 `refuse()` 同一道闸，调两次仍只一行），三处全部改走它；`create_dir_all` 失败额外把 `io::ErrorKind` 落滚动日志（只进 kind 字段，不带路径/错误文本）。闸仍是**每进程每码一条** | `create_dir_all_failure_reports_store_open_failed_once`（只读父目录 → 新进程 stderr **恰一条** `code=StoreOpenFailed retryable=true`，账号目录与库都没建）、`non_utf8_store_path_reports_store_open_failed_once`（非 UTF-8 目录名同判） |
| 2 | C.10 第 7c 条「每码一条」的断言在**同一次循环里**边打边读：`report_once()` 之后 `report_count() == 1` 无论闸共不共用都成立，**把两个码合并成同一个 `AtomicU8` 也不会红**（假绿） | 改成**新进程 + 真 stderr** 的独立用例：两个不同码各触发一次（顺序可反转），断言两行都在、各恰一条；旧循环里的形状断言保留，闸的证据移走 | `each_store_open_code_emits_its_own_line`（`StoreKeyRequired` + `StoreCorrupt` 两个真实 fixture，`required_first`/`corrupt_first` 各跑一个新进程） |

**为什么必须换进程**：`report_once` 的闸是进程级一次性 `AtomicU8` 槽，同进程里任何先跑的用例都会把它消费掉——那时断言「stderr 恰一条」读到的是别人落下的历史行，永远绿。新进程 = 闸归零 + stderr 干净，才可能真的证明「这条失败路径落且只落一行」。子用例本身是 `#[ignore]` 的（`--ignored --exact` 拉起），父用例还会断言子进程 `1 passed`（名字漂了 `--exact` 只会静默 0 用例）。

**变异验证（本机实跑，三处变异均已回滚、文件 md5 与变异前逐字节一致）**：

```text
变异 A：第 6 步改回 std::fs::create_dir_all(dir).map_err(|_| StoreOpenError::Failed)?
        → create_dir_all_failure_reports_store_open_failed_once FAILED（断言 lines.len(): left 0 / right 1）
变异 B：非 UTF-8 分支改回 return Err(StoreOpenError::Failed);
        → non_utf8_store_path_reports_store_open_failed_once FAILED（left 0 / right 1）
变异 C：store_open.rs slot() 的 Self::Corrupt(_) => &CORRUPT 改成 => &KEY_REQUIRED
        → each_store_open_code_emits_its_own_line FAILED（stderr 只剩 code=StoreKeyRequired 一行；left 1 / right 2）
```

门禁实跑（本机 macOS aarch64 / rustc 1.99.0，2026-11-12）：

```text
cargo fmt --all -- --check                                  → 0（无 diff）
cargo clippy --all-targets -- -D warnings                   → 0（零 warning）
cargo test --bin kt-signal-engine -- store_encryption_tests → 17 passed / 3 ignored（三条 child 子用例按需拉起）
cargo test --bin kt-signal-engine -- store_cipher           → 6 passed
cargo test --bin kt-signal-engine -- store_open             → 7 passed
cargo test --all-targets -- --test-threads=1                → 0：bin 672 passed / 4 ignored（3 条本批 child + 1 条既有 ignore）、
                                                              tests/contract.rs 95 passed、backup5-spike 0 条
```

已知边界：连接串解析失败（`store_connect_options*` 的 `Err(())`）没有独立用例——它要求
`path_str` 是合法 UTF-8 却让 sqlx 连接串解析失败，当前受信启动器给的路径走不到；该出口与
另两处共用 `reported()`，由 `non_utf8_store_path_reports_store_open_failed_once` 覆盖同一条
统一出口。Windows 上三条 `#[cfg(unix)]` 用例不编译（与 C.10 的既有边界一致）。

#### C.12 C-1 终态（2026-11-12）：明文库 + 开关 on → `StorePlaintextRequiresMigration`

> 触发：C.7 把 C-1 排在迁移器/连接器之后，理由是「连接器还不注入 key，现在拦明文
> 等于把『静默明文』换成『静默起不来』」。这一批把它做成**默认 off 的显式开关**：
> 开关缺席时就是 C 阶段原样，开与不开是发布决策而不是代码默认。
> 改动落在工作区、**未 commit**；门禁同 C.11。

**契约**

| 项 | 值 |
|---|---|
| 开关 | `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED`（engine 进程 env） |
| 取值 | `on` / `1` / `true` / `yes`（去空白、大小写不敏感）→ 严格；`off` / `0` / `false` / `no` / 空串 / 缺席 → 兼容（**默认**）；其它值 → **off** + 一条滚动日志 WARN（认不出**不**回落到 on：默认 off 是硬约束，笔误不该把一批账号全打成终态） |
| 触发条件 | 开关 on **且** `store.db` 文件头 = `SQLite format 3\0`（`StoreFileState::Plaintext`）。**与有没有 key 无关**——无 key 的明文库放过去，等于「删掉 key 就能读明文」，开关形同虚设 |
| 新建路径（⑨，2026-11-12 补漏） | 开关 on **且** `store.db` **不存在**（`StoreFileState::Absent`）**且无 key** → `StoreKeyRequired` 终态。这一格原来落到矩阵 a，会在 `create_if_missing` 里现造一个**新的明文库**：比 ⑤（存量明文库）更糟——存量至少是迁移源，这一格是开关打开之后才开始的明文生产。语义取本码而不取 `PlaintextRequiresMigration`：盘上没有任何东西要迁移（连账号目录都没建），缺的是 key；没有 key 就建不出密文库，而开关不许降级建明文库 |
| 新建路径的对照 | 开关 on + 库不存在 + **有 key** = 照常建密文库（矩阵 b / 九格 ⑩）；开关 off + 库不存在 + 无 key = 明文创建逐字节不变（矩阵 a / 九格 ⑪） |
| 结局 | `StoreOpenError::PlaintextRequiresMigration`：`is_terminal()==true`，不进退避/网络重连；证书维护任务同判停手（`CertTaskExit::StoreTerminal`） |
| 固定 stderr 行 | `kt-signal-engine: ERROR event=store_open_failed code=StorePlaintextRequiresMigration retryable=false`（与 C 阶段六码**逐字同形**，每进程每码一条；新码独占一个一次性闸，不与别的码共用槽） |
| 副作用 | 无：不打开、不验真、不建目录、不建库、不写一个字节、**不删明文库**（迁移器要拿它当源） |

**顺序（`service::open_store_with_requirement`）**：能力门 → key 形状 → 文件分类 →
**C-1 终态闸（3.5 明文格 + 3.6 新建格）** → 密文验真 → 建目录 → 打开并跑迁移。
两个闸都卡在分类之后、一切写动作之前；因此「不写盘」是结构事实，不是靠约定。

**九格矩阵**（任务口径写「六格」，按 2（开关）× 2（明文/密文）× 2（有/无 key）
展开实为 8 格；2026-11-12 **补漏**补上第 ⑨ 格「`store.db` 不存在 + 无 key」——
八格只覆盖「库已存在」的两种形态，把新建路径整个漏在矩阵 a 里；九格全测，逐格
用例名即证据。另加 ⑩/⑪ 两格对照，把新建路径的三种「开关 × key」组合钉死）

| # | 开关 | store.db | key | 结局 | 断言 |
|---|---|---|---|---|---|
| ① | off | 明文 | 无 | 明文打开、存量可读（C 阶段原样） | `switch_off_plaintext_without_key_still_opens` |
| ② | off | 明文 | 有 | legacy 打开（不设 key）+ warn 恰一次（矩阵 c 原样） | `switch_off_plaintext_with_key_still_opens_legacy` |
| ③ | off | 密文 | 无 | `StoreKeyRequired` 终态（原样） | `switch_off_encrypted_without_key_still_reports_key_required` |
| ④ | off | 密文 | 有 | 正常打开（原样） | `switch_off_encrypted_with_key_still_opens` |
| ⑤ | **on** | 明文 | **无** | `StorePlaintextRequiresMigration` 终态 | `switch_on_plaintext_without_key_is_plaintext_requires_migration` |
| ⑥ | **on** | 明文 | 有 | 同一终态，且**先于** legacy warn | `switch_on_plaintext_with_key_is_plaintext_requires_migration` |
| ⑦ | on | 密文 | 无 | `StoreKeyRequired`（开关只改了明文那一格） | `switch_on_encrypted_without_key_still_reports_key_required` |
| ⑧ | on | 密文 | 有 | 正常打开（迁移完成后的现实形态） | `switch_on_encrypted_with_key_still_opens` |
| ⑨ | **on** | **不存在** | **无** | `StoreKeyRequired` 终态：**不建目录、不建库、不写一个字节**（放它过去就是开关 on 还在造新的明文库） | `switch_on_absent_without_key_refuses_before_creating_plaintext` + `switch_on_absent_without_key_reports_once_per_process` |
| ⑩ | on | 不存在 | 有 | 照常建**密文**库、可打开（新闸只拦无 key，否则开关一开新账号永远建不出来） | `switch_on_absent_with_key_still_creates_encrypted_store` |
| ⑪ | off | 不存在 | 无 | 明文创建，矩阵 a 逐字节不变 | `switch_off_absent_without_key_still_creates_plaintext_store` |

补充五条：`require_encrypted_switch_parses_only_explicit_words`（解析面）、
`switch_on_classifies_plaintext_by_header_before_opening`（顺序面：明文头 + 垃圾内容
先报「要迁移」而不是 `StoreCorrupt`——头是权威，与迁移器同口径）、
`switch_on_plaintext_is_terminal_and_reports_once_per_process`（**真进程**：env 进
进程，stderr 恰一条固定行）、`switch_on_absent_without_key_refuses_before_creating_plaintext`
（⑨：终态码 + 零落盘 + 固定行形状 + 与 `PlaintextRequiresMigration` 不混码）、
`switch_on_absent_without_key_reports_once_per_process`（⑨ 的**真进程**：env 开关，`code=StoreKeyRequired retryable=false` 恰一条，两次调用闸不增）。

**off 的「逐字节不变」怎么证明**

1. **代码面**：off 时 `require_encrypted.is_on()` 为假，函数余下部分与 C 阶段一致——
   `match (key, state)` 的四个分支没改写、没多一次 stat/打开，也没把开关塞进决策表；
2. **行为面**：九格里 off 的四格（①②③④）与矩阵 a–e 同值——含新建路径的 ⑪
   （off + 无库 + 无 key 仍造明文库，新闸未生效的那一面）；C.4 的既有 17 条断言（含
   `legacy_warn_is_emitted_by_the_open_not_by_a_manual_gate`）原样全绿——它们跑的是
   `open_store_with`（进程 env 未设置 = off）；
3. **磁盘面**：on 的两格**逐字节比对**整份 `store.db` + 目录条目集合
   （`assert_plaintext_refused`）：拒绝路径零写入、零新建（含 `-wal`/`-shm`）、零删除。
   off 格不断言字节相等——它真的会打开库（WAL/迁移会动头），这正是「off = 老行为」的反面证据。

**与 storeMigrate（D3 orchestration）的关系**

- **互不替换**：`storeMigrate` 自己按文件头分流，不做「能不能打开明文」的判断；
  引擎的开关只决定「服务不服务明文」，**不迁移任何东西**。
- **推荐顺序**：① connector `KT_ENGINE_STORE_MIGRATE=dry-run` 出计划 → ② `apply`
  逐槽「停槽 → 迁移 → 起引擎」（D3）→ ③ 该账号 `storeMigrate --json` 报
  `code=store_migrated`（或幂等 `store_migration_noop`）、文件头已非明文 →
  ④ 再对这批账号打开 `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED=on`。apply 成功的直接后果
  就是「库已是密文」，所以开关对已迁移账号无感（九格 ⑧）；新账号只要带 key 也照常
  建密文库（九格 ⑩），开关不会把「新号入网」这条路堵死。
- **开关 on 期间被拒的账号怎么恢复**：跑一次正向迁移（拒绝路径**绝不删明文库**，正是
  为了留这个源），迁移成功、重启引擎即恢复。把开关关回去也能恢复，但那等于显式
  回退到「服务明文」，发版口径要写清。
- **默认 off 的当时理由（这条前提已被段尾 supersede 注推翻，原文保留作记录）**：connector 当时仍
  `env_remove` 掉 `KT_SIGNAL_STORE_KEY`（C.7-2）。此时开开关 = 把所有明文账号打成终态（九格 ⑤），**且新账号建不出来**
  （九格 ⑨：开关 on + 无 key + 库不存在 = `StoreKeyRequired` 终态）——所以开关是
  **按批打开**的运维动作，不是发行版默认。
- **与反向迁移（D2 `--direction to-plaintext`）的组合**：某账号被反向成明文后，若
  所在引擎开着开关，它会立即终态。反向迁移的用途是「撤出加密」，配套动作是把开关
  一起关掉；两个方向别混着开。

**变异验证（本机实跑，全部回滚，文件 sha256 与变异前逐字节一致）**

```text
变异 A：删掉 open_store_with_requirement 里的 C-1 闸（三行）
        → 3 failed（709 filtered）：
          switch_on_plaintext_without_key_is_plaintext_requires_migration
          switch_on_plaintext_with_key_is_plaintext_requires_migration
          switch_on_plaintext_is_terminal_and_reports_once_per_process
          （前两条：明文照旧打开，"开关 on 必须拒绝明文库"；
            第三条：子进程里 expect_err 直接 panic）
变异 B：is_terminal() 改成 !matches!(self, Self::Failed | Self::PlaintextRequiresMigration)
        → switch_on_plaintext 三条 FAILED（"终态不得退化成退避/网络重试" + 固定行
          retryable 变 true）；certificate_tick_terminal_store_open_stops_the_task FAILED
变异 C：slot() 的 Self::PlaintextRequiresMigration => &PLAINTEXT_REQUIRES_MIGRATION
        改成 => &KEY_REQUIRED
        → each_store_open_code_emits_its_own_line FAILED（stderr 只剩两行；left 2 / right 3）
变异 D（**C-1 补漏**，2026-11-12）：删掉 open_store_with_requirement 的 3.6 段（C-1 新建闸，13 行）
        → 2 failed / 1 passed（-- switch_on_absent 筛选）：
          switch_on_absent_without_key_refuses_before_creating_plaintext
            （"开关 on + 无 key + 库不存在必须终态"：新库照旧被 create_if_missing 造出来，
               Ok(SqliteStore { … }) 直接 panic）
          switch_on_absent_without_key_reports_once_per_process
            （子进程里 expect_err panic → 父用例一条固定行都收不到）
          passed：switch_on_absent_with_key_still_creates_encrypted_store（⑩ 不该被删闸影响）
变异 E：3.6 段的返回码 KeyRequired 改成 PlaintextRequiresMigration
        → 2 failed（同两条）：left: KeyRequired / right: PlaintextRequiresMigration；
          真进程那条 stderr 里的 code 也跟着变——「⑨ 选 KeyRequired 而不是
          PlaintextRequiresMigration」是断言钉死的语义选择，不是随手写的
变异 F：3.6 段的条件去掉 require_encrypted.is_on()（变成恒拦）
        → switch_off_absent_without_key_still_creates_plaintext_store FAILED
          （"off + 无 key + 无库仍是兼容的明文创建（矩阵 a）"：拿到 KeyRequired）
        ——off 的兼容面同样被 ⑪ 钉住，闸不能悄悄扩大
```

三次变异（D/E/F）都在 `src/service/mod.rs` 上做、用 `cp` 备份 + `trap` 还原，
复原后 sha256 与变异前**逐字节一致**（`e7868703…a21cd974`）。

**门禁实跑（本机 macOS aarch64 / rustc 1.99.0）**

```text
cargo fmt --all -- --check                       → 本人文件域 0 diff
                                                   （rustfmt --check src/service/mod.rs src/store_open.rs
                                                    src/store_encryption_tests.rs → MINE_CLEAN）；
                                                   仓库级 --check 的剩余 diff 全在 src/store_migrate.rs
                                                   与 tests/store_migrate.rs（d2-fixer 的文件域，本批未触碰）
cargo clippy --all-targets -- -D warnings        → 0（零 warning）
cargo test --bin kt-signal-engine -- store_encryption_tests --test-threads=1
                                                 → 32 passed / 5 ignored（C-1 初版是 28/4：+3 九格、+1 child；
                                                   C.4 的 17 条是 C 批清单，本批新增的 4 条见上面的矩阵）
cargo test --all-targets -- --test-threads=1     → 0：bin 714 passed / 6 ignored、
                                                   tests/contract.rs 95 passed、tests/store_migrate.rs 22 passed、
                                                   backup5-spike 0 条
```

> 补漏期间的一次异域红：`store_migrate::tests::reverse_terminal_key_failures_leave_the_account_dir_byte_identical`
> 在 23:32 前失败过（reverse 终态路径多出 `store.db-wal`），那是 d2-fixer 正在改
> `src/store_migrate.rs` 的中间态；其文件 23:32:18 更新后同一条用例复跑通过（本批代码未动）。
> 归属证据：`src/store_migrate.rs` 里 `open_store_with` / `require_encrypted` 只在注释里出现
> 一次，迁移器不走本批改动的开关路径。

**已知缺口与边界（诚实登记）**

1. 开关是**逐进程 env**，引擎不落「已迁移」事实；库的状态永远以文件头为准（与迁移器
   共用同一权威）。
2. 开关 on 时「明文头 + 内容垃圾」先报 `StorePlaintextRequiresMigration` 而不是
   `StoreCorrupt`（头优先，与迁移器同序）。两条都是终态；运维跑一次迁移器就会拿到
   真正的损坏码（`store_corrupt` / exit 11）。
3. `docs/jsonrpc-compat.md` §1 的「五类终态」在开关打开时已是**六类**；本批文件域
   不含该文件，**未改**，登记给下一批连同码表一起对齐。→ **已对齐**（2026-11-11 doc 纠偏批：
   §1 的 store 打开码表补上 `StorePlaintextRequiresMigration`、终态五类改六类，并新增
   `KT_SIGNAL_STORE_REQUIRE_ENCRYPTED` 条目）。
4. connector 侧还没有独立消费这个新码（目前按 `retryable=false` 的通用终态桶处理）；
   给它单开修复桶属 connector 的批次。
5. Windows 面同 C.10 的既有边界：本批新增用例不依赖权限位/符号环，`#[cfg(unix)]`
   之外同样编译与运行。

**supersede（2026-11-11 跨仓事实；只覆盖本节里被推翻的那一条前提，不改本批记录）**：下面「与 storeMigrate 的关系」里
"今天为什么必须默认 off"的前提——connector 仍 `env_remove` 掉 `KT_SIGNAL_STORE_KEY`——**已经不成立**：
connector 侧已按账号把该账号派生的 store key 注入每个池槽的引擎子进程（`kt-signal-connector` 分支
`fix/signal-runtime-state-authority`：`d87ff46` 按账号注入、`963cee6` 接上 store 迁移编排与槽锁存），
并显式剔除继承来的 dev/test 覆盖值。因此**「开关默认 off」现在是发布决策，不是技术阻塞**：引擎已有能力
服务密文库，`off` 只表示"还没按批打开"。⑨/⑤ 两格的风险（对未迁移账号打开开关 = 该账号停接收）不变，
所以"先 apply 迁移、再按批开开关"的顺序要求也不变。

### P2 — 明文迁移器 + 校验 + 回滚（5–8 人日）

改动：
- 新增离线迁移入口（推荐子命令 `storeMigrate`，或启动前置阶段；**不要**塞进 `open_store`，理由 §2.4）；
- 实现 §4.2 五步 + §4.3 校验矩阵 + 反向（加密→明文）迁移；
- `store.format` 标记 + 原子写；
- 保留策略：备份留 7 天，删除动作单独记账。

验收（每账号，灰度 1 → 3 → 全量）：

```bash
# 迁移前采样
du -b <DIR>/accounts/<lk>/store.db*
# 迁移
./kt-signal-engine storeMigrate --data-dir <DIR> --account <lk-*>     # 目标形状，待实现
# 迁移后：头、结构、迁移账本、语义
xxd -l 16 <DIR>/accounts/<lk>/store.db | grep -q 'SQLite format 3' && echo PLAINTEXT-BAD || echo ENCRYPTED
sqlcipher <DIR>/accounts/<lk>/store.db "PRAGMA key=\"x'<64hex>'\"; PRAGMA integrity_check; PRAGMA cipher_integrity_check; SELECT count(*) FROM _sqlx_migrations;"
# 回滚演练（在副本上做，别在真库上）
cp -a <DIR>/accounts/<lk> /tmp/rollback-drill && <把 store.plaintext.bak 换回去> && 起引擎验证收一条
```

DoD：pilot 账号行数/迁移账本/语义金样全一致；每个断点 kill 后可收敛；反向迁移后旧二进制能开。

### P3 — 灰度与文案（2–3 人日）

- 灰度：1 账号 → 1 组 → 全量；每档 soak 24h，看 M1–M6。
- 文案/错误码：`KEY_MISSING`/`STORE_KEY_REJECTED`/`PLAINTEXT_STORE_REQUIRES_KEY` 的用户可见说明 + 支持话术。
- 明确 scope 声明：只覆盖 `store.db`（不含媒体、不含明文导出、不含 connector 库）。

### P4 — 清理（1–2 人日）

- 删明文备份、清 sidecar、固化 `store.format`；
- 复测 M3/M4/M6 并写回基线；
- 更新 `docs/`（新增一页"引擎静态加密下的事实与边界"）+ ADR（这次要以 ADR 形式拍板，别只留代码）。

### 总排期

| 阶段 | 人日 | 阻塞门 |
|---|---|---|
| P0 | 1–2 | G-B（Windows 构建） |
| P1 | 3–5 | G-C（fail-closed 可区分）、G-A（口径） |
| P2 | 5–8 | P1 |
| P3 | 2–3 | P2 |
| P4 | 1–2 | P3 |
| **合计** | **12–20** | 不含合规等待与灰度观察窗 |

---

## 8. 问题 7：明确结论

### 结论：**做**。但按门禁推进，不是"一把梭"。

**立刻做（P0+P1）**：这两阶段不改任何用户数据的格式，只把能力与失败语义就绪。
即使后面因为合规原因不迁移现有库，P0+P1 也是净收益（有 `cipher_version` 断言、有 KEY_MISSING 语义、
有可区分的失败码、有 store_open 指标）。

**P2 迁移放到下一个 release 窗口**，且必须满足三个前置门：

| 门 | 内容 | 不过的后果 | 谁拍板 |
|---|---|---|---|
| **G-A** | **已拍板（2026-11-11）**：接受「KEK 丢 = 本机历史不可读」；**不承诺**「重链再导」（零取证，ADR 0005:137 与 parity matrix:3425 互相矛盾）；UX 口径固定为「重新配对，历史可能由手机再导入，取决于手机与备份设置」；明文残留不做安全擦除承诺。依据：§6.1.1 + `docs/plan/link-history-transfer-status.md` | —（已过） | 产品 + 安全 |
| **G-B** | Windows `crt-static` + vendored OpenSSL 构建与**运行**通过（P0 验收命令全绿） | 停在 P0；不要为 Windows 单独降级成明文（会出现同一发布里两套格式） | 工程 |
| **G-C** | fail-closed 可区分：KEK 缺失/错误/库损坏三者可分辨，且**不会**退化成"网络故障重试"或"新建明文库"。**引擎侧已具备**（C 阶段四码 + `StreamOutcome::StoreTerminal` 终态，见 §7「C 阶段」）；connector 侧消费（把固定 stderr code 落到 `key_missing`/`data_corrupt` 修复桶）待接 | 停在 P1 | 工程 + QA |

### 现在"不做"的部分

- 不做 ADR 0006 的 connector 拆库/per-account key（那是另一个 53–74 人日的项目，见该文 §6.1）。
- 不做媒体附件加密（`attachments/` 是独立域，M6 只在 scope 声明里写清）。
- 不做 `PRAGMA rekey` 原地轮换。
- 不做"加密后再宣称历史明文已擦除"。

### 一句话给拍板人

> 引擎库现在是**明文**，里面是**身份私钥和会话密钥**；保护的只有 connector 打的 0700 目录。
> 换 SQLCipher 的技术路径是通的（vendor 里 API 都在，只差一个 feature 和一条 key 通道），
> **真正要谨慎的不是加密，是"key 丢了就永久读不了"这件事和 Windows 构建**。
> 建议：P0+P1 现在做（12 天内可交付能力与语义），P2 迁移等 G-A 口径明确后的 release 窗口。

---

## 9. 证据索引

**kt-signal-engine（本仓，工作区）**

- `Cargo.toml:13`（sqlx 0.9）、`:19-25`（presage 系列 rev 钉定 + `default-features=false`）、`:33-35`（`libsqlite3-sys/bundled`）、`:53,100-103`（rand/aes-gcm/hkdf/sha2 已在图）、`:166-168`（`[patch]` 指向 vendor）
- `vendor/presage/presage-store-sqlite/Cargo.toml:8`（默认 SQLCipher）、`:34`（`libsqlite3-sys` 空 feature）、`:40-51`（独立 workspace）
- `vendor/presage/presage-store-sqlite/src/lib.rs:41-47`（`open`）、`:49-65`（`open_with_passphrase` → `pragma("key")`）、`:67-92`（WAL/Full/busy_timeout/池 2/migrate）
- `src/service/mod.rs:14668-14688`（cache_size 与 `store_connect_options`）、`:14700-14735`（`open_store` → `open_store_gated`，门在打开之前）、`:14705-14720`（未注册库探测）、`:14728-14748`（`remove_link_store`）、`:14772-14780`（`store.db` 存在性）、`:15205`、`:9116`（90s）、`:9179-9191`（store 打不开→Recoverable）、`:8472-8483`（`StreamOutcome`）、`:3598-3601`（perf 快照）、`:29583-29587`（cache_size 单测）
- `src/state.rs:47`（`STORE_DB_FILE`）、`:84-96`（注册表 fail-closed）、`:302-341`（`lk-*` 扫描/`store_db_present`）、`:7`（fake 账号无 store）
- `src/main.rs:95-126`（严格 argv）、`:133,428`（env 读取先例）、`:444`（`ERROR event=…` 形状）、`:472-476`（启动先扫库再进主循环）
- `src/service/perf_metrics.rs:33-60`（封闭词表）、`:258-287`（快照）
- `src/methods.rs:4820`（`engine.perfMetrics` wire 面）
- `src/bin/backup5-spike.rs:10-29`（开发 bin 的编译期门 + `#[path]` 引入同一份运行时门）、`:59-69`（门在 `SqliteStore::open` 之前；临时目录、无 key）
- `tests/contract.rs:33-49`（真进程 harness）、`:173-177`（伪 `store.db` 字节）、`:240-260`（注册表损坏保留 store）
- `adr/0004-p1b-link-flow-and-identity-trust.md:39-42`（`accounts/<lk-hex>` 布局与删除语义）
- `vendor/presage/presage-store-sqlite/migrations/20250112201436_init.sql`（表清单：identities/sessions/pre_keys/…）

**kt-signal-connector（交叉引用）**

- `src/auth.rs:19-21`（两行 129B payload）、`:140-165`（解析 + `StoreKey::from_hex`）
- `src/store.rs:59`（`connector.sqlite3`）、`:98`（`KT_SIGNAL_STORE_KEY`）、`:608-641`（`StoreKey`/`raw_key_spec`）、`:647-656`（env 覆盖）、`:2383-2400`（读池 `PRAGMA key`）、`:2444-2467`（`Store::open` 决策表）、`:2474`（`secure_delete=ON`）、`:11696-11717`（`db_file_state` 头判定）、`:11720-11750`（`open_encrypted` + 强制首页读）、`:11752-11820`（明文→SQLCipher 迁移五步）
- `src/engine.rs:2695-2697`（`.env_remove(STORE_KEY_ENV)`，故意不为引擎提供 key）、`:7929-7955`（data-dir 0700 + 属主校验）、`:272-278`（`rss_bytes`）
- `src/main.rs:430-436`（store key 解析）、`:444-446`（Windows 目录 ACL 加固）

**依赖源码（本机 registry，供复核）**

- `libsqlite3-sys-0.36.0/build.rs:58-84`（feature 选择与 legacy `sqlcipher` 覆盖）、`:147-152`（`SQLITE_HAS_CODEC`/`SQLITE_TEMP_STORE=2`）、`:171-173`（Windows 缺 OpenSSL 直接 panic）、`:209-227`（vendored-openssl / Apple CommonCrypto 分支）
- `libsqlite3-sys-0.36.0/sqlcipher/sqlite3.c:108426-108457`（SQLCipher 4.10.0 community / 默认 flags）、`:108441`（`PBKDF2_ITER 256000`）、`:109457-109475`（reserve 计算）、`:113396-113410`（HMAC 摘要长度）、`:14374-14376`（默认页 4096）
- `sqlx-sqlite-0.9.0/src/options/mod.rs:114-194`（pragma 固定顺序，`key` 第 0 位）、`:358-365`（`pragma()`）、`src/options/connect.rs:38,69-80`（连接后执行 pragma 串）
- `sqlx-core-0.9.0/src/pool/inner.rs:26`（`connect_options` 常驻）、`src/pool/options.rs:161-162`（idle 10min / max_lifetime 30min）

**kt-desktop（先例）**

- `.worktrees/signal-security/electron/main/signal/storeKey.ts:6-33`（保管链/两条 leg/迁移记录）、`:65-69`（反静默重建：有库无 key 直接抛）、`:352`（fail closed 保现场）

---

## 10. 未核实项（别当结论用）

1. **未跑构建**：Windows `crt-static` + vendored OpenSSL 是否能一遍过、增量多久，都是推断（G-B 就是要把它变实测）。
2. **未实测性能**：+1.95% 体积是从 amalgamation 常量推的；RSS 增量、`TEMP_STORE=2` 对真实库的影响都没测。
3. **未核实 `store-key.v1` 的时序**：引擎在 `startLink` 时创建 `lk-*` 目录并首开库（`src/service/mod.rs:14687`），
   包裹文件由引擎写是可行的；但"链接失败/切号/重链"时的清理顺序（`remove_link_store` 删目录会不会漏删包裹）
   需要在实现时逐条对。
4. **未核实 Windows 上文件锁的迁移窗口**：connector 停引擎到文件句柄释放的时序（`kill_on_drop`、强制 kill 路径）
   可能让 `rename` 撞 sharing violation，需要在 Windows 上实测。
5. **未核实 SQLCipher 与 `sqlx::migrate!()` 在加密库上的行为差异**：理论上与明文无关（记录都在库里），
   但没有实测过加密库上的迁移重入。
6. **ADR 0006 的 `file:line` 已过期**：该文引用的 `store.rs:24-25/474-481/1766-1780` 在当前 checkout（`d491396`）
   已漂移到 `:59/608-641/2444-2467`。引用它时按本文索引，别照抄旧行号。

#### D1 状态（2026-11-11，正向迁移器已落地，未 commit）

> 触发：D 拆两批（**D1 正向** / D2 反向），本批只做正向。设计稿：
> `docs/plan/engine-store-migration-design.md`（§2 命令面、§3 五步、§4 校验、§5 幂等、§9 失败矩阵、§10 验收）。

| 项 | 落地 | 证据 |
|---|---|---|
| 命令面 | 同一二进制的新 argv 模式 `--data-dir <DIR> storeMigrate [--account <lk-hex>] [--direction to-encrypted] [--dry-run] [--json]`，与 `jsonRpc` 互斥、未知参数仍 exit 2 | `src/main.rs`（`store_migrate::route` 分流）、`src/store_migrate.rs` `route()`/`is_account_name()`；`tests/store_migrate.rs::cli_usage_errors_exit_2` |
| 退出码 | 0 成功/noop/干跑、2 用法、10 可重试（空间、占用、瞬时 IO、并发持锁、**权限/只读 IO 读失败**——裁决 #14 与 C 同口径）、11 终态（缺 key、错 key、指纹不符、源库损坏、校验不过、**主库状态违规 `store_state_invalid`**、**data-dir 不存在**、账号消失、缺能力） | `src/store_migrate.rs` `EXIT_OK`/`EXIT_USAGE`/`EXIT_RETRY`/`EXIT_TERMINAL` / `MigrateReport::exit_code()` / `Failure::io()`；`permission_failures_are_retryable_not_terminal` |
| 能力门 | `store_cipher::ensure_cipher_capable()` 是 `entry()` 的第一个动作，**早于 `logfile::init`**（连 `logs/` 都不建） | `entry()` |
| 五步 | 残留清理 → `wal_checkpoint(TRUNCATE)`（读回 busy，偏离 D-2）→ `VACUUM INTO` 备份 + fsync → `ATTACH … KEY ?`（绑定参数）+ `sqlcipher_export` + 补 `user_version`/`application_id` → 校验矩阵 → **swap 前再 `wal_checkpoint(TRUNCATE)` 源库（busy≠0 或 WAL 非空 → 10，绝不删活帧）** → 删 sidecar + `rename` + fsync + marker | `migrate_plaintext()` / `attach_export()` / `checkpoint_source_quiet()` |
| 幂等 | **文件头是唯一权威**（复用 `store_open::classify`）；`store.format` 只作审计与指纹；B4 窗口（rename 后无 marker）自动补写、B5 的 `*.tmp` 一律清；no-op 前先过 §5.5 健康闸（损坏 → `store_state_invalid`，不静默绿灯） | `migrate_account()` / `settle_encrypted()`；`second_run_is_a_byte_identical_noop`（单测 + 真进程各一条）、`corrupt_encrypted_store_is_state_invalid_not_noop` |
| 校验矩阵 | 头非明文 / 无 key 打不开 / keyed 验真 / `integrity_check` / `foreign_key_check` / `cipher_integrity_check` / schema 集合 / 逐表行数 / `thread_messages` 抽样 / `_sqlx_migrations` 逐行 / `sqlite_sequence` / `user_version` / `application_id` / presage 语义 probe（**源侧只读取注册事实，要求 staging `Some` 且 ACI/设备名逐项相等**） | `verify_staging()` / `collect_stats()` / `read_registration_facts()` / `semantic_probe()`；`migrates_plaintext_store_and_verifies_the_matrix`、`semantic_probe_requires_matching_registration` |
| 已迁移库的健康闸 | no-op 分支不只 probe 首页：`integrity_check` + `cipher_integrity_check` 不过 → 终态 `store_state_invalid`（保留现场、不 prune 备份） | `settle_encrypted()` / `verify_encrypted_integrity()`；`corrupt_encrypted_store_is_state_invalid_not_noop`、`cli_corrupt_encrypted_store_is_state_invalid` |
| 干跑 | `--dry-run` 只出计划：`action=dry-run` / `code=store_migration_planned` / `backup=null` / `did=[]`，零写入 | `migrate_account()`；`dry_run_plans_without_writing`、`cli_dry_run_plans_without_writing` |
| 崩溃可续 | 五个确定性断点（`after_backup`/`after_export`/`after_sidecar`/`after_rename`/`before_marker`，**仅 debug 构建**）+「先删 staging 再重做」；另有两条注入钩子 `corrupt_staging_rows`（验失败清理）与 `before_swap`（验 pre-swap checkpoint） | `fault_point()` / `fault_mutate_staging()` / `fault_inject_foreign_writer()`；`cli_fault_points_converge`、`cli_verify_failure_discards_staging_and_keeps_source_and_backup`、`cli_pre_swap_checkpoint_refuses_to_drop_live_wal_frames` |
| 零写入 | 缺 key / 形状非法 / 空间不足 / 库被占用四条路径都不写库、不建备份与 staging | `missing_key_writes_nothing`、`invalid_key_shape_writes_nothing`、`insufficient_space_is_retryable_and_writes_nothing`、`busy_checkpoint_is_retryable_and_touches_nothing`、`cli_missing_key_exits_11_and_writes_nothing` |
| 证据行 | 每账号一条 WARN，字段 `code/stage/account_tag/bytes/elapsed_ms/keyid`，无 key、无号码 | `emit_evidence()` |

门禁实跑（本机 macOS aarch64 / rustc 1.99.0，全部实跑）：

```text
cargo fmt --all -- --check                        → 0（无 diff）
cargo clippy --all-targets -- -D warnings         → 0（零 warning）
cargo test --all-targets -- --test-threads=1      → 0：bin 693 passed / 4 ignored（既有 ignore）、
                                                    tests/contract.rs 95 passed、tests/store_migrate.rs 14 passed
                                                    （2026-11-11 审计修复批复跑；修复批新增 6 条可失败用例 + 3 条裁决用例）
```

**与设计稿的偏差（4 条，均已实测）**：

1. **ATTACH 的建文件权限跟随主库连接**（SQLite ≥3.21）：D-1 的 `create_if_missing(false)` 会让 `ATTACH … AS encrypted` 拿不到 CREATE 权限（`SQLITE_CANTOPEN`）。落地改成「先由本进程建一个空 staging 文件，再交给 ATTACH + KEY」——空库正是 SQLCipher 生成随机盐的形态，D-1 的语义（不因路径打错而悄悄建源库）不变。
2. **`sqlx::query` 对裸 `String` 有注入审计门**：带动态表名与 `PRAGMA schema.x` 的两处改走 `QueryBuilder`（表名仍按 SQLite 标识符规则转义）。
3. **`-shm` 不算迁移写入**：SQLite 的连接共享内存每连接进出都会改写，「零写入」的判据取**库文件 + 备份 + staging**，不取 `-shm`（`busy_checkpoint_is_retryable_and_touches_nothing` 写明了这条边界）。
4. **VACUUM 之后多一道备份闸**：SQLite 内核在个别页布局上会让 `VACUUM INTO` 的 step 失败，而 sqlx 仍把 `execute` 报成 Ok（本机复现：系统 sqlite3 3.54 写出的 12B reserved 页布局 → 目标文件 0 字节 + worker `MISUSE`）。备份是唯一回滚物，所以落地加了 `ensure_plaintext_backup()`（非空 + 明文头，不过就删掉半个备份并 exit 10）。该边界的 stderr 里仍会看到 sqlx 那句 panic（sqlx 对 `SQLITE_MISUSE` 的选择），但迁移器已收敛为**可重试失败、原库不动、无残留**；生产库全部由 presage/SQLCipher 写出（reserved 0），不走这条边界。

**审计后修复批（D1-audit → task-3，2026-11-11）**：

| # | 问题 | 修法 | 可失败用例（回滚该修法即红） |
|---|---|---|---|
| 1 | `--dry-run` 谎报 `action=migrate` / `code=store_migrated` / 备份名 | 干跑专用 `action=dry-run` + `code=store_migration_planned`，`backup=null` | `dry_run_plans_without_writing`、`cli_dry_run_plans_without_writing` |
| 2 | argv 误吞：`--data-dir` 的**取值**被当成子命令 | `subcommand_position()` 跳过取值位置（`--data-dir`/`--receive-mode`/`--account`/`--direction`） | `argv_route_is_strict`（新增取值位置用例）、`cli_data_dir_named_store_migrate_still_starts_json_rpc` |
| 3 | data-dir 不存在也 `ok:true`/exit 0 | 终态 11 + `store_migrate_data_dir_missing`，且不代建目录 | `missing_data_dir_is_terminal_and_creates_nothing`、`cli_missing_data_dir_exits_11` |
| 4 | 语义校验空转（`Ok(None)` 也算通过，不比 ACI/设备名） | 源侧只读 `kv` 取注册事实，staging 必须 `Some` 且 ACI/设备名逐项相等 | `semantic_probe_requires_matching_registration`（漂移/丢行/多行三变体） |
| 5 | 加密库只 probe 首页：页 HMAC/结构坏了仍报 noop/exit 0 | no-op 分支加 `integrity_check` + `cipher_integrity_check`，不过报 `store_state_invalid` | `corrupt_encrypted_store_is_state_invalid_not_noop`、`cli_corrupt_encrypted_store_is_state_invalid` |
| 6 | verify 失败路径无注入用例 | debug 断点 `corrupt_staging_rows` 改 staging 行数：删 staging、留原库与备份、exit 11 | `cli_verify_failure_discards_staging_and_keeps_source_and_backup` |

两条裁决同步落地：#14 权限/只读 IO 读失败统一 exit 10（设计 D-15 改口径，`permission_failures_are_retryable_not_terminal`）；
#8 `--json` 的 `plan[]/did[]` 以设计 §10.2 为准改成**步骤名数组**（跨账号并集、规范顺序）；
#10 swap 前再 checkpoint 一次源库再删 sidecar（`checkpoint_source_quiet()`，活帧 → exit 10 且不删 WAL，`cli_pre_swap_checkpoint_refuses_to_drop_live_wal_frames`）。

**本批未做（明确留给下一批）**：

1. **反向迁移（`--direction to-plaintext`）**：`entry()` 以 exit 2 明确拒绝；往返一致性属 D2。
2. **C-1（明文 + 有 key → 终态 `StorePlaintextRequiresMigration`）**：本批不动 `open_store_with`（矩阵 c 与既有断言保持原样）。它与连接器注入 key（`src/engine.rs:2695-2697` 的 `env_remove`）必须同批做，否则只是把「静默明文」换成「静默起不来」。
3. **指标词表（§10.7 的 `timings.store_migrate_*` / `counters.store_migrate_*`）**：本批只落滚动日志证据行，`perf_metrics` 的封闭词表未扩（扩词表要同步漂移单测）。
4. **`KT_ENGINE_STORE_MIGRATE` 三档开关与连接器编排（§2.3）**：跨仓，走 connector 自己的流程。
5. **50 轮随机 kill 属性测试（§10.4）**：确定性 5 断点已覆盖收敛；随机杀循环与「无第三态」谓词留给 D2 一起做（那时才有往返谓词可断言）。

