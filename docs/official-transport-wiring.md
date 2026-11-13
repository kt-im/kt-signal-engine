# Official transport 接线方案

日期：2026-11-08
状态：设计，未实施
前置：fa821b7 Process 已接 WsLink

## 1. 目标

让 PushService 在拨号时选择 Legacy 或 Official，且：

- 每账号 identified <= 1
- 发送和收流共用同一个 OfficialSocket
- close code 可快照
- keepalive 只有一个 owner

## 2. 构造形状

```rust
SignalWebSocket::new_official(
    socket: OfficialSocket,
    unidentified_push_service: PushService,
) -> (Self, impl Future<Output = ()>)
```

内部：

- 建 `request_sink/requests` 通道
- 建 `inner.stream`，但入站由 official 事件消费，不从这个 stream 取
- spawn `run_official`：
  - select 应用请求 / official 事件 / kill
  - 应用请求：`socket.send(request, REQUEST_RESPONSE_TIMEOUT)`，回填 oneshot
  - official 事件：`ChatIncoming` 投进 messagepipe 通道
  - Finished：记录 close code，结束
- `SignalWebSocket.official = Some(Arc<OfficialRequester>)`
  - `request()` 直接走 official，不再进 process 通道
- `take_request_stream` 对 official 返回 `None`，presage 走 `stream_from_chat`

## 3. 为什么 request 不能走 process

官方 `send` 是完整往返，`process` 若在 select 里 await 会饿死 keepalive 和收流。  
所以 official 的 `request()` 必须直接调用 `OfficialRequester::send`，process 只做事件和 close。

## 4. 状态函数

| 函数 | Legacy | Official |
|---|---|---|
| is_closed | request_sink closed | !official.is_alive() |
| is_used | stream None | true |
| close_code | inner.last_close_code | official close code |
| terminate | kill.notify | official.disconnect |

## 5. keepalive

- Legacy：旧 30s watchdog
- Official：不启旧 watchdog；由 libsignal-net ping 保活
- 连接建立参数：`RECOMMENDED_CHAT_WS_CONFIG`

## 6. 接线顺序

1. 加 `SignalWebSocket.official` 字段和 `new_official`
2. 加 `run_official`
3. `request()` 分流
4. 状态函数分流
5. `PushService::ws` 按配置选择
6. 真机

## 7. 待拍板

- Official 下 1000 无法表达：先记 None + 打点，不硬编码。
- queue-empty：保持 200。
- 未知 path：400。
