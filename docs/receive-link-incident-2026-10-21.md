# 接收链路现场取证（2026-10-21）

> 结论先行：**重启风暴的根因在 connector 账号数据/选择，不在引擎接收链路，也不是
> 引擎凭证被服务端失效。** 本报告为零代码改动取证；工作区中 `src/service/mod.rs` /
> `src/state.rs` / `tests/contract.rs` 的 P0-B orphan quarantine 改动是并行 WIP，
> 本报告未触碰。

## 1. 凭证有效性：有效

从 live data-dir 的 store 读当前设备注册材料（uuid + device_id 2）后：

- HTTP `GET https://chat.signal.org/v1/accounts/whoami`：直连 200、`socks5h://127.0.0.1:7890`
  200，响应含 `uuid` / `pni` / `number` / `usernameHash`。
- raw WebSocket `/v1/websocket/` 升级 101，服务端立即下发
  `PUT /api/v1/queue/empty`。

=> 设备未被移除，Basic 凭据、TLS、代理链路、接收通道均可用。用户证据中的
“网络正常（直连与 socks5h 均 200）”与本节一致。

## 2. HEAD 引擎在同数据副本上接收正常

用 HEAD `4814503` 构建的 `target/debug/kt-signal-engine`，指向
`signal-data` 的副本（同 store、同 `engine-state.json`、同代理）：

| 轮次 | `getUserStatus`（有效号） | `receive.emptied` |
|---|---|---|
| 1 | `{isRegistered: true}` | 3.28s |
| 2（kill -9 后立即重启） | `{isRegistered: true}` | 3.11s |

`listContacts` 对有效号返回 5 条联系人；接收流建立后 `engine.perfMetrics`
可读到 per-account 计数。=> 引擎侧建流、鉴权、接收管线均正常，连续重启两轮仍稳定。

## 3. 现场失败的是“引擎不认识的账号”，不是引擎连接

live `engine-stderr.log`（678 行）在 17:46 之后只有：

- 105 次 `getUserStatus` → `-32603`（每 30s 一次，与 connector watchdog 节奏一致）；
- 4 次 `getConfiguration` → `-32603`；
- 6 次 `listContacts` → `SendError::UnknownAccount`；
- 唯一一次 WS close 是 17:46:38 的 provisioning 连接 `1000 "Closed"`，以及 17:46:42
  一次 slot handover；**没有** 4409 / 401 / keepalive stale / SignalWebSocket error /
  tungstenite error。

引擎 `engine-state.json` 只有 **1** 个账号（`+44743402****`，末四位打码；其 ACI
与 `session-bindings.json` 的 active binding `297438f7…` 一致）。connector 日志
同时出现两个 account id：

- `297438f7…`：active session `aa4751ed…`，发送/接收/会话读取均用它；
- `59103575…`：旧 session `b96f1c0f…`（16:00–17:44 有活动，17:46 重链后不再绑定）。

connector 的 liveness ping 目标由
`SELECT signal_account FROM accounts WHERE proxy_group=?1 LIMIT 1` 选出（无
`ORDER BY`，旧行先插入），而 contacts resync 会遍历组内全部账号。对引擎发
`getUserStatus` / `getConfiguration` / `listContacts`（未知号）实测 **0.34s
返回 -32603**，与现场 `duration_ms=0` 完全一致；对有效号则成功。

=> 失败的是 connector 仍持有的旧账号行；引擎对未知账号返回 `-32603` 是既有的
signal-cli 兼容语义，不是“接收通道不可用”的连带表现，也不是引擎要重连的信号。

## 4. 根因判定

| 层 | 判定 | 证据 |
|---|---|---|
| 网络 | 排除 | 直连/socks5h whoami 均 200；WS 101 |
| 引擎代码 | 排除（本次重启风暴） | HEAD 引擎同数据副本 receive 3.1–3.4s；live 日志无 4409/keepalive/WS error |
| 引擎数据/凭证 | 当前账号有效；旧账号无引擎 store | state 仅 1 账号；connector 有 2 个 account id；UnknownAccount |
| connector 账号数据/选择 | **根因** | watchdog `LIMIT 1` 命中旧行 + resync 遍历旧行 → `-32603` → 判 liveness 失败 → ~330s 重启 |

重启风暴 = connector ping 到引擎不存在的旧账号，不是引擎 bug。接收延迟是重启风暴
的连带结果：每次重启都会重建接收流，重启窗口内的信封只能等服务器队列重放。

## 5. 恢复路径

1. 在 connector store 中识别并移除/退役 `59103575…` 这个没有引擎 store 的旧账号
   （或把它移出 `default` proxy group）；确保该组只剩引擎 `listAccounts` 能对上的账号。
2. 若确实还需要旧账号的历史/在线能力：按 connector 正常流程重新扫码链接一次
   （新设备身份），不要复用旧 store。
3. 验证：对 `listAccounts` 每个账号调用 `getConfiguration` / `listContacts` 均成功；
   watchdog 不再 30s 报 `getUserStatus`；引擎不再按 ~330s 被重启。
4. 不要手工删除 `accounts/lk-*` 目录；P0-B 的隔离/恢复流程先于任何删除（并行 WIP）。

## 6. 官方语义对照（供后续对齐，不作为本次修复）

| 事项 | 官方行为 | 出处 |
|---|---|---|
| 应用层 keepalive | 每 30s 发 `GET /v1/keepalive`；单请求 30s 无应答 → close(3001)；5min 无成功应答 → 立即 close；成功 → `lastAliveAt=now` 并重置 30s 计时 | Signal-Desktop `ts/textsecure/WebsocketResources.preload.ts`:504-514 / :544-580 / :609-640 |
| libsignal ChatConnection | 本地/远端空闲 31s 发 WS PING；45s 无任何入站 → 断连 | libsignal v0.99.0 `rust/net/infra/src/timeouts.rs`（`WS_KEEP_ALIVE_INTERVAL` / `WS_MAX_IDLE_INTERVAL`）+ `rust/net/infra/src/lib.rs`:291-297 `RECOMMENDED_WS_CONFIG` |
| 重连 | Fibonacci `1,2,3,5,8,13,21,34,55s` + 5s jitter；连接失败 `return reconnect()`（不终止）；连接成功 `#backOff.reset()`；1000/4409 不重连；401/403 → authError 不重连 | Signal-Desktop `ts/textsecure/SocketManager.preload.ts`:98 / :256-296 / :363 / :391-394；`ts/util/BackOff.std.ts` |

引擎现状（本次未改，仅记档）：应用层 30s tick、5min stale 已对齐；但“无应答”在
**下一次 tick** 才判（30–60s 窗口，官方是单请求 30s）；keepalive send 失败路径只
落 info 级（滚动日志 warn 级看不到），也无 close frame；重连序列为
`1,1,2,3,…`（比官方多一个 1s）。这些是观测/节奏差异，**不是本次重启风暴的原因**，
建议后续单独批处理，不要在本次数据态事件中改动。

## 7. 本报告未做的事

- 未改引擎/connector 任何代码；vendor keepalive 已保持原样。
- 未 commit / push。
- 未触碰并行 P0-B WIP 的三个文件。
