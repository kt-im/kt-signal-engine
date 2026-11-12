# 链接后接收卡顿：取证与结论（2026-11-12）

## 症状

链接（扫码绑定）完成后，首屏消息要等十几到三十秒才出现；卡片遮罩也停留同样久。
稳定态之后正常（同一毫秒内收帧、分类、投递）。

## 已确认的事实

实测一次链接（引擎 stderr，UTC）：

| 时刻 | 事件 |
|---|---|
| 12:35:28 | socket 1（legacy）建立，握手帧 seq 1–2 |
| 12:35:51 | 接收管线启动 |
| 12:35:52 | socket 2（official）建立，**管道绑定到它** |
| 12:35:53 | socket 3（legacy）建立（发送用） |
| 12:35:54–55 | 三个 sync 请求发出（Contacts/Configuration/Blocked） |
| 12:35:55–57 | 内容帧 seq 6–9 **从 socket 2 到达** |
| 之后 | 没有 pipe 投递、没有进入接收循环的日志 |

关键结构性事实：**引擎同时存在三条 websocket 连接**（1 legacy / 2 official / 3 legacy 发送），
MessagePipe 绑定的是 official 那条；内容帧确实从 official 到达，但此后不再下行。

## 已排除的假设

| 假设 | 反证 |
|---|---|
| 单线程 LocalSet 被阻塞 | 500ms 心跳全程无 `local_set_stall`（gap≥2s 才记录） |
| 我们自己的接收循环处理慢 | `envelope_handled` 中位 0–1ms |
| stdout 背压 | `flush_barrier` 未超过 200ms |
| 服务端补发慢 | 帧在 socket 层已收到（`ws_frame_received`），早于投递数秒到数十秒 |
| manifest 拉取阻塞 | manifest 与卡顿窗口不重叠（实测 8.8–11.0s，且在其后开始） |

## 探针清单（本批加入）

| 探针 | 位置 | 说明 |
|---|---|---|
| `ws_instance` | vendor/libsignal-service/src/websocket/mod.rs | 每条连接分配 `socket_id` 与 kind |
| `ws_frame_received` | 同上 | 带上 `socket_id`、`seq`、字节数 |
| `process_frame_enter` / `decode_failed` | 同上 | 解码与进入分支 |
| `frame_branch` | 同上 | 帧类型与有无 request/response |
| `request_stream_sent` | 同上 | 已推入请求流 |
| `pipe_enqueued` | 同上（official 路径） | 已交给 request_sink |
| `pipe_attached` / `pipe_event_wait` | messagepipe.rs | 管道绑定与出队等待时长 |
| `ws_frame_to_loop` | src/service/ws_latency.rs | 帧进入接收循环的排队时长 |
| `envelope_handled` | src/service/mod.rs | 单封信封处理耗时 |
| `flush_barrier` | src/service/mod.rs | stdout 背压 |
| `login_manifest_pull` | src/service/mod.rs | 登录期 manifest 拉取耗时 |
| `local_set_stall` | src/main.rs | LocalSet 心跳缺拍 |

## 官方客户端基线（2026-11-12 同机对照）

官方 Signal Desktop 在同机、同账号下完成一次链接（app.log）：

| 时刻 | 事件 |
|---|---|
| 12:45:50.7 | registration done |
| 12:45:51.3 | 发出 sync 请求 |
| 12:45:53.9 | waiting for postRegistrationSyncs |
| 12:46:01.4 | 收到第一个 sync 消息（Configuration） |
| 12:46:03.9 | handleContacts |
| 12:46:05.6 | postRegistrationSyncs complete |

**注册完成 → 同步结束约 15 秒。** 也就是说“等主设备回传十几秒”是官方也有的行为，
不是 KT 独有缺陷；我们实测 11.6–30 秒，量级相同但偏慢。

## 服务端限流（2026-11-12 实测）

一轮链接里三个 sync 请求全部失败：

```
Rate limit exceeded: retry after PT600S
```

同一时段官方客户端一次链接即可成功——额度是被**反复解绑重链**（一小时内十余次）消耗掉的，
不是账号级封禁。测试纪律：同一账号 10 分钟内不要重复链接。

## 仍未闭合的一点

12:35 那轮：内容帧（seq 6–9）已从 official socket 到达并计入 `ws_frame_received`，
但之后 40 秒内 `ws_frame_to_loop` 一条都没有——接收循环一次未取到。这一条与官方基线不符，
是新加 `pipe_enqueued` / `pipe_event_wait` 探针要回答的问题。

## 下一步

在 official 路径上补齐最后两点：`pipe_enqueued`（已加）与 `pipe_event_wait`（已改为每次都记）。
两者对齐即可判定卡在 `run_official` 的交出侧还是管道的消费侧。

如果确认在 fork 的传输层，则本仓内能做的只有：把 official 接收层改为不阻塞转发，或退回 legacy 连接路径。
