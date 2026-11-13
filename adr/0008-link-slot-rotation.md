# ADR 0008：链接双槽 QR 轮换——attempt/槽模型与 attempt 级提交

日期：2026-11-11 ｜ 状态：已接受 ｜ 批次：双槽 QR 轮换引擎腿（wire additive【engine-compat】）
依据：ADR 0004（链接流程）、ADR 0003（B1 会话终局）、`docs/jsonrpc-compat.md` §3
`startLink`/`rotateLink` 行与 §5「双槽 QR 轮换引擎腿批」块

## 背景

单槽 QR 的扫码窗口只覆盖「手机扫到 → 确认」这一次。connector/桌面要在**不打断
用户确认**的前提下换码，官方桌面 Provisioner 的做法是同一时刻开多个 provisioning
socket 轮换（`ts/textsecure/Provisioner.preload.ts`：45s 一换、最多 2 个 socket
在开、最多 6 轮）。引擎此前把「一次 `startLink` = 一个 provisioning 会话 = 一条
`PendingRecord`」写死在 `Inner::links` 与 `finishLink` 的 uri 索引上，没有
attempt 概念：想换码只能再发 `startLink`，而新 `startLink` 会作废旧会话（旧
finish 确定性回 LINK_LOST）——用户正在确认的那一次被顶掉，等于把轮换做成了重来。

本批在引擎侧补 attempt/槽模型与两个 additive 键，把「轮换」与「重来」分开。换码
时机是产品节奏，引擎不设定时器（见决策 5 差异①）。

## 决策

1. **attempt 与槽的关系：attempt 级提交、每槽独立 `PendingRecord`**。
   `startLink` 铸第 0 槽并铸 `linkAttemptId`；`Inner::attempts`（attemptId →
   `LinkAttempt{slots: Vec<uri>, committed}`）只做归属与提交门，`Inner::links`
   （uri → `Arc<PendingRecord>`）仍是每槽一条独立记录——uri 键控保证一条 finish
   只能等自己那槽的结果，不可能读到别人的号码冒充成功。slot 号 = live 槽表下标
   （0 起，进程内存态，收回后重排，不落盘）。提交是 **attempt 级**：全部槽的
   `PendingRecord::committed` 共享同一个 `Arc<AsyncMutex<Option<LinkedAccount>>>`，
   只有真正执行过 `state.add_account` 的那次 finish 才会落 `Some`。没有 rotate
   时一个 attempt 只有一个槽——单槽语义与旧版逐字相同。wire 面 `startLink` 的
   `deviceLinkUri` 仍是第 0 槽，`linkAttemptId` / `slots` 是 additive 键。
   新 `startLink` 照旧作废上一个 attempt（清 `attempts` + 取走全部 `links`），
   同一时刻只有一个 attempt 活着。

2. **最多 2 个 live 槽，第三个进就淘汰最旧，死槽先出表**。
   `LINK_MAX_LIVE_SLOTS = 2`（`src/service/mod.rs:935`）。`rotateLink` 为同一
   attempt 铸新槽并 push，`while slots.len() > 2` 从表头（铸造序最旧）取淘汰对象。
   槽表不变量：**`attempt.slots` 恒等于 live 槽表**。任何让 uri 离开 `links` 的
   出口（finish 的 LINK_LOST 收口、退休收口）都同步
   `retain_live_attempt_slots`；`rotateLink` 在 push 新槽前再按 `links` 存在性
   修剪一次（双保险），淘汰循环遇死槽跳过、继续找真正最旧的活槽。结果：死槽既
   不占 `slot` 下标、也不抢淘汰名额（活槽不会被死槽顶退休），应答 `slots` 只含
   live 槽。调用方每次按应答里的 `slots` 全表重渲染，不得缓存槽号。

3. **退休分两级：有在飞 finish 只置旗，否则当场收口**。
   `Service::retire_slot` 先置 `retired`（此后不再接受**新的** finish），然后：
   - **有在飞 finish**（`inflight_finishes > 0`）：只置旗返回，**绝不 abort**——
     abort 会把用户正在确认的那次提交结果丢掉。物理收口延后到最后一次 finish
     离场时在 `FinishGuard::drop` 里归零触发 `finalize_retired_slot`。
   - **无在飞 finish**：当场 `finalize_retired_slot` → `retire_pending_record`：
     立 `superseded` 旗 → abort flow 并 await → take store 句柄 → 未提交则回收
     未注册目录（先 take 再 `remove_link_store`，Windows 才删得掉）→ 落
     LINK_LOST 墓碑（64 条 FIFO，重复 finish 仍稳定回同一确定错误）→ 从 `links`
     摘除。
   已随 attempt 提交的记录不走墓碑：`retire_pending_record` 见 `committed` 非空
   就保留 store、唤醒等在本槽结果上的 finish 让它读提交位拿回同一账号，返回
   `None`（不抹掉成功）。提交成功后 `retire_attempt_siblings` 把同 attempt 其余
   槽全部物理收口，并把 attempt 从表里摘掉（此后 `rotateLink` 答
   `LINK_ATTEMPT_NOT_FOUND`）。

4. **finish 接受任一 live 槽，提交只发生一次，被顶掉回 LINK_LOST**。
   `finishLink` 仍按 `deviceLinkUri` 索引自己那槽的记录，该 attempt 内任一 live
   槽都可以完成链接。已退休的槽不再接受新的 finish（回 LINK_LOST），但已计数的
   在飞那次照常完成。提交段由 `committed` 锁贯穿（读注册数据 → 设备名改写 →
   `add_account`）：先到者落 `Some`，后到者读同值返回——同槽并发、跨槽并发都只
   提交一次；已提交 attempt 的任一槽重复 finish 幂等回同一账号（connector 超时
   重试的确定面）。被顶掉的两条路径都归 LINK_LOST：记录已被新 `startLink` 取走
   → 墓碑命中回 `LinkError::Lost`（无墓碑才是 `UnknownUri`）；在飞 finish 醒来
   见 `superseded` 或 `lost_at` 亦回 LINK_LOST，不冒充 `-32603`。URL 交付前的
   失败仍是 `-32603`（ADR 0003 B1 语义不变）。

5. **与官方 Signal Desktop Provisioner 的对应与差异**
   （`ts/textsecure/Provisioner.preload.ts`，只读行为参照）：
   - **对应**：`MAX_OPEN_SOCKETS = 2`（:97）↔ `LINK_MAX_LIVE_SLOTS = 2`——第 3
     个槽进就收最旧（官方 `#sockets.push` 后 `while length > 2` 逐个 shift 掉
     最旧并 `disconnect`，:440-443）。「任一信封到达即完成」↔「任一 live 槽的
     finish 都能完成链接」（官方 `onReceivedEnvelope` 解出信封即
     `SocketState::Done` + `EventKind.Envelope`，:377-396；落账号在 main 侧
     消费方，Provisioner 自己只发通知）。
   - **差异①轮换时机与次数**：官方引擎自己 45s 定时（`ROTATION_INTERVAL`，:96）、
     最多 6 轮（`MAX_ROTATIONS`，:98，超限抛 `MaxRotationsError`），首连前按
     `QR_CODE_TIMEOUTS = [10,20,30,60]s`（:102-107）逐级放宽等 uuid 超时。我方
     引擎不设定时器、不数轮次：`rotateLink` 由 connector/桌面驱动，引擎只约束
     「同时最多 2 个 live 槽」；attempt 的生命周期由调用方决定（它想转几轮就几轮）。
   - **差异②退最旧槽的力度**：官方无条件 `disconnect`；我方分两级——有在飞
     finish 时只置退休旗、把物理收口延后到那次 finish 归零。官方在 socket 粒度上
     没有「用户已确认、正在等提交」的窗口（信封到达即完成，:387 `state = Done`），
     所以它可以一刀切；我们不行（finish 可能阻塞数分钟）。
   - **差异③完成面的单飞**：官方无 attempt 概念，谁先到信封谁赢，其他 socket 直接
     丢弃（`#handleClose` 里 `index === -1` 忽略，:453-456），不需要幂等。我方是
     **attempt 级单飞 + 已提交跨槽幂等**：connector 可以用任一槽的码重放、可以超时
     重试，结果恒为同一账号、账户只建一次。
   - **差异④30s 的语义**：我方 `mint_link_slot` 对「等 URL」有 30s 有界等待
     （超时 = `-32603` + 回收未注册 store）；官方是
     `getProvisioningConnection(timeout)` 的连接/等 uuid 超时。数字不同源，语义都
     只是「建一个槽不能无限挂着」，不承担产品节奏。

6. **回滚：无 rotate 即单槽**。wire 全 additive——不调 `rotateLink` 的调用方读到
   的 `slots` 恒为单槽、`deviceLinkUri` = 第 0 槽，行为与旧版逐字一致，所以回滚 =
   调用方停用 `rotateLink`，引擎侧零改动。引擎侧如需彻底退回单槽，把
   `LINK_MAX_LIVE_SLOTS` 落 1（rotate 铸完即淘汰新槽，等价单槽，只是 wire 仍多两
   键）；要连键一起撤就得走 ADR 修订（`docs/jsonrpc-compat.md` §3 两行 + §5 批块
   是形状权威）。双槽期间产生的数据不需要迁移：每槽一个 `accounts/lk-*` 目录，
   未提交槽按 B1 纪律回收，提交后的账号与单槽路径落同一份 `engine-state.json`
   登记。

## 后果与边界

- **旧端零感知**：wire 只加 `linkAttemptId` / `slots` 两键与 `rotateLink` 新方法；
  不读新键的 connector/桌面行为不变（`tests/contract.rs:425` 钉形状）。
- **attempt 表是进程内存态**，不落盘，重启即空——重启后 finish 旧码 = `UnknownUri`，
  与 B1 前一致；没有跨重启的轮换状态要维护。
- **磁盘峰值 2 份半程注册数据**：双槽 = 两个 `accounts/lk-*` store。未提交槽一律
  回收，不留孤儿：铸造过期走 `MintedLinkSlot::discard`，退休走
  `retire_pending_record`，两者同一收口顺序（abort → take store 句柄 → 删目录）。
- **锁纪律**：`inner` 锁内只做 attempt 校验与槽表登记；铸造（开 store + spawn flow
  + 30s URL 等待）与退休收口全在锁外，慢 provisioning WS 不阻塞并发
  start/rotate/finish；finishLink 等手机确认也在锁外（可阻塞数分钟）。
- **不变量由测试钉住**（`src/service/mod.rs` 单测）：rotate 后两槽都能 finish、
  rotate 不打断在飞 finish、第 3 次 rotate 淘汰最旧且旧码回 LINK_LOST、两槽并发
  finish 只提交一次、attempt 不存在回 `LINK_ATTEMPT_NOT_FOUND`；槽表三条——第 0
  槽 LINK_LOST 后 rotate 应答不含死槽、残留死槽不抢淘汰名额、死槽不占 slot 下标。
- **fake 面只覆盖 wire 形状**（无 store、无 attempt 级提交位）：契约测试断言三键
  形状、rotate 槽表、提交 rotate 出的第 1 槽、`LINK_ATTEMPT_NOT_FOUND` 码位；跨槽
  幂等只在真面单测断言，不用 `accounts.len()==1` 自证单飞。
