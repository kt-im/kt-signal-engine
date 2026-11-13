# Signal 三进程统一状态与自愈设计

- 状态：设计稿（Phase 1 完成；Phase 2 heartbeat + coordinator；Phase 3 gate + snapshot IPC；Phase 4 link/unlink journal；Phase 5 instanceId + stateRevision + revision gap refetch；Phase 6 deterministic chaos + kill escalation + gray switch；schema consistency 已修复）
- 范围：KT Desktop（Electron Main/Renderer） / Signal Connector / Signal Engine
- 关联：`engine-pool-per-account-design.md`（每账号一个 engine 进程）
- 结论：**当前只有“进程级重启”，没有“状态级统一权威”；进程不在时仍把本地缓存里的 `device_unlinked` 当成手机端已解绑，这是误报的直接根因。**

---

## 0. 结论先行

### 0.1 “手机还显示已绑定，KT 却显示已解绑”不是 Signal 服务端事实

本地证据链：

1. engine 账号库仍是有效注册态：`signal-data/accounts/lk-fb7a.../store.db` 的 `kv.registration` 含 `uuid=6411f5aa-1f68-4f1e-8e35-625d8ee5da26`，与桌面 binding 的 ACI 一致。
2. 桌面 binding 仍为 `active`：`session-bindings.json`、`bindings/4cc18....json`。
3. connector 不可用时，`list_accounts()` 直接回退到本地 store 行：`kt-signal-connector/src/supervisor.rs:2282-2290`。
4. 本地行可能持久化着旧的 `state='device_unlinked'`：`kt-signal-connector/src/store.rs:2685-2698`。
5. 渲染层只按这个字段出横幅：`src/view/signal/SignalWorkspace.vue:19390`，模板 `:1192`。

所以：**engine 没起来时，本地旧状态被当成“手机端已解绑”呈现；手机侧当然仍是已绑定。**
这不是“旧扫码结果没删”单独造成的，而是“旧状态 + 进程不在 + 回退投影”三者叠加。

证据分级：第 1/2/3/5 条已验证；第 4 条是代码路径成立，但当前 connector sqlite 是加密的，具体行是否正好为 `device_unlinked` 还没有直接读出，属于**推断**。

### 0.2 “Connect 提示进程未启动”与误报是同一条故障链的两端

本机当前 dev profile 的 connector stderr 只有一行：

```text
ERROR kt_signal_connector: connector exiting with an error
error=connector store key was rejected by the existing store
```

connector 每次 spawn 后很快退出（本机日志约 0.1s）；桌面按 1/2/4/8s 退避重试，预算 5 次；60s 内 3 次崩溃另进 `crash_loop`。UI 得到 `START_FAILED`，于是显示“进程未启动”。
这不是网络问题，也不是 Signal 解绑；是 **connector 的加密 store 与当前数据桶的 key 不匹配**。重启不能修复，自动删除又会丢本地历史/凭据，所以当前代码只能失败。

### 0.3 现有自愈边界

| 层 | 已有什么 | 缺什么 |
|---|---|---|
| 桌面 → connector | 5 次重启，1/2/4/8/16s，单次上限 60s；60s 内 3 次崩溃进 `crash_loop` | 不区分可重试/不可重试；启动失败仍可进 Signal 页；状态与账号解绑混在一起 |
| connector → engine | watchdog 按 receive 存活/`getUserStatus`/stderr 重启，指数退避 | 只保证进程，不保证账号状态权威；engine 不在时回退旧 store |
| engine receive | 断线重连、写超时、stale close | 进程/会话/账号三层状态未上报成统一 epoch |
| 绑定恢复 | 启动清扫、缺席不自动解绑、ACI 重建识别 | 旧 link 的跨进程提交窗口与旧 binding 对账仍不完整；staging 清理已有，但不能覆盖 finish/lost-finish 的所有窗口；`ACCOUNT_NOT_FOUND` 仍会被当终态 |

---

## 1. 问题定义

### 1.1 三个“状态”被混成了一个

必须拆开：

1. **组件状态**：desktop 是否存在、connector 二进制是否可执行、engine 是否在跑。
2. **通道状态**：connector 是否握手、engine receive 是否建立、请求面是否可用。
3. **账号状态**：Signal 服务端是否仍认为这台设备已链接。
4. **绑定状态**：KT session ↔ 本地 accountId 的簿记。
5. **投影状态**：renderer 当前看到的会话/消息/健康徽标。

当前实现里，`account.state='device_unlinked'` 同时被用来表达 3 和 5；进程不在时又用 4 的缓存去补 3。任何一层抖动都会冒成“已解绑”。

### 1.2 误报路径（已验证代码路径）

```text
engine 未运行 / listAccounts 失败
  -> connector list_accounts() 回退 store rows
  -> row.state 仍是上一次持久化的 device_unlinked
  -> getSessionView 把 row 原样投影为 account
  -> SignalWorkspace.boundAccountUnlinked = true
  -> “已解绑”横幅 + 自动出码引导
```

补充路径：

```text
任意 account-scoped RPC 返回 ACCOUNT_NOT_FOUND
  -> connectorSupervisor 把 ACCOUNT_NOT_FOUND 与 ACCOUNT_UNLINKED 同等对待
  -> health: account_unlinked（终态）
  -> 只有显式重链或 runtime-ready 才能离开
```

`ACCOUNT_NOT_FOUND` 的语义是“这个 id 在当前 connector/engine 的视图里没有”，不等于“Signal 服务端已解除设备”。冷启动、slot 尚未加载、pool 迁移未完成都可能返回它。这是第二个误报源。

---

## 2. 设计原则

### P1. 每个事实只有一个权威

| 事实 | 权威 | 本地缓存的作用 |
|---|---|---|
| 设备是否仍链接 | Signal 服务端（401/未注册/whoami） | 只做离线展示，必须带 `stale` |
| 当前账号在哪个 engine slot | connector 的 accountId → slot 映射 | 可重建，不可反向覆盖 |
| engine 是否健康 | connector watchdog + 进程状态 | 只做展示 |
| session 绑定到哪个 accountId | 桌面 binding registry | 不能由进程状态推断 |
| 会话/消息投影 | connector store + engine 同步游标 | 可重建 |
| 健康总状态 | **唯一的 Runtime Coordinator** | renderer 只读快照 |

### P2. 进程缺席永远不是账号死亡证据

- 进程不在 → `runtime_unavailable` / `unknown`。
- 网络失败 → `unknown`，可重试。
- 只有上游明确的“设备已解除关联 / 401 / not registered”才能进 `device_unlinked`。
- store 里的旧终态在 engine 未确认前一律降级为 `unknown`，不得直接出“已解绑”。

### P3. 终态必须可反证

任何 `account_unlinked` 结论必须能回答：哪一次上游请求？哪个账号/设备 ACI？服务端返回码/错误体是什么？是否在 link/re-key 窗口内？是否经过两次独立、稳定的 inventory 读？回答不了，就不是终态。

### P4. 自愈只做可逆故障

可自动：进程崩溃、握手超时、receive 断线、锁残留、临时网络。
不可自动：store key 不匹配、密钥丢失、数据目录归属冲突、账号被上游注销。
不可自动的故障必须显示“需要修复”，而不是“已解绑”。

### P5. 进入 Signal 页要有就绪闸

- `ready`：正常进入。
- `starting/restarting`：显示“正在恢复”，可停留但禁用写操作。
- `failed/crash_loop/key_missing/data_corrupt`：显示修复页，不进入聊天工作面。
- `account_unlinked`：只有权威证据成立才进入重链引导。

---

## 3. 目标架构：一个 Runtime Coordinator

### 3.1 位置

放在 **Electron Main**，不放在 renderer，也不放在 connector：

```text
Renderer
  -> preload / signalHost
  -> Main: SignalRuntimeCoordinator  (唯一状态权威)
       -> ConnectorPool / ConnectorSupervisor
            -> Connector process
                 -> per-account Engine process
                      -> Signal server
```

connector 仍是 engine 生命周期的执行者；coordinator 负责“期望状态、收敛、证据、门禁”，不直接操作 engine。

### 3.2 统一快照

```ts
interface SignalRuntimeSnapshot {
  epoch: number;                 // 单调递增，renderer 丢弃旧 epoch
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
      pid?: number;
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

关键点：

- `epoch` 是唯一排序依据；异步 RPC 回来晚于新 epoch 一律丢弃。
- `accounts[].evidence` 明确来源；UI 只对 `evidence='upstream'` 或 `evidence='engine'` 且明确未注册的 `device_unlinked` 出横幅；`inventory/cache` 都不行。
- `stale=true` 的账号只能显示“状态未知/正在恢复”，不能显示“已解绑”。
- `sessions[].binding` 与 `accounts` 分离；绑定存在不等于账号在线，账号不在也不等于绑定该删。

### 3.3 收敛循环

```text
on app_open / user_enter_signal / process_exit / rpc_error / timer:
  1. 读取当前快照
  2. 计算期望状态 desired=on
  3. 若 connector 不在且可启动 -> start（带退避）
  4. 若 connector ready 但 engine slot 不在 -> 让 connector 启动/重启该 slot
  5. 等所有已绑定账号的 slot 达到 running
  6. 对每个账号做 whoami / getUserStatus（有界）
  7. 连续两次稳定 inventory（同一组 accountId/ACI）
  8. 合并账号状态；只有 upstream 或 engine whoami 明确未注册才允许 device_unlinked
  9. 解析 binding（不因缺席解绑）
 10. 生成新 epoch 快照；renderer 原子替换
```

每一步都有 timeout、最大重试、可取消；旧 epoch 的结果不能写回。

---

## 4. 启动与进入页面的时序

### 4.1 冷启动

```text
App Home onMounted
  -> Coordinator.ensure('app_open')
  -> connector start
  -> connector handshake
  -> engine slots start
  -> per-account whoami/listAccounts
  -> 2x stable inventory
  -> binding resolve
  -> phase=ready / degraded / repair
  -> renderer route gate
```

### 4.2 门禁规则

| phase | Signal 页面 | 文案 |
|---|---|---|
| boot/starting | 显示启动壳，不渲染聊天列表 | “正在启动 Signal…” |
| ready | 正常 | — |
| degraded（单 slot 故障） | 可进入，故障账号只读 | “部分账号正在恢复” |
| repair（key/corrupt） | 修复页 | “运行组件需要修复” + 诊断/重装/受控重置 |
| sessions[].view=unlinked（仅 upstream/engine 明确未注册） | 重链引导 | “手机端已解除关联” |
| stopped | 启动页 | “Signal 未启动” + 启动按钮 |

**phase 与 account 终态解耦**：单个账号 `device_unlinked` 只影响该 session 的 `view=unlinked`；只要 runtime 健康，全局 `phase` 仍是 `ready`，不阻塞其他账号。单个 connector 的 key/corrupt 只把该 connector 的 session 置为 `view=repair` 和全局 `phase=degraded`；只有 desktop 级致命或所有已绑定 connector 都不可修复才把全局 `phase` 置为 `repair`。

**禁止**：在 `repair/starting` 阶段用缓存账号状态渲染“已解绑”。

### 4.3 已有实现的差距

- `src/view/signal/signalRuntimeBootstrap.ts` 失败时 `.catch(() => undefined)`，不阻塞 UI。
- `SignalWorkspace.vue` 在 runtime 未 ready 时仍可挂载，只是 composer 禁用。
- `ensureSignalRuntimeOnAppOpen` 只调用一次，没有持续收敛循环。
- `getSessionView` 在 connector ready 但 engine 不在时仍可能返回缓存 `device_unlinked`。

---

## 5. 自愈矩阵

| 故障 | 自动动作 | 上限 | 失败后的用户面 |
|---|---|---|---|
| connector 进程崩溃 | 桌面重启 connector | 5 次，指数退避，60s 熔断 | 修复页，保留日志/诊断 |
| connector 握手超时 | 杀掉半启动进程，重试 | 同上 | “启动超时”，可手动重试 |
| connector 假死（进程在、不响应） | 3 次 miss 先 degraded；30s 无成功 status 则优雅终止/强杀 + 重启；link/finish 窗口内延迟 | 与退出共用 5 次预算；软降级 15s，硬重启 30s | 修复页/degraded |
| connector store key 不匹配 | **不自动删**；标记 `key_missing` | 0 自动 | 修复页：恢复匹配 key / 受控重置 |
| connector store 损坏 | 标记 `data_corrupt`，保留原件 | 0 自动 | 修复页 + 备份/诊断 |
| engine 进程崩溃 | connector watchdog 重启该 slot | 指数退避，防风暴 | 该账号 degraded，其他账号不受影响 |
| engine receive 静默 | ping 2 次失败后重启（tick 30s + 15s×2） | 指数退避，反复重启受 5min anti-storm 限制 | 该账号“正在重连”，实际是分钟级 |
| engine 启动失败 | 重试该 slot；不牵连其他 slot | 每 slot 独立 | 该账号 degraded |
| 网络断开 | 保持 ready/degraded，重连 | 无限退避 + jitter | “网络不可用”，不判解绑 |
| link 中途取消 | 取消 link、删 staging store、释放 lease | 一次 | 回扫码页，无 binding |
| link finish 失败 | unknown outcome 走现有 lost-finish 收养（唯一新账号才收养）；明确失败才 cancel/staging cleanup | 一次 | 不盲目回滚，不产生半绑定 |
| 手机端解绑 | 上游 401/未注册 → `device_unlinked` | — | 重链引导 |
| 用户主动解绑 | 幂等删除任务，重启后继续 | 有界 | 完成后 unbound |
| 重复 engine / 同号双 slot | 目标硬闸；当前 `provision_slot` 先分配随机 id，同号在 commit 阶段才可能被拒 | Phase 4 补跨 slot 唯一性与回滚测试 | 诊断日志，不双写 |

### 5.1 不可自动修复的判定

- `connector store key was rejected by the existing store`
- desktop 必须把 connector fatal 原因分类：当前 key mismatch 只落 `START_FAILED`，还没有变成 `key_missing` 修复面，这是待补缺口。
- `data directory already owned by another process`
- `registration missing` 且无备份
- 上游明确 `ACCOUNT_ALREADY_LINKED` 且当前设备不是持有者

这些必须是 `repair`，不是无限重启。

---

## 6. 绑定与解绑状态机

### 6.1 绑定

```text
unbound
  -> linking (lease + staging store)
  -> active (finish 成功，accountId/ACI 已提交)
  -> unlinking (用户主动)
  -> unbound

active
  -> device_unlinked (仅 upstream 证据)
  -> relinking (用户显式)
  -> active
```

规则：

- `linking` 必须有 lease、transaction id、staging store；超时/取消/失败要删 staging。
- `finish` 成功才写 binding；不能先写 binding 再等 finish。
- 列表缺席、engine 不在、connector 不在：保持 `active`，只写 `unknown`，不自动 unbind。
- ACI 相同但 accountId 变了：视为重建，懒回填，不 unbind。
- 同号重复 slot：拒绝，不产生第二个 active binding。

### 6.2 旧扫码残留

当前风险：

- 旧的 `bindings/<accountId>.json` 与 `session-bindings.json` 可能指向一个已经不再被当前 engine 持有的 accountId。
- binding 只在 finish 成功后写入；真正危险的是“finish 成功后、binding 写入前桌面死掉”的窗口。现有 lost-finish 收养能接住唯一新账号，但收养失败会留下服务端已链接、KT 看不到的 ghost device。
- 下次启动时 connector 回退 store，旧行的 `device_unlinked` 被投影成“已解绑”。

结构修复：

1. 直接复用 `linkSessionId` 作为事务键，binding 只在 finish 成功后的同步提交点写入；不新增第二套 `linkTxnId`。
2. 启动时对每个 binding 做“账号身份对账”：同 accountId 在 engine inventory → active；同 ACI 在新 accountId → 迁移 binding；都不在 → `unknown`，保留 binding，不自动删；上游明确未注册 → `device_unlinked`。
3. 只有 `unknown` 超过人工确认阈值才提示“需要重新链接”，而不是自动横幅。

---

## 7. 账号状态证据模型

### 7.1 状态来源

```ts
type AccountStateEvidence =
  | { source: 'upstream'; code: 'unauthorized' | 'not_registered'; at: number }
  | { source: 'engine'; whoami: 'registered' | 'unregistered'; at: number }
  | { source: 'inventory'; accountId: string; stableReads: 2; at: number }
  | { source: 'cache'; lastKnown: string; at: number };
```

规则：

- `device_unlinked` 只允许 `source='upstream'` 或 `source='engine'` 且 `whoami='unregistered'`。
- `inventory` 只能证明“当前进程看不到”，不能证明“服务端已解绑”。
- `cache` 永远不能产生终态 UI。
- 旧 connector 没有 evidence 字段：启动时读到的持久化 `device_unlinked` 按 `cache` 处理；只有本次进程收到 `account.changed` / `ACCOUNT_UNLINKED` 才升级为终态。

### 7.2 `ACCOUNT_NOT_FOUND` 的正确语义

- 不是 `ACCOUNT_UNLINKED`。
- 先查 connector 的 slot 映射是否存在：slot 存在但未 ready → `unknown/starting`；slot 不存在 → 触发一次有界 `listAccounts` / whoami；两次稳定 inventory 都没有，且该账号此前 active → `account_missing`（可恢复），不是 `account_unlinked`。
- 只有上游 401/not registered，或 engine whoami 明确 `unregistered`，才升级为 `device_unlinked`。

### 7.3 当前代码需要改的点

1. `connectorSupervisor.ts:2157`：拆分 `account_not_found` 与 `account_unlinked`。
2. `supervisor.rs:2282-2290`：engine 不可用时回退行必须带 `stale=true`，且 `device_unlinked` 降级为 `unknown`。
2b. `store.rs:2631`：`listAccounts` 的 upsert 不应无条件把 `device_unlinked` 改回 `ready`；终态只能被 finishLink 或 whoami registered 清除，否则会来回抖动。
2c. `engine_event_watch_loop`：active link/finish 窗口内不立即持久化 `device_unlinked`；桌面侧的 `linkWindowDepth` 只抑制 health，store 已经写终态，顺序太晚。窗口结束后复核再落。
3. `sessionHost.ts:793-804`：投影前检查 `account.stale`，终态只信 `evidence`。
4. `SignalWorkspace.vue:19390`：横幅条件从 `state === 'device_unlinked'` 改为“终态证据 + 非 stale”。
5. `signalRuntimeBootstrap.ts`：失败不能吞掉；返回结构化结果，由路由门禁消费。

---

## 8. 消息/模型状态同步

### 8.1 所有权

| 数据 | 写者 | 读者 |
|---|---|---|
| Signal 协议会话/密钥 | engine | connector |
| 账号注册状态 | engine / 上游 | connector / desktop |
| 会话/消息投影 | connector store | desktop |
| 桌面视图缓存 | renderer | 仅 UI |
| 绑定/会话 | desktop Main | renderer |

### 8.2 同步纪律

- 每个 account 一个 engine slot，slot 内单写者。
- connector 对 engine 的请求带 `accountId + epoch`；响应晚到丢弃。
- 消息写入用稳定 ID/事务 ID 幂等；重复接收不重复插入。
- v1 不做事件日志/seq 重放：connector 给账号/连接器 `stateRevision`；账号/运行时事件带 revision，消息事件沿用现有 ID/cursor；桌面发现 revision 缺口就重拉快照与读模型，不靠“全量轮询”。`revision` 只在同一 `connectors[].instanceId` 内可比；connector 重启换 instanceId 时强制全量重拉，避免旧 revision 大于新 revision 导致丢更新。精确 seq 重放列为后续。
- 读模型（会话列表、未读、联系人）用版本号/失效通知，不靠定时全量轮询。
- 任何“模型状态”都区分 `authoritative / optimistic / stale`，UI 不把 optimistic 当权威。

### 8.3 健康心跳

- v1 不给 engine 加新心跳方法：connector 记录最近一次 `getUserStatus` ping 成功时间、最近 receive 事件时间、queue depth，投影为 slot health。
- connector 聚合为 slot health；超过 2 个心跳周期无更新 → `degraded`，不直接判死。
- 现有 watchdog 是 tick 30s + 单次 15s、连续 2 次失败；engine 假死实际是分钟级，且反复重启受 5 分钟 anti-storm 限制。要 15s 级需要单独提速，不在 v1 承诺。

---

## 9. Matrix 的参考价值

### 9.1 Matrix 是什么

Matrix 是一套开放、联邦的实时通信协议：Homeserver 保存房间事件、账号、设备；Client-Server API 读写房间/同步/发消息；Server-Server API 联邦；Room 是事件 DAG，状态事件 + 消息事件，状态解析处理并发冲突；`/sync` 带 `since` token 长轮询拿增量，`next_batch` 作为下一次游标；E2EE 用 Olm（设备间）、Megolm（房间）、设备密钥/交叉签名/密钥备份；to-device messaging 做设备到设备的定向消息。

### 9.2 可以借的

| Matrix 机制 | 对 KT Signal 的映射 |
|---|---|
| server 是房间状态权威 | 上游是账号链接状态权威；本地只做投影 |
| `since` / `next_batch` 增量同步 | connector `stateRevision` / engine 消息游标；revision 缺口重拉 |
| 事件不可变 + 稳定 ID | 消息/状态事件用稳定 ID，幂等 upsert |
| 发送事务 ID 去重 | link/finish/删除/发送都带 transaction id |
| 设备级身份与验证 | accountId + ACI + device identity 分离 |
| 明确的 unknown/不可达语义 | 进程不在 = unknown，不是 unlinked |
| 退避重连、长轮询 | connector/engine 重连策略 |

### 9.3 不能直接借的

- Matrix 是另一张网络，**不能直接承载 Signal 协议**；用户不会因为接入 Matrix 就变成 Signal 用户。
- Matrix 的 room DAG / state resolution 对单账号单设备场景过重；引入只会增加状态面。
- Matrix 没有“connector 进程守护”这层；它解决的是协议状态与同步，不解决本机进程编排。
- 用 Matrix bridge 接 Signal，底层仍然要跑 signal-cli/engine，维护量不会消失，只是换个壳。

**结论：Matrix 作为“状态权威 + 增量同步 + 幂等”的架构参考，不作为协议替换方案。**

---

## 10. 官方 Signal Desktop 方案（待定）

### 10.1 可行部分

- 每个账号一个独立 `--user-data-dir`，做数据隔离。
- 用独立 profile 做“官方 UI + 我们的壳”，避免维护协议层。
- 通过 CDP/自动化做有限集成。

### 10.2 代价

- 官方客户端没有稳定的外部 API；自动化入口随版本变化。
- SQLCipher key 在系统钥匙串，迁移/备份/多账号管理复杂。
- 官方更新会覆盖模改；签名/打包/升级链要长期维护。
- 仍然是一账号一设备，无法规避 Signal 的设备模型。
- 无法提供当前 connector 这种细粒度消息投影/批量操作/服务端集成。

### 10.3 决策

**暂不切换。** 先把三进程统一状态与自愈做稳；官方客户端只作为 UI/行为参考。如果未来要做，单独开 ADR，以“隔离 profile + 只读/有限写桥”为边界，不与当前协议栈混跑。

---

## 11. 实施切片

### S0. 证据与不变量（1-2 天）

- 增加 account 状态证据字段（source/observedAt/stale），先只读投影。
- 增加“进程不在时禁止输出 device_unlinked”的单测。
- 增加 link finish 失败回滚测试。

### S1. 误报止血（P0，不改协议）

- connector 回退行标记 `stale`；`device_unlinked` 降级 `unknown`。
- 桌面横幅只认非 stale 的终态。
- `ACCOUNT_NOT_FOUND` 不再单独触发 `account_unlinked`。
- 验证：kill engine 后 UI 显示“正在恢复”，不显示“已解绑”。

### S2. Coordinator + 门禁（P0）

- Main 新增 `SignalRuntimeCoordinator`，统一收敛 connector/engine/binding。
- 路由门禁消费 snapshot.phase。
- 所有 IPC 返回带 epoch；旧 epoch 丢弃。

### S3. 绑定事务化（P1）

- link/finish/unbind 复用 `linkSessionId` / `operationId` + journal。
- 启动对账 binding ↔ inventory ↔ ACI。
- 旧 staging store 清理；不自动删有 registration 的目录。

### S4. 状态同步（P1）

- connector 账号/连接器加 `stateRevision`；桌面发现缺口重拉快照与读模型。
- connector 用最近 ping 成功/receive 事件推导 slot health，v1 不给 engine 加新心跳方法。
- 读模型版本号/失效，不再全量轮询。

### S5. 混沌测试（P1）

- kill connector / engine / renderer 中途。
- 扫码中 kill、finish 中 kill、解绑中 kill。
- key 不匹配、store 损坏、锁残留。
- 双 engine 同号、pool 迁移中断。
- 网络断开/恢复、receive 静默。

### 验收线

1. connector/engine 被杀，正常情况下 15s 内进入 `degraded/restarting`；连续故障按退避预算；connector 假死 15s 软降级、30s 硬重启；Desktop Main 被杀为下次启动恢复；不出现“已解绑”。
2. engine 不在时，账号横幅为“正在恢复”，不是“手机端已解除关联”。
3. 只有上游明确未注册或 engine whoami 明确未注册，才出现重链引导。
4. link 失败不留下 active binding；重启后不出现幽灵账号。
5. 同一账号不会同时被两个 engine 持有。
6. 所有状态转换可在日志中用 epoch/evidence 复盘。

---

## 12. 未验证与开放问题

- 当前 dev profile 的 key 不匹配是何时引入的：是复制数据桶、keychain 变更，还是旧版本迁移？需要保留原始 key 备份后再判定。
- 旧 binding `4cc18...` 对应的 connector store 行是否确为 `device_unlinked`：connector sqlite 是加密的，当前无法直接读；需要 connector 诊断导出或匹配 key 后确认。
- 手机端设备列表是否仍包含 ACI `6411f5aa...`：需要用户在手机上核对，或通过 engine 的 linked devices 读取。
- `ACCOUNT_NOT_FOUND` 在 pool 冷启动中的实际触发频率：需要加指标后再决定是否保留“快速终态”路径。
- 官方 Desktop 的隔离 profile 是否能在不破坏更新的前提下做有限桥接：未验证。

---

## 13. 一句话给实现者

**不要把“进程不在”翻译成“账号解绑”；不要把“本地缓存”翻译成“服务端事实”；不要把“重启”当成“自愈”。** 先统一权威和 epoch，再谈重启策略。
