# 接收 Duplicate 恢复（决策 B）

状态：phase1 + phase2 均已落地（2026-11-10）
范围：只改 kt-signal-engine（`vendor/presage` + `src/`）
- phase1：停写 `kt_outbox`，重放改读 `thread_messages`（§1–§5）。
- phase2：DROP 表、删除 store 读面/引擎死代码/kt-events 旁路，兼容面收口
  一版（§6）。

## 1. 背景与决策

ADR 0004 引入了 presage 侧的 `kt_outbox`：`save_messages` 在同一事务里
再存一份 Content 拷贝，Duplicate 时按 `dup:{sender}:{ts}` 读回重建。实践
暴露三个问题：

1. `kt_outbox` 与 `thread_messages` 是同一份内容的两份拷贝，写入放大，
   且长期没有 prune（confirm 只在引擎内部收口时调用）。
2. key 是进程内拼接的 `sender:timestamp`，而引擎重放用的是
   `dup:sender:timestamp`（前缀不同），读面与写面天然对不上；按 key 查
   行这条链路本身不可靠。
3. `kt-events.db` 的 Duplicate recovery marker 只写不读：没有任何消费方，
   marker 只是持续增长的旁路库。

决策 B：接收恢复只依赖 presage 已经落库的 `thread_messages`，不再维护
第二份 outbox；`kt-events.db` marker 停写；表结构保留，DROP 留 phase2。

## 2. phase1 改动

### 2.1 vendor（presage-store-sqlite）

- `save_messages` 不再调用 `save_outbox_on_tx`，`kt_outbox` 零写入；
  `save_outbox_on_tx` 已删除。
- `save_message_on_tx` 补写 `thread_messages.server_ts`（来自
  `Metadata.server_timestamp`）。此前该列在接收路径恒不写，重建时只能
  回落到 `ts`，丢失信封服务器时刻。SQL 用运行期 `sqlx::query`（不走
  `query!` 宏），离线 `.sqlx` 缓存零改动。
- 新增 `SqliteStore::messages_by_sender_and_timestamp(sender, ts)`：按
  sender+client_ts 返回**全部** thread 行。presage 的重投判定是
  thread-scoped，同 sender+ts 落两个群是合法形态，单行 `fetch_optional`
  会静默丢一条。
- 新增 `kt_outbox_len()`：诊断/回归计数（表不再写入，phase2 随 DROP 删除）。
- `sent_message_by_timestamp` 与 `messages_by_sender_and_timestamp` 共用
  行解码：`recipient_id` 按 BLOB UUID 解（原 TEXT 假定在真实行上会解码
  失败）。
- `kt_outbox` 表、迁移、`kt_outbox_get/confirm` 读面全部保留（兼容；
  phase2 已 DROP 并删除读面，见 §6）。

### 2.2 engine

- Duplicate 接收臂不再写 `kt-events.db`（`duplicate_outbox().enqueue_duplicate`
  写点删除）。`event_store` 模块与表保留到 phase2（§6 已删）。
- `receive.confirm` 改为幂等 no-op：`kt_outbox` 已无行可 confirm，账号
  已知即返回 `true`，未知账号仍 `false`。wire 形状不变。
- Duplicate 重放改读 `thread_messages`：
  - 接收臂从信封内容推导 `Thread`（`Thread::try_from(&Content)`），按
    **thread + ts** 读行。远端删除会把行改写为 nil ACI 的
    `NullMessage` 墓碑，sender 已变，只有 thread+ts 找得到；两群同
    sender+ts 也只有 thread 能区分。
  - 无 thread 的 legacy 入口（`receive.replay` 的 `dedupeKey`）只把
    `dup:{sender}:{ts}` 解析成 sender+ts 查询参数，按 sender+ts 返回全部
    命中行；`dup:` 前缀不再是 store 行键。
  - 重建仍走 `SqlMessage -> Content`，保留 `pni_verified`、
    `server_ts`、`sender_device`、`destination`、`was_plaintext`、
    `needs_receipt`、`unidentified_sender`。
- 读面结局三分类（2026-11-10 终审修复）：`replay_duplicate` /
  `replay_duplicate_in_thread` 返回 `Result<Option<_>>`。`Ok(Some)` =
  投影；`Ok(None)` 仅当 store 查询成功且**确认无行** →
  `DuplicateReplayOutcome::Unavailable` → 计 `duplicate_replay_unavailable`
  → **仍等 flush 栅栏** → ack（止血 b，避免服务器重投风暴）。
  `Err(DuplicateReplayError)`（store 打不开/查询或解码失败/账号不可解析/
  key 不合法）→ 计 `duplicate_replay_failed` + error 日志 →
  `DuplicateReplayOutcome::Failed` → **扣留 ack**（行状态未知，不能当
  「无行」吞掉重投信封）。spawn 失败/panic/abort 同 Failed。
- `reconstruct_outbox_content` 保留但标记 phase1 兼容（仅回归测试用；
  phase2 已删，见 §6）。

## 3. 语义对照

| 场景 | phase1 行为 |
|---|---|
| 正常重投 | thread+ts 读回原行，重建 Content，投影后 flush → ack |
| 同 sender+ts 两个群 | 两行都在；thread 精确重放各回各行；legacy key 返回两行 |
| 远端删除后重投原消息 | thread+ts 命中墓碑行，重建为 `NullMessage`，投影零消息，不复活正文 |
| 重投同一信封多次 | `INSERT OR REPLACE` 单行；恢复拷贝表已 DROP（phase2） |
| store 查询成功且确认无行（`Ok(None)`） | `Unavailable` → flush → ack（计数 + warn） |
| store 打不开/查询或解码失败（`Err`） | `Failed` → 扣留 ack（计数 + error），信封留服务器队列 |
| replay spawn/panic/abort | `Failed` → 扣留 ack，信封留服务器队列 |

## 4. 兼容面（phase1 保留，phase2 收口）

phase1 保留面与 phase2 处置：

- presage `kt_outbox` 表与迁移：phase2 新迁移 `DROP TABLE IF EXISTS
  kt_outbox`（`20261110120000`）；表自带的 dedupe_key UNIQUE 索引随表消失，
  从未有显式索引。
- `kt_outbox_get` / `kt_outbox_confirm` / `kt_outbox_len`：phase2 全部删除。
- `receive.confirm` / `receive.replay` 方法名与参数：不变，保留一版
  （connector 零调用已核对；见 §6）。
- `dup:{sender}:{ts}` key 形状：不变（只降级为查询参数）。
- `kt-events.db` 与 `event_store` 模块：phase2 删除模块与 `event_pool`；
  旧 `kt-events/` 文件成为无读写方的残留（见 §6）。
- `reconstruct_outbox_content`：phase2 删除。

## 5. 测试

engine（`cargo test --bin kt-signal-engine duplicate_`，phase1 面）：

- `duplicate_replay_rebuilds_both_groups_sharing_sender_and_ts`
- `duplicate_replay_after_remote_delete_is_null_message`
- `duplicate_redelivery_keeps_one_row`（重投单行；phase2 起不再断言 kt_outbox）
- `duplicate_key_miss_acks_after_flush`（store 确认无行 `Ok(None)` → Unavailable → flush → ack）
- `duplicate_replay_store_open_failure_withholds_ack`（store 打不开 →
  `Err(StoreOpen)` → Failed，不 flush、不 ack）
- `duplicate_replay_store_read_failure_withholds_ack`（store 可开但查询
  失败 → `Err(StoreRead)` → Failed，不 flush、不 ack）
- `duplicate_replay_success_projects_and_acks`（正常行 → Projected →
  flush → ack）
- `duplicate_key_roundtrips_and_rejects_garbage`（key 解析）

engine（phase2）：

- `upgrade_drops_kt_outbox_and_replay_still_reads_thread_messages`
  （phase1 schema + 一行 kt_outbox → 引擎 open_store 升级 → 表不存在；
  随后写入 thread_messages 行并按 thread+ts 重放成功）
- `receive_confirm_stays_an_idempotent_noop`（已知账号恒 true 且幂等、
  key 形状不参与判定、未知账号 false）
- `tests/contract.rs::receive_compat_surface_stays_a_noop`（wire 面：
  confirm 幂等、未知账号 false、参数缺失 -32603、replay 无行 -32603）

presage store（standalone）：

- `save_messages_persists_a_whole_window_and_the_outbox_table_is_gone`
- `two_group_messages_sharing_sender_and_ts_both_replay`
- `remote_deleted_message_replays_as_null_message`
- `redelivery_keeps_one_row_and_preserves_metadata`
- `fresh_store_never_exposes_kt_outbox`
- `upgrade_from_phase1_schema_drops_kt_outbox_and_keeps_data`
- `rollback_script_restores_the_pre_drop_schema`
- `drop_migration_is_idempotent_on_reopen`

## 6. phase2（已落地 2026-11-10）

1. **迁移**：新增 `vendor/presage/presage-store-sqlite/migrations/20261110120000_drop_kt_outbox.sql`
   （`DROP TABLE IF EXISTS kt_outbox`）。schema 版本沿用 presage 的 sqlx
   迁移机制（`_sqlx_migrations`），无独立版本常量：新库在同一条链里先建后
   DROP，旧库下次 open 时补跑该迁移，两者都收敛到「无表」。执行幂等
   （重复/已无表均为 no-op）。
2. **可回滚**：sqlx 拒绝「迁移表比二进制新」的库，所以二进制回退还要跑
   `vendor/presage/presage-store-sqlite/rollback/20261110120000_drop_kt_outbox_rollback.sql`
   （逐账号 store）：重建 phase1 表与 dedupe_key UNIQUE 索引，并删掉
   `_sqlx_migrations` 里该版本行。phase2 二进制下次 open 会再次 DROP。
3. **store 读面**：删除 `kt_outbox_get` / `kt_outbox_confirm` /
   `kt_outbox_len`。
4. **引擎死代码**：删除 `src/event_store.rs`、`mod event_store`、
   `Service::event_pool`、`reconstruct_outbox_content`；`outbox_model` 只保留
   `dup:{sender}:{ts}` 的拼装/解析（不再是行键）。旧 `kt-events/` 目录不再
   被任何代码读写，作为残留由运维按需清理（不自动删数据）。
5. **RPC 兼容面**：`receive.confirm` 保持幂等 no-op（已知账号 true、未知
   false、dedupe key 不参与判定）；`receive.replay` 保持从 thread_messages
   重建。二者 connector 零调用，保留一版供回滚窗口，下一版再评估下线。
6. **观测**：`duplicate_replay_unavailable` 口径 = 「store 查询成功且确认
   thread_messages 无行」；读面无法确认（open/查询/解码）另计
   `duplicate_replay_failed`，其余三键不变。

## 7. 验收命令

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build --release

# vendor store 自测（standalone workspace 需要 cdsi feature 才能编译
# libsignal-service，见 vendor/README.md 的 standalone 说明）
cargo test --manifest-path vendor/presage/presage-store-sqlite/Cargo.toml \
  --features presage/cdsi
```

phase2 门禁记录（2026-11-10）：

- `cargo fmt --all -- --check` + vendor `cargo fmt --check`：绿。
- `cargo clippy --all-targets -- -D warnings`：绿；vendor standalone
  `cargo clippy --features presage/cdsi --all-targets`：绿。
- vendor store 测试：16 passed（含 4 件 migration 回归）。
- engine bin 单测 614 passed（`--test-threads=1`，跳过既有 stack overflow
  用例）；契约测试 92 passed；本批新增 3 件全绿。`cargo build --release`
  通过。
- 一次在机器 I/O 峰值下的复跑出现成片 `pool timed out while waiting for an
  open connection`（store 类用例，5s acquire 超时），同二进制复跑即全绿，
  属环境噪声，非用例回归。
- 注意：本机并行跑全量 bin 用例偶发随机 hang（60s+ 无进展，涉及与本次
  改动无关的 store/投影用例；排除本批新增用例后仍复现），串行即全绿。
  `receive_loop_retries_unavailable_manager_until_shutdown` 为既有
  stack overflow 用例（compat §5 M4 批已登记）。门禁复跑建议
  `cargo test --all-targets -- --test-threads=1 --skip receive_loop_retries_unavailable_manager_until_shutdown`。

终审修复门禁记录（2026-11-10，duplicate replay 读失败不得当「无行」）：

- `cargo fmt --all -- --check`：绿。
- `cargo clippy --all-targets -- -D warnings`：绿。
- `RUST_MIN_STACK=8388608 cargo test --all-targets -- --test-threads=1
  --skip receive_loop_retries_unavailable_manager_until_shutdown`：
  bin 单测 617 passed（1 filtered）；契约 92 passed；0 failed。新增/改写的
  四件见 §5：open 失败、read 失败、确认无行、正常投影。
- 复跑提醒：首次全量跑在本机既有随机 hang 用例
  `sticker_pack_sync_projection_with_service_write_is_best_effort`（sqlite
  open 互斥等待，与本批改动无关）停住；同命令复跑即全绿。
