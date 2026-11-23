# Presage PreKey 上游对照（2026-11-23）

## 版本事实

- 本地 `Cargo.toml` pin：`96f5e925c45b2d7a1c09afd0aee44733a2c3e671`。
- `whisperfish/presage` HEAD：`10d41fc421f33523a1d5a589812dcbaf0c399dcf`。
- 两者相差 28 个上游提交；HEAD 标题为 `Rework PreKey Management (#466)`。
- 本地通过 `[patch]` 使用 vendored Presage/libsignal-service，并含 KT 的接收、发送、
  传输与存储修复；切换 HEAD 不是修改一行 rev，而是跨依赖与数据库迁移项目。

## 上游 Rework 与本地覆盖

上游 Rework 主要新增 `pre_key_metadata`（last issued/active key id、rotation 时间）
及多次 SQL 迁移，并让 Manager 经 `PreKeysStore` 更新 ACI/PNI bundle。其
`check_pre_keys → rotate → recheck` 一致性块仍被注释，源码明确保留 TODO；因此
上游 HEAD 本身也不是该面的完成性证明。

本地 `src/service/prekey.rs` 已独立覆盖：ACI/PNI 双身份水位补发、30 分钟与 200
信封快速检查、signed/last-resort Kyber 36 小时轮换、官方保留窗清理、发送前判龄；
维护任务在 stream 作用域的受监控 `spawn_local` worker 中运行，不阻塞 receive loop。
本地使用记录时间戳与既有 KV 书签，不包含上游 `pre_key_metadata` 的持久 ID 面。

## 决策与升级门槛

本批不把 28 个上游提交和 SQL 迁移一次性混入消息/UI 修复，也不声称已同步 HEAD。
后续升级必须单独冻结 wire/schema 契约，提供旧 store 备份、迁移中断恢复、ACI/PNI
在役键不变、last-issued id 单调、重链/重启/接收/发送回归及真实双设备验收。
当前没有证据要求为修复现有 receive/prekey 症状而承担这次整包升级。

验证：`cargo test prekey`；完整 `cargo test`、clippy、release build。
