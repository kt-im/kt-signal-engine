# 接收环 prekey 维护隔离契约

日期：2026-11-23 ｜ 状态：实施基线

## 背景与证据

真实客户端引擎日志出现 `receive_loop_gap` 30 秒以上长窗，并伴随
`prekey_maintenance_timeout` 与 `ws_frame_to_loop` 延迟。接收循环的时间维护、
时间快检和量触发快检都在信封分支体内直接等待 Manager 网络调用；30 秒预算
只限制单次等待，不消除这期间停止轮询 `stream.next()` 的结构性阻塞。

量触发另有计数错误：每封处理后都把 `quick_seen` 清零，而阈值是 200，导致
正常单封处理路径无法累计到触发线。

## 决策

1. `manager.receive_messages()` 建立独立 stream 后，Manager 由本线程异步互斥保护。
2. prekey 时间维护、时间快检、量快检统一投递给一个 `spawn_local` 串行 worker；
   收流循环只投递作业和更新计时簿，不等待协议网络调用。
3. 初始 sync 与 worker 共用同一 Manager 锁，继续保持 Manager 作业互斥。普通
   Content 投影使用 `SqliteStore::clone`（共享同一 sqlx pool 与 identity-change 账），
   不等 Manager 锁；DERM、verified 以外需要 Manager 的分支仍按原语义短时取锁。
4. 同类 prekey 作业在排队/执行期间合并；维护和快检使用不同位，互不误吞。
5. worker 句柄由流级 guard 持有，流退出即 abort，旧连接的维护不得跨重连存活；
   receive `select!` 同时监测 worker，panic/异常退出进入既有监督重连，不保留陈旧 bit。
   流关闭分类前先 abort worker，再取 Manager 锁读取 close code。
6. 量触发累计到 200 才清零并投递快检；作业在途时后续触发合并到同一作业。

## 不变量

- JSON-RPC wire、通知形状、信封 ACK 顺序和 connector 契约均不变。
- prekey 调用仍是 best-effort，保留原 30 秒超时和固定日志阶段词。
- 不把 Manager 移到多线程 runtime；非 Send 状态继续留在 `LocalSet`。
- 不为消除卡顿而复制 Manager 或并发写同一账号 store。

## 验收

- 单元测试：200 封累计边界、作业合并、guard drop 取消。
- 完整门禁：`cargo fmt --check`、`cargo test`、`cargo clippy --all-targets`、release build。
- 真机：维护窗口内持续收流，`receive_loop_gap` 不再与 30 秒维护预算同长；
  量触发日志可在 200 信封后出现，重连后旧 worker 不继续写账号 store。
