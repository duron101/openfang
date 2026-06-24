# ArkSim 指令生命周期追踪 — 联调中间件需求

> 日期: 2026-06-11 | 状态: Requirements（脑暴产出，待 ce-plan）
> 范围: ArkSim 适配器联调可观测性 | 体量: Deep — feature

---

## 1. 问题与背景

ArkSim 适配器当前是一条"自动接管 → 只取定制态势 → 喂内核"的**单向硬管道**。联调时最典型、最抓狂的场景是：

> **发了 proto 实体指令，但实体没反应，也不知道 ArkService 到底收没收到。**

在现有代码里，这条 `send` 路径有 **7 个环节是全黑的**（已逐一在代码中核实）：

| # | 黑盒 | 位置 | 性质 |
|---|------|------|------|
| 1 | 构建出的 proto 字节看不到（只记 `payload_len`） | `command_mapper::to_proto_bytes` / `lib.rs` 发送处 | 生命周期不可观测 |
| 2 | `SetOutsideControl` 只对武器指令自动补，运动/传感/干扰指令不补 → 实体未接管时静默 no-op | `strike_protocol::partition_strike_batches` | 协议时序缺失 |
| 3 | `send_actions` 等 2s 收不到 ack 就 `warn!` 后返回 `Ok(())`，超时被当成功 | `arkservice.rs` send_actions | 错误被吞 |
| 4 | WarlockDirect 去重：字节相同的指令被 driver 静默丢弃 | `zmq_sim_bridge.rs` 去重逻辑 | 中间件静默吞命令 |
| 5 | `platform_id` 与想定实体名不匹配 → proto 合法但无实体命中，ArkService 不报错 | 协议语义 | 效果不可验证 |
| 6 | `track_id` 与真实航迹不匹配 → 开火无目标，静默不发 | 协议语义 | 效果不可验证 |
| 7 | 没有前后状态 diff → 无法确认实体航向/位置/弹量是否真的变了 | 全链路 | 效果不可验证 |

**核心差距不是某个函数，而是整层"指令可观测 + 效果可验证"抽象的缺失。**

## 2. 用户与主用例

- **主用户**：联调 ArkSim / 外部仿真应用的开发者（协议对接阶段）。
- **主用例**：下发一条实体指令后，能在**一条记录里**看清它走到了哪一步、卡在哪个环节、实体到底有没有按预期动。
- **当前替代做法**：靠 `tracing` 散落的 debug 日志拼凑 + 旁开 python `test_fire_at_target.py` 手测对比字节 —— 信息分散、线上看不到真实字节、超时被当成功。

## 3. 核心产出（Goal）

把"指令发出去 → 是否被接收 → 是否报错 → 实体是否真的动"这条链路，**压缩成一条可查询、可过滤的结构化记录**，让"卡在哪一步"在一条 trace 里一眼可见。

## 4. 选定方案

**A 脊柱 + 折一片 C**（在 Phase 2 三方案中选定）：

- **A（脊柱）**：在现有发送路径上**被动埋点**，每条指令在它本就经过的阶段各落一条结构化记录：`映射 → 编码(proto 字节 + hex) → 封装(JSON 信封) → 发送 → ack/error/超时`。存入**有界环形缓冲**，API 读取。ack 用现有就近匹配（best-effort），不引入 correlation id。
- **C（效果片）**：trace 记录带上**目标实体的发送前 / 发送后状态快照 diff**（航向/位置/弹量/接管标志等关键字段），让记录展示的是"效果"而非只是"发送"。

不选 B（correlation id 精确对齐）：依赖 ArkService 回显 id，未确认；WarlockDirect 裸 protobuf 信封无处可塞 id。

## 5. 功能需求

- **FR-1 全链路阶段记录**：每条指令产出一条 trace，含各阶段时间戳与状态：`mapped / encoded / enveloped / sent / acked | errored | timed_out`。
- **FR-2 原始载荷可见**：trace 含构建出的 proto 字节（hex）与发出的 JSON 信封，可与已知正确字节比对（黑盒 #1）。
- **FR-3 效果 diff**：trace 含目标实体发送前后关键状态快照及差异；无变化时显式标注"窗口内未观测到效果"（黑盒 #5/#6/#7）。
- **FR-4 诚实化**：超时（#3）、去重丢弃（#4）、接管缺失（#2，运动/传感/干扰指令未接管即下发）必须落入 trace 并告警，不再静默返回成功。
- **FR-5 可过滤**：trace 可按实体 id、指令类型、状态（成功/失败/超时/丢弃）筛选。
- **FR-6 API 优先产出**：通过 `/api` 端点返回结构化 trace + 环形缓冲快照，`curl`/脚本可拉取，作为后续 UI 的数据源。
- **FR-7 两通道适用**：ArkService（60004）与 WarlockDirect（18000）两条传输路径都产出 trace。

## 6. 范围边界

**本次实现：**
- 第 4 节选定方案 A+C 与第 5 节全部 FR。

**Deferred for later（原始诉求里有，本期不做）：**
- **消息类型可配置 / 协议扩展**：配置驱动的收发映射与新协议（如实时态势 `zmq_observer_pb3`）接入 —— 独立大轴，另开脑暴。
- **手动启停想定操作面**：把 `start/pause/resume/stop/restart` 暴露成 API/UI 供人工干预 —— 相关但独立。
- **correlation id 精确对齐（方案 B）**：除非确认 ArkService 会回显 id。
- **UI 展示形态**：Web 仪表盘 / CLI TUI / tactical.js 同屏 —— FR-6 的 API 落地后再选，不绑前端。

## 7. 成功标准

- 复现"发指令实体没反应"场景时，**单条 trace 即可定位**根因属于 7 个黑盒中的哪一个。
- 接管缺失（#2）、超时（#3）、去重丢弃（#4）**不再表现为静默成功**。
- 一次开火指令的 trace 能展示：构建字节、是否补了 `SetOutsideControl`、ack/超时、目标实体弹量发送前后差异。

## 8. 待定问题（Outstanding）

- **显示形态**：API 落地后接 Web / CLI / tactical.js 哪一个（用户暂跳过，默认 API 优先）。
- **持久化**：trace 仅内存环形缓冲，还是落 sqlite / jsonl 供回放。
- **效果模型覆盖度**：C 的效果 diff 先覆盖哪些指令类型（开火/航向/接管优先？），慢效果的判定窗口与容差如何取。
- **接管缺失的处理度**：trace 仅"标红告警"，还是顺带为运动/传感/干扰指令也自动补 `SetOutsideControl`（后者属行为变更，需单独评估安全影响）。

## 9. 依赖与假设

- 假设 ArkService 不保证回显 correlation id —— 故 ack 用就近匹配。
- 依赖现有 `WorldSnapshot` 能提供实体航向/位置/弹量/接管标志用于效果 diff；若字段不足需在 `state_mapper` 侧补齐（属 plan-arksim.md 3B「Mapper 补齐」范畴）。
- 不触碰 `openfang-cli`（用户在主力开发）。
