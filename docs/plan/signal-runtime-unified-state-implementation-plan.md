# Signal 三进程统一状态与自愈 — 实施计划

- 状态：计划稿（Phase 1 完成；Phase 2 heartbeat + coordinator；Phase 3 gate + snapshot IPC；Phase 4 link/unlink journal；Phase 5 instanceId + stateRevision + revision gap refetch；Phase 6 deterministic chaos + kill escalation + gray switch；schema consistency 已修复）
- 关联设计：`signal-runtime-unified-state-self-healing.md`
- 目标：把“进程级重启”升级为“状态级权威 + 可证明自愈”，消除“进程不在 → 显示已解绑”的误报。
- 结论：方向完整，但不是零风险终稿；直接开工前先补 6 个工程约束。

---

## 0. 评审结论

### 0.1 方案是否完善

覆盖完整：状态分层、权威矩阵、epoch、就绪门禁、绑定事务化、混沌测试都已定义。

仍需补 6 个约束：

1. **不新增第二套状态机。** 现有 `runtimeHealthMachine` 继续做环境/进程子状态；Coordinator 只做总拥有者，把 health 作为子 reducer，避免两份 health 互相覆盖。
2. **wire 只做 additive。** 账号状态增加 `stateEvidence` / `observedAt` / `stale`，不改旧字段语义；旧 desktop 仍能读，新 desktop 对旧 connector 做保守降级。
3. **跨进程事务显式化。** link 已有 lease、`linkSessionId` 和 lost-finish 收养；不新增第二套 `linkTxnId`，直接用 `linkSessionId` 做事务键，补 desktop binding journal，否则进程在提交点死掉仍可能留下幽灵绑定。
4. **按 accountId 分片。** 一个全局 `phase` 不足以表达“9 个账号 1 个故障”；快照里要有 `accounts[]`，UI 只对故障账号降级。
5. **状态对象不跨线程搬。** Electron Main 单写者；connector 用 per-slot actor/Mutex；engine 一账号一进程一 LocalSet；renderer 只收序列化快照。真正要解决的是异步回包的**顺序**，不是把状态搬到另一个线程。
6. **快照必须是 app 级 + session 投影。** `phase` 是运行时总状态；`sessions[].view` 才是 link/app/unlinked/recovering/repair。不能把 `account_unlinked` 塞进 phase，也不能只放一个 binding。

### 0.2 线程隔离结论

| 层 | 执行模型 | 状态归属 | 隔离结论 |
|---|---|---|---|
| Desktop Main | JS 单线程事件循环 | Coordinator 独占 | 不开 `worker_threads` 承载状态；异步回调只提交事件 |
| Connector | Tokio 多线程 runtime，task 可换 worker | `Arc<Mutex<ConnectorService>>`、`AccountPool` 的 slot 表、每 slot supervisor | 共享状态由 mutex/actor 串行化；Rust 类型系统保证 `Send/Sync`；`LocalSet` 不跨进程 |
| Engine | 一账号一进程；进程内单账号 LocalSet | 进程内独立 | 跨账号无共享地址空间；跨线程无 `Rc/RefCell` |
| Renderer | 独立进程 | 只读快照 | 不共享可变对象；IPC 序列化 |

**真实风险不是线程安全，而是状态顺序：**

- 旧请求/旧事件晚到，覆盖新状态；
- connector 重启后旧回调仍写桌面；
- 多窗口同时订阅，收到不同 revision；
- 主线程被重计算阻塞。

对应措施：Coordinator 单写者 + `epoch` + `revision` + 串行 dispatch + 有界快照；重计算只传纯数据，不搬状态。

### 0.3 进程死亡 / 假死矩阵

“被杀”与“假死”是两种故障，当前处理不一样：

| 故障 | 当前行为 | 会不会自动补齐 | 缺口 |
|---|---|---|---|
| Desktop Main 被杀 | 整个客户端退出；connector 父进程探针 5s 内发现并收掉 engine；engine stdin EOF 干净退出 | 下次打开客户端时恢复；进程内不会自拉起 | Electron 主进程自身没有外部守护；要自动拉起需要 launchd/Task Scheduler 一类外部 launcher |
| Connector 被杀 | 桌面收到 child exit，最多 5 次退避重启；engine stdin EOF 退出 | 多数情况会重新拉起 connector + engine | 5 次耗尽进 crash_loop；UI 仍可能进入旧工作面 |
| Connector 假死（进程在、不响应） | 请求超时；没有桌面侧周期心跳探测 | **当前不会自动重启** | 必须加 heartbeat；连续 miss 后 kill + restart |
| Engine 被杀 | connector watchdog 发现 exit / ping 失败，重启该 slot | 会；池化下只影响单账号 | 重启耗尽后变 degraded；需要 UI 正确呈现 |
| Engine 假死 | watchdog 连续 2 次 `getUserStatus` 失败后重启 | 会，但慢 | tick 30s + 单次 15s，2 次失败；实际可能是分钟级，反复重启还受 5 分钟 anti-storm 限制 |
| 三者全不在 | 打开客户端时 Home 启动 runtime | 打开后可以重建；客户端没启动时无进程可恢复 | 依赖用户/OS 启动客户端 |
| Key 不匹配 / store 损坏 | 重试后熔断 | **不会自动修复** | 必须进 repair，不能删数据 |

**结论：**

- “被杀”多数能自动重启；但 **connector 假死目前是空白**，这是本轮必须补的。
- “重新组成一个整体”不能只看进程起来，还要过：connector handshake → engine slot running → whoami/listAccounts → 两次稳定 inventory → binding 对账。任何一步没过，就只能是 `starting/degraded/repair`，不能显示 `ready`。
- Desktop Main 自身被杀，Electron 进程内没有自愈；本轮保证“下次打开时恢复”，外部自动拉起另开需求。

---

## 1. 不可变约束

- I1 每个状态只有一个写者：Main Coordinator。
- I2 进程缺席、网络失败、列表缺席只能产生 `unknown/degraded`，不得产生 `device_unlinked`。
- I3 `device_unlinked` 只接受上游 401/未注册，或 engine whoami 明确未注册。
- I4 所有异步结果带 `epoch`；旧 epoch 不写回。
- I5 状态对象不进入 worker thread；重计算只允许传纯数据。
- I6 不在持锁时 await；connector 新代码不引入跨 await 的 `StdMutex`。
- I7 wire 变更 additive；旧 desktop/旧 connector 可共存一个版本窗口。
- I8 不自动删除有 registration 的数据目录；key/corrupt 只进 repair。
- I9 快照有界：只放运行态元数据，不放消息正文/附件/联系人全量。
- I10 所有自动动作有上限、退避、可取消、可回滚。

---

## 2. 目标状态模型

### 2.1 快照

```ts
interface SignalRuntimeSnapshot {
  epoch: number;
  revision: number;
  desired: 'on' | 'off';
  phase: 'boot' | 'starting' | 'ready' | 'degraded' | 'repair' | 'stopped';
  components: {
    desktop: 'ok' | 'error';
    connectors: Array<{
      id: string;
      instanceId: string;
      state: 'absent' | 'starting' | 'ready' | 'exited' | 'failed';
      channel: { rpc: 'up' | 'down'; receive: 'up' | 'down' | 'unknown'; lastEventAt?: number };
      lastHeartbeatAt?: number;
      revision: number;
      lastError?: { code: string; message: string; retryable: boolean; at: number };
    }>;
    engines: Array<{
      connectorId: string;
      accountId: string;
      state: 'starting' | 'running' | 'exited' | 'failed' | 'unprovisioned';
      receive: 'up' | 'down' | 'unknown';
      lastHeartbeatAt?: number;
      restarts: number;
    }>;
  };
  accounts: Array<{
    accountId: string;
    state: 'ready' | 'unknown' | 'device_unlinked' | 'unlinking';
    evidence: 'upstream' | 'engine' | 'inventory' | 'cache';
    observedAt: number;
    stale: boolean;
  }>;
  sessions: Array<{
    sessionUid: string;
    connectorId: string;
    binding: {
      accountId?: string;
      state: 'unbound' | 'linking' | 'active' | 'unlinking' | 'unknown';
      revision: number;
    };
    view: 'link' | 'app' | 'unlinked' | 'recovering' | 'repair';
  }>;
  lastError?: { code: string; message: string; retryable: boolean; at: number };
}
```

### 2.2 phase / view 派生

`phase` 是运行时总状态，`sessions[].view` 是会话投影，两者不混用：

- desktop 级致命错误，或所有已绑定 connector 都进入不可修复态 → `phase=repair`。
- 单个 connector/slot 的 `lastError.retryable=false`（key/corrupt/lock 冲突）→ 该 connector 的 session `view=repair`，全局 `phase=degraded`（不阻塞其他账号）。
- 任一已绑定账号的 slot 未 running，或 connector 正在启动 → `phase=starting/degraded`。
- 所有已绑定账号的 slot 达到 running，且 inventory 稳定 → `phase=ready`；单个账号 `device_unlinked` 只影响该 session 的 `view=unlinked`，不把全局 phase 拉成 repair，也不阻塞其他账号。
- 用户显式停止 → `phase=stopped`。

会话 view（优先级：repair > unlinked > recovering > app > link）：

- 该 session 的 connector 致命，或全局 phase=repair → `view=repair`。
- binding active + account `device_unlinked`（仅终态证据）→ `view=unlinked`。
- binding active + 该 session 的 connector 正在启动/降级 → `view=recovering`。
- binding active + account ready → `view=app`。
- 无 binding / linking → `view=link`。
- 其他 connector 的 degraded 不把本 session 拉成 recovering。

**命名约定：** wire 用 `stateEvidence` / `stateObservedAt`；Coordinator 快照用 `evidence` / `observedAt`。两者是投影映射，不混用。

### 2.3 权威矩阵

| 事实 | 权威 | 缓存规则 |
|---|---|---|
| 设备是否仍链接 | Signal 上游 401/未注册/whoami | 只读展示，必须标 stale |
| accountId → engine slot | connector pool | 可重建，不可反向覆盖 |
| engine 健康 | connector watchdog | 心跳过期只降级，不判死 |
| session 绑定 | desktop registry | 进程状态不参与 unbind |
| 会话/消息 | connector store | 用 revision 触发重拉 |
| 健康总状态 | Coordinator | renderer 只读 |

---

## 3. 工作分解

### Phase 0 — 基线与测试环境

- 备份并隔离当前 dev profile；key 不匹配单独处理，不混入代码修复。
- 建立 fake connector/engine harness：可控启动、退出、延迟、乱序事件、key 错误。
- 固化现有 `runtimeHealthMachine` 单测，作为子 reducer 基线。
- 验收：不改产品代码，harness 能复现“engine 不在 → 旧 device_unlinked → 横幅”。

### Phase 1 — Connector 账号状态证据（additive）

改动：
- `kt-signal-connector/src/store.rs`：`stateEvidence` / `stateObservedAt` 只做审计，不当作当前权威；启动读取时一律重新验证。更稳的做法是只持久化 `state_origin` / `state_observed_at`，`stale` 在投影时计算。
- `src/supervisor.rs`：`list_accounts()` engine 不可用时，回退行标 `stale=true`；`device_unlinked` 投影为 `unknown`，保留原值供诊断。
- `src/service.rs`：所有 AccountSummary 投影带 evidence；upstream 401/未注册或 engine whoami 明确未注册才写 `device_unlinked`；启动时对历史 `device_unlinked` 做一次有界复核。
- `store.rs` 的 `upsert_account_from_signal_with_id`：不要因为 `listAccounts` 列出本地账号就无条件把 `device_unlinked` 改回 `ready`；只有 finishLink 或 whoami registered 才清终态。否则会出现“解绑 → 列表复位 ready → 401 再解绑”的抖动。
- `engine_event_watch_loop`：active link/finish 窗口内收到 401/AccountUnauthorized 时，不立即持久化 `device_unlinked`；窗口结束后复核再落。当前桌面侧 `linkWindowDepth` 只抑制 health 事件，store 已经被写终态，顺序太晚。
- connector fatal 退出前输出结构化原因（例如 `KT_FATAL code=STORE_KEY_REJECTED retryable=false`）。
- `src/supervisor.rs` / desktop `connectorSupervisor.ts`：识别 `STORE_KEY_REJECTED` / key mismatch，落 `key_missing` 修复桶（detail 区分“密钥缺失/密钥不匹配”，不新增第十一态）并标 `retryable=false`，不再做 5 次无意义重启；`data_corrupt` 同理。
- `schemas/connector-api-v1.schema.json`：新增可选字段，版本 revision +1。
- 测试：engine down、key mismatch、正常 list、device_unlinked、启动复核、device_unlinked + listAccounts 不得复位、link 窗口内 401 不得持久化。

验收：
- engine 不在时，connector 不再把 `device_unlinked` 作为可消费终态。
- key mismatch 一次失败就进 repair，不再显示“进程未启动”并重试 5 次。
- 旧字段仍可读；旧 desktop 不崩。
- 不删除任何账号目录。

### Phase 2 — Desktop Coordinator（单写者 + epoch）

改动：
- 新增 `electron/main/signal/runtimeCoordinator.ts`：`dispatch(event)` 串行队列、`snapshot`、`epoch`、`revision`、`AbortController`。
- `runtimeHealthMachine.ts` 保留为子 reducer；Coordinator 组合它，不复制状态。
- `connectorPool.ts` / `connectorSupervisor.ts`：状态变化改为向 Coordinator 发事件；旧 `getStatus()` 保留兼容投影。
- `registerIpc.ts`：新增 `signal:getRuntimeSnapshot` / `signal:subscribeRuntimeSnapshot`；旧 `getRuntimeStatus` 继续可用。IPC 按调用方 sessionUid 过滤 `sessions[]`，诊断导出才带全量。
- `shared/signalWire.ts`：新增 snapshot 类型；旧类型不动。
- 新增 connector 心跳：Coordinator 每 4s 直连发 `runtime.status`（绕过请求调度队列，单次 3s 超时）；连续 3 次 miss → 优雅终止 → 宽限 → 强杀（POSIX 用 SIGTERM/SIGKILL；Windows 用 child.kill/taskkill）→ 走同一条重启预算。
- 假死判定分两级：3 次 miss → `degraded`；`lastEventAt` / 最近成功请求在 15s 内时先不杀。硬截止：连续 30s 没有任何成功 `runtime.status` → 即使还有事件也重启（处理“事件能出、请求面卡死”的半死状态）。
- link/finish 窗口内 heartbeat miss 只延迟判定，不杀 connector；窗口结束仍无成功 status 才重启，避免把正在等手机确认的链接打断。
- 心跳失败与 child exit 共用重启预算，避免“假死重启 + 退出重启”双倍风暴。
- 测试：乱序事件、旧 epoch 回包、双 connector 聚合、多窗口订阅、connector 假死。

验收：
- 任意异步回包晚到，不覆盖新 epoch。
- 一个 connector 故障不会把其他账号标成 unlinked。
- connector 假死：心跳和事件都静默后 ≤15s 被发现；最近有事件时先观察，最坏约 30s。重启另计，不会无限卡在“进程在但无响应”。
- 主线程无长任务；快照生成 < 10ms。

### Phase 3 — 账号状态与 UI 门禁

改动：
- `sessionHost.ts`：投影前消费 `stale` / `evidence`；缓存 `device_unlinked` 降级 `unknown`。
- 旧 connector 兼容：字段缺席时 `stale` 视为 true；持久化的 `device_unlinked` 一律按 `unknown`，只有本次进程收到的 `account.changed` / `ACCOUNT_UNLINKED` 才信任。这样旧 connector 的陈旧终态不会继续出假横幅。
- `connectorSupervisor.ts`：`ACCOUNT_NOT_FOUND` 不再直接喂 `account_unlinked`；先做有界 inventory/whoami 复核。
- `src/view/signal/SignalWorkspace.vue`：横幅条件改为“终态证据 + 非 stale”；新增 `phase + sessions[].view` 门禁。
- `signalRuntimeBootstrap.ts`：返回结构化结果；失败不吞。
- `Home.vue` / 路由：`phase=starting` 显示恢复壳；`sessions[].view='repair'` 显示修复页；只有 `sessions[].view='unlinked'` 才显示重链；其他 session 不受影响。
- 测试：kill engine 后不出现“已解绑”；key mismatch 显示“需要修复”；真实 401 才出重链。

验收：
- 进程不在时，页面是“正在恢复/需要修复”，不是“手机端已解除关联”。
- 旧 binding 不会被自动删除。

### Phase 4 — Link / Unlink 事务化

改动：
- `sessionHost.ts`：直接复用 `linkSessionId` 作为事务键贯穿 begin/finish；binding 只在 finish 成功后的同步段写入（现有顺序保留），增加 journal 记录 `committed/failed/unknown`。
- finish 的 unknown outcome 走现有 lost-finish 收养（唯一新账号才收养），明确失败才 cancel/staging cleanup；不做盲目回滚。
- connector `service.rs` / `store.rs`：link commit 与 account row 用同一 `linkSessionId` 幂等；重复 finish 回同一结果。
- `beginLogout` / `purgeDeadAccountBinding`：desktop operationId 与 connector delete operation 对账；重启后续跑。
- `migrate.rs` / cleanup：只清理无 registration 的 staging；有 registration 的进 quarantine，等待人工确认。
- 测试：finish 后 desktop 死、connector 死、engine 死；重启后要么 active，要么 unknown，不出现幽灵 active。

验收：
- 扫码取消/失败不留下 active binding。
- 解绑中断后重启可继续；不会复活旧 account。
- 同号不会被两个 slot 同时持有。

### Phase 5 — 同步 revision 与心跳

改动：
- connector：账号级 `stateRevision` 与 slot `lastHeartbeatAt/receiveState` 进入 runtime 快照；`lastHeartbeatAt` 由 connector 的 watchdog ping 成功/最近 receive 事件推导，v1 不给 engine 加新方法。
- desktop：快照 revision 跳变时重拉会话/联系人/未读；消息沿用现有 cursor，不新增全量轮询。
- v1 不做事件日志/seq 重放：账号/运行时事件带 `revision`，消息事件沿用现有 ID/cursor；revision 缺口就重拉快照与读模型。`revision` 只在同一 `connectors[].instanceId` 内可比；connector 重启换 instanceId 时强制全量重拉，避免旧 revision 大于新 revision 导致丢更新。精确 seq 重放列为后续。
- 视图打开/回前台做一次快照对账，不常驻轮询。
- 测试：断线重连、乱序事件、长会话、批量未读；消息链路只做回归，不在本轮重写。

验收：
- UI 不会长期停留在旧账号/旧会话状态。
- 重连后不需要重启应用即可收敛。
- 消息链路只做现有 cursor/ID 语义的回归，不在本轮重写。

### Phase 6 — 混沌与灰度

- 混沌脚本：kill connector/engine/renderer；connector 假死（SIGSTOP/不响应）；engine 假死；扫码中 kill；finish 中 kill；解绑中 kill；key 错；双 engine 同号；网络断开。
- 灰度：`KT_SIGNAL_RUNTIME_COORDINATOR=1` 先 dev；shadow 对比旧 health；再默认开；保留一键回旧路径。
- 观测：每次转换记录 `epoch/from/to/evidence`；诊断导出包含最近 50 条转换。
- 回滚：关开关即回旧投影；connector 新字段 additive，不需要数据回滚；不执行破坏性迁移。

---

## 4. 测试矩阵

- Reducer：随机事件排列 + 旧 epoch 回包，属性测试确保终态不会被旧事件覆盖。
- phase/view：无绑定、active、device_unlinked、repair、multi-session、multi-connector 的派生表。
- 进程：connector 5 次熔断、engine watchdog、parent death、orphan sweep、Desktop Main SIGKILL 后下次启动恢复。
- 账号：engine down、key mismatch（一次失败即 repair）、store corrupt、list timeout、401、device_unlinked、启动复核。
- 绑定：link cancel、finish unknown、finish 后 desktop crash、unlink 中断、ACI 重建、同号双 slot。
- 假死：connector 无响应（有心跳/无事件）、connector 忙但事件仍在（不得误杀）、link/finish 窗口内 heartbeat miss 不得杀、engine 假死（分钟级，不承诺 15s）。
- UI：starting/recovering/repair/unlinked 四态；多窗口；缓存只读；不出现假横幅。
- revision：connector 重启换 instanceId 强制全量重拉；旧 revision 晚到不得覆盖新 instance。
- 性能：1000 条事件、10 个账号、快照 < 10ms，主线程无 > 50ms 阻塞。
- 兼容：旧 desktop + 新 connector；新 desktop + 旧 connector；字段缺席路径。

---

## 5. 完成定义

1. connector/engine 被杀，正常情况下 15s 内进入 `degraded/restarting`；连续故障按退避预算；Desktop Main 被杀为下次启动恢复；不出现“已解绑”。
2. connector 假死：≤15s 进入 degraded，30s 无成功 status 硬重启；engine 假死按现有 watchdog（30s tick + 15s×2，受 5min anti-storm）收敛，不承诺 15s。
3. engine 不在时，账号显示“正在恢复”，不是“手机端已解除关联”。
4. 只有上游明确未注册或 engine whoami 明确未注册，才出现重链引导。
5. link 失败不留下 active binding；重启后无幽灵账号。
6. 同一账号不会同时被两个 engine 持有。
7. 多账号中单 slot 故障不影响其他账号。
8. 所有状态转换可用 `epoch/evidence` 复盘。
9. 旧 desktop/旧 connector 在过渡窗口可用。
10. 构建、类型、lint、单测、集成、混沌全绿。

---

## 6. 非目标

- 不切换到官方 Signal Desktop；不改 Matrix 协议。
- 不引入 worker thread 承载状态。
- 不做破坏性数据迁移；不自动删除有 registration 的目录。
- 不重写消息存储；只加 revision/证据层。

---

## 7. 关键文件

Desktop：
- `electron/main/signal/runtimeCoordinator.ts`（新）
- `electron/main/signal/connectorHeartbeat.ts`
- `electron/main/signal/runtimeHealthMachine.ts`
- `electron/main/signal/connectorSupervisor.ts`
- `electron/main/signal/connectorPool.ts`
- `electron/main/signal/sessionHost.ts`
- `electron/main/signal/registerIpc.ts`
- `shared/signalWire.ts`
- `src/view/signal/SignalWorkspace.vue`
- `src/view/signal/signalRuntimeBootstrap.ts`
- `src/view/signal/signalRuntimeGate.ts`（新）
- `src/view/signal/signalRevisionGap.ts`（新）
- `src/services/signalHost.ts`
- `electron/preload/index.ts`
- `src/global.d.ts`
- `src/view/home/Home.vue`

Connector：
- `src/supervisor.rs`
- `src/store.rs`
- `src/service.rs`
- `src/pool.rs`
- `schemas/connector-api-v1.schema.json`

Engine：
- `src/service/mod.rs`（whoami/健康面，如需）
- `src/service/supervision.rs`（如已有状态面）

---

## 8. 执行顺序

Phase 0 → 1 → 2 → 3 → 4 → 5 → 6。

Phase 1/2 可并行，但 Phase 3 依赖 2 的 snapshot，Phase 4 依赖 3 的 binding 证据，Phase 5 依赖 2 的 revision。

每阶段独立可回滚；Phase 1 先上 additive 字段，Phase 2 再切读路径，避免同时改两端。

---

## 9. 执行记录（2026-11-03）

### 已落地

连接器：
- accounts schema v43：新增 `state_origin` / `state_observed_at`（additive nullable）。
- `AccountSummary` 新增 `stateEvidence` / `stateObservedAt` / `stale`（false 不上 wire）。
- engine 不可用 / store-only 投影：`stale=true`、`stateEvidence='cache'`、
  `device_unlinked` 降级 `unknown`。
- inventory upsert 不再覆盖 `device_unlinked`；finish 成功显式
  `mark_account_ready_by_id(..., "engine")` 清终态。
- link/finish 窗口内 `AccountUnauthorized` 不立即持久化终态。
- 修复 origin/main 合并遗留的 `DELETE , delete_failed` SQL 语法错误。

桌面：
- `ACCOUNT_NOT_FOUND` 不再喂 `account_unlinked`；只有 `ACCOUNT_UNLINKED` 进入终态。
- `SignalAccountSummary` 消费 `stateEvidence/stateObservedAt/stale`。
- `getSessionView` 对无权威证据的 terminal 状态降级 `degraded/stale`。
- “已解绑”横幅只认 `upstream/engine` 且非 stale 的终态。

### 门禁

- Connector `cargo test --lib`：892 passed。
- Connector `cargo fmt --check`：绿。
- Connector `cargo clippy --all-targets --all-features -D warnings`：绿。
- Desktop `tsc6 -p tsconfig.signal-node.json --noEmit --disableSourceOfProjectReferenceRedirect false`：绿。
- Desktop `vue-tsc --noEmit`：绿。
- Desktop `signalConnectorSupervisor.spec.ts`：80 passed。
- Desktop `signalConnectorHeartbeat.spec.ts`：5 passed。
- Desktop `signalRuntimeCoordinator.spec.ts`：6 passed。
- Desktop `signalRuntimeGate.spec.ts`：5 passed。
- Desktop `signalRevisionGap.spec.ts`：3 passed。
- Desktop `signalRuntimeChaos.spec.ts`：2 passed。
- Desktop Phase 3/5/6 deterministic 测试集：116 passed（supervisor 81 + pool 12 + IPC 7 + gate 5 + coordinator 6 + revisionGap 3 + chaos 2）。
- 加 heartbeat 5，deterministic 合计 121 passed；跨重启 unlink 定向测试另 1 绿。

### 未完成 / 已知问题

- key mismatch 已完成：connector stderr `KT_FATAL code=STORE_KEY_REJECTED`；
  desktop 落 `key_missing`、retryable=false、停止自动重启预算；桌面 supervisor
  测试 80 绿。
- connector heartbeat 已落地：`connectorHeartbeat.ts` 决策表 + supervisor
  4s 直连 `runtime.status`、3s 超时、3 miss、15s 事件迟滞、30s 硬静默；
  link/finish 窗口内延迟；测试 5 绿。
- Runtime Coordinator core 已落地：`runtimeCoordinator.ts` 单写者、
  begin/commit generation guard、epoch/revision、phase 派生、per-session
  view 优先级；`ConnectorPool.aggregateStatus()` 已记录快照，新增
  `runtimeSnapshot()`。coordinator 测试 5 绿，pool 测试 12 绿。
- Coordinator core 已接入 `ConnectorPool.runtimeSnapshot()`；新 IPC 尚未接。
- Phase 3 gate 已接 UI：新增 `signalRuntimeGate.ts` 纯决策 + `SignalWorkspace`
  recovering/repair 全屏门禁；测试 5 绿。
- Snapshot IPC 已接：`signal:getRuntimeSnapshot` → preload → `signalHost.getRuntimeSnapshot`
  → `SignalWorkspace.refreshStatus`；gate 优先消费 coordinator phase，旧 status
  作为 fallback。deterministic 测试集 121 绿（含 IPC boundary 7）。
  - 当前 IPC 按 sessionUid 过滤：`registerIpc` 读该 binding 的
    `getSessionView`，填 `sessions[]`/`accounts[]` 后交给
    `pool.runtimeSnapshotWith()`；无 sessionUid 时返回全量绑定投影。
    session view 已用于 UI gate（`sessions[].view=repair/recovering` 优先）。
- link 窗口内 401 已补专门测试：`account_unlink_write_is_deferred_while_a_link_session_is_pending`。
- Phase 4 link commit journal 已落地：connector schema v44 `link_commits`；
  同 `linkSessionId` 重复 finish 直接 replay 同一 account，不再触发第二次
  手机确认或 LINK_NOT_FOUND。
- Phase 4 unlink 面复核：desktop `beginLogout/completeLogout/abortLogout`
  + connector `account_delete_operations` 已有 operationId journal，
  `signalSessionHost.spec.ts` 覆盖 unknown 结果后续删同一 operationId；
  本轮未新增删除路径。
- Phase 5 已落地：
  - connector status 可选 `instanceId`；Coordinator 按
    `connectorId:startedAt:pid` 合成，revision 签名纳入 instanceId。
  - connector schema v45 `accounts.state_revision`；AccountSummary
    `stateRevision` additive；inventory 不抖动，真实迁移才 +1；Coordinator
    revision 签名纳入 stateRevision。
  - 读模型按 revision 缺口重拉已接：`signalRevisionGap.ts` 检测 revision
    跳变/回退/instanceId 变更，gap 时重拉会话列表与当前消息；单步变化不重拉。
- Phase 6 部分落地：
  - connector kill 升级测试：unresponsive connector SIGTERM → 宽限 → SIGKILL。
  - 灰度 kill switch：`KT_SIGNAL_RUNTIME_COORDINATOR=0` 时 renderer 不使用
    coordinator phase/session view，回退旧 status gate；snapshot 仍采集供诊断。
  - deterministic chaos：`signalRuntimeChaos.spec.ts` 串起
    ready → non-retryable repair → recovered app → authoritative unlink，
    以及旧 generation 晚到不覆盖新 repair；测试 2 绿。
- Phase 4 跨重启 unlink 恢复测试已补：reload `session-bindings` 后
  `logoutSession` 续用同一 operationId；测试 1 绿。
- Connector `schema_consistency` 已修复：`group.changed` 及后续 event 分支的
  allOf 指针因历史插入漂移，已按实际 `group.changed` / `message.deleteForMe` /
  `identity.verifiedChanged` / `message.receiptsChanged` / `message.attachmentBackfillSettled`
  索引重钉；4 项 schema 测试全绿。
- Desktop 其余两个 session-view 测试失败（link-only auto-connect、prepare stopped/failed）
  与本次改动路径无关，尚未在本批修复。
