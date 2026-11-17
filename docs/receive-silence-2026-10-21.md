# 接收流静默死亡取证与根治（2026-10-21 03:28–03:36）

> 结论：`getConfiguration -32603` 不是接收停摆的原因。接收管线当时已经建流
> 成功并跑完登录期维护；停摆发生在建流之后，根因是 vendored WebSocket I/O
> pump 与接收循环里存在**无上限的等待**：任何一条同步/manifest/保活写请求
> 挂死，保活看门狗（与写请求同一个 pump）就永远不会再 tick，接收任务也永远
> 停在维护臂里。没有 close、没有 warn、没有流结束事件 → 接收静默死亡，只能
> 靠重启恢复。

## 1. 现场时间线（UTC；本地 +08:00）

| UTC | 本地 | 证据 | 含义 |
|---|---|---|---|
| 19:28:13 | 03:28:13 | Electron main log `signal.runtime.state stopping/stopped` | 旧引擎/连接器停止，准备换新二进制 |
| 19:29:02 | 03:29:02 | main log `starting_engine` → `ready` | 新引擎启动（pid 随后为 45246 系） |
| 19:29:08 | 03:29:08 | engine log `getConfiguration code=-32603` | 某个请求指向引擎未知账号/不可读 store；请求面独立失败 |
| 19:29:26 | 03:29:26 | engine log `profile refresh batch start/done skipped=5` | 登录期维护任务已 spawn **且** 收流已建立（该任务由建流后的 +2s/+10s 沉降臂拉起） |
| 19:29:29 | 03:29:29 | engine log `engine.whitelist-warmup ... filled` | 同一条收流建立链的最后一步完成 |
| 19:29:29–19:36:31 | 03:29–03:36 | engine log 空窗；无 `websocket closed`/stale/error | 收流不再产出任何事件；保活看门狗没有 tick 到日志分支 |
| 19:32 | 03:32 | 用户手机发消息 | 服务端入队；引擎没有消费 |
| 19:36:26 | 03:36:26 | connector log `engine process started pid=45246` | 重启 |
| 19:36:50+ | 03:36:50+ | profile refresh/whitelist warmup 再次出现，随后消息由队列重放 | 建流恢复 |

关键反证：`getConfiguration -32603`（19:29:08）**早于**收流建立与登录期维护
（19:29:26/29）。如果它是接收停摆的原因，收流不可能在它之后还完成建流和
维护批。因此它只是同一时段 connector 侧账号参数/解析的独立失败面。

另一条旁证：重启后引擎 store 中存在 `ts=2026-10-20T19:32:53Z` 的信封
（thread_messages rowid 58，与用户手机发送时刻吻合），且 19:36 前引擎日志
没有任何建流/close/重连行（旧二进制也无 `receive_stream_established` 证据
行）。这与
「消息留在服务器队列、重启后重放落库」一致，而不是「已收但投影丢失」：
后者会在重启重放时被 presage 判 Duplicate 就地 ack，用户永远收不到。

## 2. 代码路径：接收为什么能「活着但不动」

`receive_account_loop`（`src/service/mod.rs`）每轮：

1. `serve_account_stream` 开 store → `Manager::load_registered` →
   `register_device_capabilities` → `manager.receive_messages()`；
2. 建流成功后进入一个 `tokio::select!`：主臂 `stream.next()` 收信封，
   其余臂做登录期 sync、manifest 拉取、prekey 维护、DERM 等网络调用；
3. 这些维护臂在**臂体里直接 await**，而 manager 的 WebSocket 请求路径
   （`vendor/libsignal-service/src/websocket/mod.rs`）当时没有应答超时，
   `Sink::send` 也没有写超时；
4. 保活循环与业务请求共用同一个 `SignalWebSocketProcess::run` pump。一旦
   pump 卡在某次 `ws.send(...).await`，保活 tick 不会运行；服务器也没有
   收到 close；`MessagePipe` 的请求流不会结束；
5. 于是 `stream.next()` 永远 pending（或在维护臂里永不返回），
   `serve_account_stream` 既不会返回 `Connected` 也不会返回 `Recoverable`，
   外层重连环没有机会执行。

这不是「接收管线没起来」：日志里的 profile refresh / whitelist warmup 证明
它起来过。旧代码的失败可见性缺口在于：

- `Manager::load_registered` 失败、`receive_messages` 失败、能力注册失败
  全部静默返回 `Recoverable`（无日志）；
- keepalive 发送失败只落 **info**，且只 `break` 不 close；
- 单条 keepalive 无应答要等**下一次 30s tick** 才判；
- 应用请求无 30s 应答闸，挂起的请求可以把收流任务无限期留在维护臂。

## 3. 为什么 REST 仍然可用

JSON-RPC 请求在 `main.rs` 里每条 `spawn_local` 独立执行，账号解析与
`storage_context` 每次调用自建 store/Manager/HTTP；接收管线用启动期
`engine-state.json` 的账号和独立 Manager。二者只在进程级 WebSocket slot
上共享连接，不共享「请求是否完成」的状态。所以：

- 请求失败（如 `getConfiguration -32603`）不会拆接收流；
- 接收任务挂起时，LocalSet 是协作式调度，其他请求任务照常 yield/运行，
  REST 可以继续回答——这正是「REST 正常、接收没消息」的机制。

## 4. 修复（对齐官方）

| 要求 | 改动 |
|---|---|
| A 保活发送失败 = 死亡 | vendor `send_frame` 30s 写超时；失败/超时 → **warn** + 有界 close(3001) + 退出 pump（旧：info + 不 close） |
| B 单请求 30s 无应答即 close | `KeepAliveState` 在飞满 30s → `CloseUnanswered`；应用请求 30s → `CloseRequestTimeout`；1s watchdog 立即动作，不等下一拍 |
| C 去掉多余 1s、成功 reset | `reconnect_backoff_delay` 改为 1,2,3,5,8…55；`ReconnectSchedule` 在 `Connected` 时 reset 到首拍；新增单测钉序列与 reset |
| D 1000/4409 不重连 | `receive_close_is_terminal` + `StreamOutcome::Terminated` 停环；401/403 仍 `Unbound` 停环；本地槽接管 4000 也停环（防旧管线抢回）；其余走 Fibonacci |
| E 启动失败可重试可观测 | store/manager/建流/能力注册每一步 warn（stage+账号短哈希）；manager 不可用按退避持续重试；单 WS 请求 30s、多步建流/维护 90s、后台 manifest 120s 上限；manifest 拉取移入独立任务；notify/flush 也加 30s 上限 |

配套：`docs/websocket-connection-ledger.md` §3/§5 更新；vendor 台账
`vendor/README.md` 新增本批条目。

## 5. 验证

- `cargo test --bin kt-signal-engine`：547 通过（含新增最小复现
  `receive_loop_retries_unavailable_manager_until_shutdown`、
  `reconnect_schedule_retries_recoverable_then_resets_on_success`、
  `receive_close_codes_follow_official_no_reconnect_rule`）。
- `cargo test --manifest-path vendor/libsignal-service/Cargo.toml --lib`：
  59 通过（含 `websocket::keepalive_tests` 十例）。
- `cargo test --test contract`：90 通过。
- `cargo fmt --check` / `cargo clippy --all-targets --all-features -D warnings`
  / vendor 同名 clippy 全绿；`git diff --check` 无输出。

## 6. 残余风险

- 1000/4409 停环后不会自动恢复：连接器需要显式重链/重启引擎（官方同语义）。
- 30s 单请求闸是官方值；高延迟链路的单条 WS 应答超过 30s 会被判死并
  重连，代价是一次重连而非永久静默。多步建流/维护用 90s、后台 manifest
  用 120s，避免误杀正常慢请求。
- `flush_barrier` 30s 上限把「连接器长阻塞」判为出口死亡；若连接器只是
  极慢（>30s 不读 stdout），会触发 `StdioDead` 停管线，需重启恢复。
- 本次未改 `state.rs`/connector；P0-B 的隔离/恢复流程不受影响。
