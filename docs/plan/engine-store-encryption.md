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
| 5 | 失败模式 | **密钥丢失 = 数据永久不可读**（本文 §6.1，必须写进产品文案与错误码）。次严重的是「半实施」：`PRAGMA key` 在纯 SQLite 上是**静默 no-op**，feature 没翻也不会报错 —— 测试全绿而库仍是明文。 |
| 6 | 分阶段 | P0 构建门禁（1–2 人日）→ P1 密钥通道 + fail-closed（3–5）→ P2 迁移器（5–8）→ P3 灰度与文案（2–3）→ P4 清理（1–2）。**合计 12–20 人日**，不含合规等待。 |
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
> 用户目录被清、profile key 轮换没重包）或 `store-key.v1` 丢/坏 ⇒ 该账号历史数据永久不可读，
> 且不可逆、不可恢复、不能靠重扫码救回历史。**

现在的代码对此**没有任何防线**（没有格式标记、没有 KEY_MISSING 语义、没有反静默重建闸）。
桌面已有可抄的先例：`storeKey.ts:65-69`（"store 存在但 key 丢了 → 抛，绝不新建"）、`:352`（fail closed 并保留现场）。

### 6.2 失败模式全表

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
| 3 | 运行时门（**前置**） | `src/store_cipher.rs`（`CipherGate` / `NegativeCache`）+ `src/service/mod.rs:14694-14744`（`open_store` → `open_store_gated`） | 顺序钉死为 **门 → 建目录 → 建 `store.db` → 打开并跑迁移**：`open_store_gated` 第一步 `gate.ensure()`，缺能力直接 `Err(())`，**不建目录、不建 `store.db`、不起连接池、不跑 `sqlx::migrate!()`**（这些正是 `open_with_options` 的副作用，vendor `presage-store-sqlite/src/lib.rs:67-92`）。判定结论**正负都缓存**：成功结论 `OnceLock`；失败进 `NegativeCache`（同样 `OnceLock`）→ 后续 open 只回同一错误，不重探 `:memory:`。失败证据行（`error!` + `eprintln!`，固定 `code=StoreCipherUnavailable`）由 `CipherGate::report_denial_once` 的 `compare_exchange` 闸控，**每进程只落一次**，重试不刷屏。走既有「store 打不开 → 未注册链接库保留 / 接收管线可重试」语义（`src/service/mod.rs:9183` 一带不变）。 |
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
| **G-A** | 合规/产品口径：谁需要"引擎库静态加密"；是否接受"KEK 丢 = 历史不可读"；是否接受"历史明文残留不可安全擦除" | 只做 P0+P1，存量不动，文档如实声明"连接器库已加密、引擎库未加密" | 产品 + 安全 |
| **G-B** | Windows `crt-static` + vendored OpenSSL 构建与**运行**通过（P0 验收命令全绿） | 停在 P0；不要为 Windows 单独降级成明文（会出现同一发布里两套格式） | 工程 |
| **G-C** | fail-closed 可区分：KEK 缺失/错误/库损坏三者可分辨，且**不会**退化成"网络故障重试"或"新建明文库" | 停在 P1 | 工程 + QA |

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
