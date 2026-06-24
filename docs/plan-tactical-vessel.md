# OpenFang 远洋无人艇自主大脑 — 实施方案

> 版本: v1.0 | 日期: 2026-06-06 | 状态: Plan  
> 基于: PRD v1.0 + 源码审阅（14 crates, 全量核查）  
> 框架版本: OpenFang v0.3.24

---

> **范围说明（2026-06-08 更新）**：本实施方案聚焦**水面无人艇（USV）**。无人机
> 单机 / 母机 / ArkSIM 蓝军已拆分为独立路线，共享同一套 Phase 0「活线接入」内核：
> - 单机：[`plan-uav-single.md`](plan-uav-single.md)
> - 母机：[`plan-uav-mothership.md`](plan-uav-mothership.md)
> - ArkSIM：[`plan-arksim.md`](plan-arksim.md)
>
> Phase 0 已落地（`crates/openfang-types/src/config.rs` 的 `PlatformConfig`、
> `crates/openfang-kernel/src/platform_boot.rs`、`platform_control.rs`、
> `crates/openfang-runtime/src/platform_tools.rs` 的工具映射器，以及
> `AdapterRegistry` 多 adapter 路由）。本文档中与上述重叠的「实时控制层 / 平台
> 适配层」章节以代码现状为准。

---

## 目录

1. [核心原则](#1-核心原则)
2. [架构修正：实时控制层](#2-架构修正实时控制层)
3. [精简策略：Tactical Feature Flag](#3-精简策略tactical-feature-flag)
4. [直接命令通道](#4-直接命令通道-direct-command-channel)
5. [平台适配层](#5-平台适配层-platform-adapter-layer)
6. [实施分阶段计划](#6-实施分阶段计划)
7. [组件级详细任务](#7-组件级详细任务)
8. [代码量估算（修正版）](#8-代码量估算修正版)
9. [验证门禁](#9-验证门禁)
10. [风险与缓解（更新）](#10-风险与缓解更新)
11. [ArkSIM 仿真集成](#11-arksim-仿真集成)
12. [UMAA 架构对齐与完善](#12-umaa-架构对齐与完善)
13. [异构无人集群协同控制](#13-异构无人集群协同控制)

---

## 1. 核心原则

### 1.1 架构铁律

| 原则 | 内容 | 理由 |
|------|------|------|
| **P1** | 所有控制回路在 Rust 原生层 | WASM sandbox 的 JSON 序列化/反序列化 + fuel metering 开销不可接受于微秒级控制 |
| **P2** | WASM 仅用于非实时任务 | 声学特征后处理、战术偏好配置、参数优化 |
| **P3** | Agent loop (LLM) 做决策，Rust 原生层做执行 | 慢回路（秒级）vs 快回路（微秒级）严格解耦 |
| **P4** | 不修改 openfang-cli | 用户活跃开发中 |
| **P5** | Feature flag 裁剪，非物理删除 | 保持开源主线完整 |
| **P6** | 安全门控改造不破坏现有 ApprovalManager 架构 | 扩展而非替换 |
| **P7** | 平台适配层隔离硬件差异 | 切换仿真/实装只需替换 adapter，不改 Agent 代码 |
| **P8** | 时间敏感操作走直接命令通道 (DCC)，绕过 LLM | 碰撞规避、箔条投放、自毁确认等 <1ms 延迟；LLM 事后审计 |

### 1.2 三层架构（含平台适配层）

```
┌──────────────────────────────────────────────────────────────┐
│  Layer 3: Agent 决策层 (openfang-runtime) — 秒级              │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐    │
│  │ TCA      │  │ Workflow │  │ Trigger  │  │ Backgrnd │    │
│  │ 战术决策 │  │ Engine   │  │ Engine   │  │ Executor │    │
│  └────┬─────┘  └──────────┘  └──────────┘  └────┬─────┘    │
│       │ platform_* tools (协议无关)              │           │
│       │ LLM (Qwen 3.6 27B via llama.cpp)        │           │
├───────┼──────────────────────────────────────────┼───────────┤
│  Layer 2: 平台适配层 (openfang-platform)                      │
│       │                                          │           │
│  ┌────┴──────────────────────────────────────────┴──────┐    │
│  │              PlatformAdapterRegistry                  │    │
│  │  ┌──────────────────┐  ┌──────────────────┐          │    │
│  │  │ ArkSimAdapter    │  │ DdsAdapter       │  ...     │    │
│  │  │ (protobuf/TCP)   │  │ (rustdds/UDP)    │          │    │
│  │  └────────┬─────────┘  └────────┬─────────┘          │    │
│  └───────────┼─────────────────────┼────────────────────┘    │
├──────────────┼─────────────────────┼─────────────────────────┤
│  Layer 1: 原生控制算法 (Rust Native — 微秒级)                 │
│  ┌───────────┴──────┐ ┌──────────┐ ┌──────────┐             │
│  │ NavControl       │ │ Sensor   │ │ Weapon   │             │
│  │ (路径/避碰/能耗) │ │ Fusion   │ │ I/F      │             │
│  └──────────────────┘ └──────────┘ └──────────┘             │
│  ┌──────────┐ ┌──────────┐ ┌──────────────┐                 │
│  │ Audio    │ │ Comm     │ │ SelfDestruct │                 │
│  │ DSP      │ │ Monitor  │ │ Guard        │                 │
│  └──────────┘ └──────────┘ └──────────────┘                 │
│              ↕ PlatformAdapter trait                         │
│         connect() / poll_state() / send_commands()          │
├──────────────────────────────────────────────────────────────┤
│  Layer 0: 外部系统 (仿真/硬件)                                │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                   │
│  │ ArkSIM   │  │ DDS Bus  │  │ CAN/1553 │                   │
│  │ Engine   │  │(FastDDS) │  │ (Future) │                   │
│  └──────────┘  └──────────┘  └──────────┘                   │
└──────────────────────────────────────────────────────────────┘
```

```

---

## 2. 架构修正：实时控制层

### 2.1 问题

PRD 原方案的"WASM + DDS → 微秒级设备控制"不可行：

- `sandbox.run_wasm()` 每次调用经过：host_call dispatch → capability check → JSON serialize/deserialize
- Wasmtime fuel metering 每 N 条指令触发 epoch check
- 全局 60s tool timeout 对所有 WASM 调用生效
- JSON 编解码时延在 100μs-1ms 级别，叠加后远超微秒级预算

### 2.2 解决方案

所有实时控制路径为 **Rust 原生函数**，通过 `KernelHandle` trait 暴露给 Agent loop：

```rust
// 实时路径示例（Rust 原生，不经过 WASM）
impl DdsBridge {
    /// 发布导航指令到 DDS topic — 微秒级延迟
    pub fn publish_nav_command(&self, cmd: NavCommand) -> Result<(), DdsError> {
        // 直接调用 rustdds DataWriter::write()
        self.nav_writer.write(&cmd, 0)?;  // 0 = 默认时间戳，无分配
        Ok(())
    }

    /// 订阅传感器航迹 — 回调模式，零拷贝
    pub fn subscribe_radar_tracks(&self, callback: impl Fn(Vec<RadarTrack>)) {
        // DDS DataReader + WaitSet，回调在 DDS 事件线程执行
    }
}
```

Agent 通过 tool call 触发控制动作（如 `dds_publish_nav`），tool runner 调用 Rust 原生函数而非 WASM：

```
LLM decision → tool_runner::execute("platform_set_heading")  // 通过 PlatformAdapter trait 路由 
  → DdsBridge::publish_nav_command()  // 原生 Rust, <100μs
  → DDS DataWriter::write()
```

WASM 仅保留用于：
- 声学特征**后处理**（FFT 结果的可视化/日志格式化）
- 战术偏好参数的动态调整
- 自定义分类模型的热加载

---

## 3. 精简策略：Tactical Feature Flag

### 3.1 Feature Flags 定义

```toml
# Cargo.toml (workspace)
[features]
default = ["full"]
full = ["channels", "skills-bundled", "hands", "desktop", "migrate", "web-search"]

# 战术局域网 profile — 最小化二进制 + 零攻击面
tactical = ["offline-core", "ofp", "dds", "audio-dsp", "arksim"]
offline-core = []
ofp = ["openfang-wire"]
dds = ["rustdds"]
audio-dsp = []
arksim = ["prost", "prost-types"]

# 可选组件（tactical 不包含）
channels = ["openfang-channels"]
skills-bundled = ["openfang-skills/bundled"]
hands = ["openfang-hands"]
desktop = ["openfang-desktop"]
migrate = ["openfang-migrate"]
web-search = []
```

### 3.2 条件编译映射

| 组件 | 条件 | tactical 包含？ |
|------|------|:---:|
| openfang-kernel | always | ✅ |
| openfang-runtime | always | ✅ |
| openfang-memory | always | ✅ |
| openfang-types | always | ✅ |
| openfang-wire | `feature = "ofp"` | ✅ |
| ArkSim Bridge (新增) | `feature = "arksim"` | ✅ |
| DDS Bridge (新增) | `feature = "dds"` | ✅ |
| Audio DSP (新增) | `feature = "audio-dsp"` | ✅ |
| openfang-api | always (调试接口) | ✅ |
| openfang-channels | `feature = "channels"` | ❌ |
| openfang-skills (bundled) | `feature = "skills-bundled"` | ❌ |
| openfang-hands | `feature = "hands"` | ❌ |
| openfang-desktop | `feature = "desktop"` | ❌ |
| openfang-migrate | `feature = "migrate"` | ❌ |
| Web Search (Tavily/Brave/Perplexity/DDG) | `feature = "web-search"` | ❌ |
| reqwest TLS (rustls) | `feature = "web-search"` 或 `feature = "channels"` | ❌ |
| tokio-tungstenite | `feature = "channels"` | ❌ |
| lettre + imap | `feature = "channels"` | ❌ |
| 27 云 LLM provider 配置 | 配置文件默认关，仅保留 `ollama` / `custom` localhost | — |

### 3.3 预期效果

| 指标 | Full Build | Tactical Build |
|------|-----------|---------------|
| 二进制体积 | ~32 MB | ~14-18 MB |
| Rust 依赖 crate 数 | ~300+ | ~100-130 |
| 攻击面 (网络) | 40 adapter + reqwest/TLS + tungstenite + SMTP/IMAP | OpenAIDriver (仅 HTTP) + OFP (TCP+HMAC) + DDS (UDP) |
| 编译时间 | 基准 | 减少 50-65% |
| 内存占用 (空闲) | ~40 MB | ~25-35 MB |

---

---
---

## 4. 直接命令通道 (Direct Command Channel)

### 4.1 问题

当前架构中，所有控制指令必须经 LLM agent loop 分解再路由：

```
Sensor → poll_state() → EventBus → Agent (LLM, 2-8s) → platform_* tool → ActionCollector → Adapter
```

这在以下场景不可接受：

| 场景 | 要求延迟 | 当前路径延迟 | 差距 |
|------|:------:|:--------:|:----:|
| 碰撞规避机动 | < 50ms | 2-8s | 40-160x |
| 雷达锁定 → 自动箔条 | < 100ms | 2-8s | 20-80x |
| 岸基武器批准回传 | < 200ms | 2-8s + OFP RTT | 10-40x |
| 自毁指令执行 | < 500ms | 2-8s | 4-16x |
| 高频航向修正 (10Hz) | < 100ms/次 | 不可行 | ∞ |

**核心矛盾**：LLM 提供战术智能但引入不可控延迟；紧急响应要求确定性但不需要智能。

### 4.2 解决方案：双通道架构

```
                              ┌─────────────────────────┐
                              │   WorldSnapshot 入站     │
                              │   (sensor data)          │
                              └───────────┬─────────────┘
                                          │
                    ┌─────────────────────┼─────────────────────┐
                    │                     │                     │
                    ▼                     ▼                     │
         ┌──────────────────┐  ┌──────────────────────────┐   │
         │  慢通道 (秒级)    │  │  快通道 / DCC (微秒级)    │   │
         │                  │  │                          │   │
         │  EventBus        │  │  DirectCommandChannel    │   │
         │     ↓            │  │     ↓                    │   │
         │  Agent (LLM)     │  │  RuleEngine              │   │
         │     ↓            │  │     ↓                    │   │
         │  platform_* tool │  │  generate PlatformCmd    │   │
         │     ↓            │  │     ↓                    │   │
         │  ActionCollector │  │  (audit log + event)     │   │
         │                  │  │                          │   │
         └────────┬─────────┘  └────────────┬─────────────┘   │
                  │                         │                  │
                  └─────────┬───────────────┘                  │
                            ▼                                  │
                   ┌────────────────┐                          │
                   │ AdapterRegistry│                          │
                   │ .route_cmds()  │                          │
                   └───────┬────────┘                          │
                           ▼                                   │
                   PlatformAdapter::send_commands()            │
```

**快通道（DCC）** 不经过 LLM，直接由规则引擎基于 WorldSnapshot 匹配条件 → 生成 PlatformCommand → 注入 AdapterRegistry。

### 4.3 RuleEngine 设计

```rust
// crates/openfang-runtime/src/direct_channel.rs

/// 直接命令通道 — 时间敏感的规则驱动快速响应
pub struct DirectCommandChannel {
    rules: Vec<TriggerRule>,
    enabled: AtomicBool,        // 可被 Agent 动态禁用
}

/// 一条触发规则
pub struct TriggerRule {
    pub name: String,                         // 规则名称 (用于审计日志)
    pub condition: TriggerCondition,          // 触发条件
    pub action: PlatformCommand,              // 触发动作
    pub priority: CommandPriority,            // Critical / High
    pub cooldown_ms: u64,                     // 冷却时间 (防重复触发)
    pub max_fires_per_minute: u32,            // 频率上限
    pub requires_capability: Option<Capability>, // 所需权限
    pub enabled: bool,
}

pub enum TriggerCondition {
    /// 雷达锁定检测: track.quality > threshold && track.range < max_range
    RadarLock {
        min_track_quality: f64,    // 0.0-1.0, 默认 0.7
        max_range_m: f64,          // 最大检测距离
        track_affiliation: Option<Affiliation>, // 仅敌方/任意
    },

    /// 碰撞风险: 最近接触点 CPA < min_distance
    CollisionRisk {
        min_cpa_m: f64,            // 最小会遇距离 (米)
        max_tcpa_s: f64,           // 最大会遇时间 (秒)
    },

    /// 外部指令到达 (OFP/A2A 审批、自毁指令等)
    ExternalCommand {
        command_type: ExternalCmdType,  // ApprovalResponse / SelfDestruct / WaypointUpdate
        requires_hmac: bool,            // 是否需 HMAC 验证
    },

    /// 系统状态转移
    StateTransition {
        from_state: String,        // "autonomous" | "degraded"
        to_state: String,          // "emergency" | "survive"
    },

    /// 复合条件 (AND/OR)
    And(Vec<TriggerCondition>),
    Or(Vec<TriggerCondition>),
    Not(Box<TriggerCondition>),
}

/// 命令优先级 — 决定在 ActionCollector 中的排序
pub enum CommandPriority {
    Critical = 0,   // DCC 生成，立即注入 adapter，不进入 ActionCollector
    High = 1,       // DCC 生成，排在 LLM 命令之前
    Normal = 2,     // 标准 LLM 路径
}
```

### 4.4 DCC 执行流程

```rust
impl DirectCommandChannel {
    /// 每帧调用 — 在 poll_state() 之后、Agent tick 之前执行
    /// 返回的 Critical 优先级命令直接写入 adapter，不等待 Agent
    pub async fn evaluate(
        &mut self,
        snapshot: &WorldSnapshot,
        registry: &AdapterRegistry,
        audit: &AuditLog,
    ) -> Vec<DirectActionResult> {
        let mut results = Vec::new();

        for rule in &self.rules {
            if !rule.enabled || rule.is_in_cooldown() {
                continue;
            }

            if rule.condition.evaluate(snapshot) {
                // Capability 检查
                if let Some(ref cap) = rule.requires_capability {
                    if !self.capability_check(cap) {
                        audit.log_violation(&rule.name, "capability_denied");
                        continue;
                    }
                }

                // 生成命令
                let mut cmd = rule.action.clone();
                cmd.priority = rule.priority;

                match rule.priority {
                    CommandPriority::Critical => {
                        // 立即注入 adapter，不排队
                        registry.route_commands(&[cmd]).await;
                    }
                    _ => {
                        // 注入 ActionCollector (排在 LLM 命令之前)
                        self.action_collector.push_priority(cmd);
                    }
                }

                // 审计日志
                audit.log_direct_action(&rule.name, &cmd);
                
                // 发布事件 (LLM 可在下一 tick 看到)
                self.event_bus.publish(EventPayload::DirectAction {
                    rule: rule.name.clone(),
                    command: cmd.clone(),
                    timestamp: Utc::now(),
                });

                rule.fire();  // 更新冷却计时器
                results.push(DirectActionResult { rule: rule.name.clone(), command: cmd });
            }
        }

        results
    }
}
```

### 4.5 预定义规则集 (示例)

```toml
# config.toml — direct_command_rules
[[direct_command_rules]]
name = "auto_chaff_on_radar_lock"
condition = { type = "RadarLock", min_track_quality = 0.7, max_range_m = 8000 }
action = { type = "FireChaff", count = 3, interval_s = 0.5 }
priority = "Critical"
cooldown_ms = 5000
max_fires_per_minute = 6

[[direct_command_rules]]
name = "collision_avoidance"
condition = { type = "CollisionRisk", min_cpa_m = 200, max_tcpa_s = 60 }
action = { type = "SetHeading", heading_delta_deg = 90, speed_ms = "max" }
priority = "Critical"
cooldown_ms = 2000
max_fires_per_minute = 30

[[direct_command_rules]]
name = "auto_jam_on_threat_radar"
condition = { type = "RadarLock", min_track_quality = 0.5, max_range_m = 20000, track_affiliation = "Hostile" }
action = { type = "JamStart", bandwidth_hz = "wide" }
priority = "High"
cooldown_ms = 10000

[[direct_command_rules]]
name = "shore_weapon_approval_forward"
condition = { type = "ExternalCommand", command_type = "ApprovalResponse", requires_hmac = true }
action = { type = "WeaponArmAcknowledge" }
priority = "Critical"
cooldown_ms = 0

[[direct_command_rules]]
name = "shore_self_destruct_forward"
condition = { type = "ExternalCommand", command_type = "SelfDestruct", requires_hmac = true }
action = { type = "SelfDestructExecute" }
priority = "Critical"
cooldown_ms = 0
# 注：SelfDestructGuard 验证仍在执行前触发
```

### 4.6 安全约束

DCC 是快速路径但不是无约束路径：

| 约束 | 机制 |
|------|------|
| **操作白名单** | DCC 只能触发 `RuleSet` 中预定义的命令，不能生成任意 PlatformCommand |
| **武器限制** | DCC 不得触发 `FireAtTarget` / `FireSalvo` 类型命令（仅箔条/干扰可以） |
| **岸基审批** | `ApprovalResponse` 和 `SelfDestruct` 命令仍需 HMAC 验证 + quorum 检查后再执行 |
| **审计追踪** | 所有 DCC 动作写入 Merkle 审计链，与原 Agent 审计链路合并 |
| **Agent 事后可见** | 每步 DCC 动作都发布 EventPayload::DirectAction，LLM 下一 tick 可以看到并评估是否撤销 |
| **Agent 可禁用** | TCA Agent 可通过 `platform_dcc_disable("rule_name")` 动态关闭特定规则 |
| **频率上限** | 每条规则有 `max_fires_per_minute` 防止振荡触发 |
| **冷却时间** | `cooldown_ms` 防止同一规则短时间内重复触发 |

### 4.7 LLM 与 DCC 的协作模型

```
Tick N:
  1. DCC 评估 WorldSnapshot → 检测到雷达锁定 → 自动发射箔条 (Critical)
  2. DCC 发布 EventPayload::DirectAction { rule: "auto_chaff", ... }
  3. Agent tick 开始 → LLM 在状态摘要中看到 "DCC fired: auto_chaff_on_radar_lock"
  4. LLM 评估战术态势 → 决定是否持续投放、是否需要规避机动
  5. LLM 生成的 platform_* 命令排在 DCC 命令之后，合并发送

Tick N+1:
  1. DCC 评估 → 雷达锁定仍在 → 仍在冷却期，跳过
  2. Agent 评估 → LLM 可能决定: platform_evasive_maneuver() 或 platform_dcc_disable("auto_chaff")
```

**DCC 是 LLM 的下级**：预先授权的快速反射，LLM 保留监督和撤销权。

### 4.8 设计决策

| 决策 | 内容 |
|------|------|
| ADR-011 | 引入 DirectCommandChannel 作为 LLM Agent 的快速反射路径 |
| ADR-012 | DCC 规则集由配置文件定义，支持热加载（无需重启 kernel） |
| ADR-013 | Critical 优先级命令绕过 ActionCollector，直接写入 AdapterRegistry |
| ADR-014 | DCC 操作白名单: 禁止 FireAtTarget/FireSalvo，仅允许防御性动作 + 外部指令转发 |
| ADR-015 | DCC 所有动作写入 Merkle 审计链，Agent 通过 EventBus 获得事后可见性 |

---

## 5. 平台适配层 (Platform Adapter Layer)


> **设计目标**: Agent 决策逻辑与后端协议完全解耦。切换 ArkSIM -> DDS -> CAN 只需改配置文件，不改一行 Agent 代码。

### 4.1 核心 Trait

```rust
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn adapter_id(&self) -> &str;
    fn adapter_type(&self) -> AdapterType;
    async fn connect(&mut self) -> Result<(), PlatformError>;
    async fn disconnect(&mut self) -> Result<(), PlatformError>;
    fn is_connected(&self) -> bool;
    async fn poll_state(&mut self) -> Result<WorldSnapshot, PlatformError>;
    async fn send_commands(&mut self, commands: &[PlatformCommand]) -> Result<CommandResult, PlatformError>;
    fn capabilities(&self) -> PlatformCapabilities;
}
```

### 4.2 Crate 结构

```
crates/
├── openfang-platform/              # trait + AdapterRegistry (协议无关)
│   └── src/
│       ├── lib.rs                  # PlatformAdapter trait
│       ├── registry.rs             # 多 adapter 路由管理
│       ├── command.rs              # PlatformCommand enum
│       └── capabilities.rs         # PlatformCapabilities 位图
│
├── openfang-platform-arksim/       # ArkSIM protobuf 适配器
│   ├── build.rs                    # prost-build
│   └── src/
│       ├── lib.rs                  # ArkSimAdapter: impl PlatformAdapter
│       ├── state_mapper.rs         # StateMessage -> WorldSnapshot
│       └── command_mapper.rs       # PlatformCommand[] -> ActionsFromOutside
│
├── openfang-platform-dds/          # DDS 适配器
│   └── src/
│       ├── lib.rs                  # DdsAdapter: impl PlatformAdapter
│       ├── publisher.rs            # PlatformCommand -> DDS write
│       └── subscriber.rs           # DDS read -> WorldSnapshot
│
└── openfang-types/src/
    └── platform.rs                 # WorldSnapshot, PlatformCommand 领域类型
```

### 4.3 Agent Tools — 协议无关命名

| 旧名称 (耦合后端)      | 新名称 (协议无关)        |
|-----------------------|-------------------------|
| `sim_get_state`       | `platform_get_state`    |
| `sim_set_heading`     | `platform_set_heading`  |
| `sim_set_speed`       | `platform_set_speed`    |
| `sim_fire_at_target`  | `platform_fire`         |
| `sim_jam_start`       | `platform_jam_start`    |
| `dds_publish_nav`     | (合并入 platform_set_heading 等) |

### 4.4 配置驱动的后端切换

```toml
# 仿真模式
[platform]
mode = "simulation"
[platform.adapters.primary]
type = "arksim"              # 改 type 即可切换后端
host = "127.0.0.1"
port = 5000

# 实装模式
[platform]
mode = "hardware"
[platform.adapters.primary]
type = "dds"
domain_id = 0

# 混合模式 (仿真激励 + 硬件在环)
[platform]
mode = "hybrid"
[platform.adapters.sim_vessel]
type = "arksim"
platforms = ["usv-01", "usv-02"]
[platform.adapters.hw_vessel]
type = "dds"
platforms = ["usv-03"]
```

### 4.5 设计决策

| 决策     | 内容 |
|----------|------|
| ADR-006  | trait object `Box<dyn PlatformAdapter>` — 支持运行时热切换 |
| ADR-007  | 领域类型在 `openfang-types/src/platform.rs` — 编译期一致性 |
| ADR-008  | Agent tool 统一 `platform_` 前缀 |
| ADR-009  | 每个 adapter 为独立 crate — 编译期可选链接 |
| ADR-010  | trait 无泛型参数 — 避免单态化爆炸，保持 `dyn` 可用 |

---

## 6. 实施分阶段计划


### Phase 0: 基础设施准备 (Week 1)

| 任务 | 工时 | 产出 |
|------|------|------|
| P0-01 | Cargo.toml 添加 feature flags | 1d | `cargo build -F tactical` 可编译 |
| P0-02 | 条件编译 gate 添加到各 crate 入口 | 1d | `#[cfg(feature = "channels")]` 标注 |
| P0-03 | 精简 LLM provider 列表（仅保留 Ollama/Custom） | 0.5d | 配置项 |
| P0-04 | 移除 Web Search 依赖（reqwest 条件编译） | 0.5d | `#[cfg(feature = "web-search")]` |
| P0-05 | llama.cpp + Qwen 3.6 27B Q4_K_M 部署验证 | 1d | 本地推理 >15 tok/s 基准 |
| P0-06 | `config.toml` tactical profile 模板 | 0.5d | 预配置 localhost provider |

### Phase 1A: 平台适配层 + ArkSIM 集成 (Week 2-3) ← 前置于实时控制层

> **策略**: 平台适配层（trait + 领域类型）先行，ArkSIM adapter 基于 trait 实现。数字孪生先于硬件。

| 任务 | 工时 | 产出 |
|------|------|------|
| P1A-01 | `openfang-platform` crate — `PlatformAdapter` trait + `AdapterRegistry` | 1d | trait 定义 + registry 骨架 |
| P1A-02 | `openfang-types/src/platform.rs` — 领域类型定义 | 0.5d | `WorldSnapshot`, `PlatformCommand`, `PlatformState` 等 |
| P1A-03 | `openfang-platform-arksim` crate 脚手架 + `prost-build` | 0.5d | proto 编译到 Rust |
| P1A-04 | `ArkSimAdapter` — 实现 `PlatformAdapter` trait (connect/disconnect/poll_state) | 2d | TCP 连接 + 帧编解码 + StateMessage → WorldSnapshot 映射 |
| P1A-05 | `command_mapper` — PlatformCommand[] → ActionsFromOutside protobuf | 1.5d | 29 种指令类型的协议翻译 |
| P1A-06 | `AdapterRegistry` 集成到 Kernel — `platform_registry()` 方法 | 1d | Agent tool 可通过 registry 路由指令 |
| P1A-07 | Agent tool 注册 — 统一 `platform_*` 命名 | 1d | `platform_get_state`, `platform_set_heading`, `platform_fire` 等 |
| P1A-08 | ArkSIM 端到端连通性测试 (adapter 模式) | 1d | 基础指令往返 + 日志验证 |

### Phase 1: 安全模型扩展 (Week 4-5)

| 任务 | 工时 | 产出 |
|------|------|------|
| P1-01 | Capability 新增变体: `WeaponArm`, `WeaponLaunch`, `WeaponAbort`, `PayloadControl`, `SelfDestruct` | 2d | 5 个新能力类型 + match 分支 + serde |
| P1-02 | CapabilityManager 注册新增武器能力 | 0.5d | manifest 解析支持 |
| P1-03 | ApprovalManager 改造为 quorum-based 多人联署 | 2d | `PendingRequest.required_signers` + `DashSet` signatures |
| P1-04 | ApprovalPolicy 新增武器审批策略 | 0.5d | `require_approval: ["weapon_arm", "weapon_launch", "self_destruct"]` |
| P1-05 | `classify_risk()` 新增武器工具映射 → RiskLevel::Critical | 0.5d | 武器操作自动判定最高风险 |
| P1-06 | SelfDestructGuard 独立模块 | 1.5d | 三方 HMAC 验证 + 时间戳过期检查 + 身份校验 |
| P1-07 | CommunicationMonitor 独立模块 | 2d | OFP Ping 探活 |
| P1-08 | DirectCommandChannel + RuleEngine 模块 | 2d | 规则引擎 + TriggerCondition + Priority + cooldown |
| P1-09 | DCC 预定义规则集配置 (config.toml) | 1d | 碰撞规避/箔条自动投放/岸基审批转发/自毁转发规则 | → LinkLost/LinkRestored 事件 → auto_approve_autonomous toggle |

### Phase 2: 实时控制层 (Week 6-9)

| 任务 | 工时 | 产出 |
|------|------|------|
| P2-01 | `openfang-platform-dds` crate — 实现 `PlatformAdapter` trait | 2d | DdsAdapter: connect/poll_state/send_commands |
| P2-02 | DDS IDL 定义 + QoS 配置 + topic 序列化 | 1.5d | nav/sensor/weapon/heartbeat topic 定义 (与 ArkSIM state mapper 对齐) |
| P2-03 | NavControl 模块 (原生 Rust) | 3d | 航线规划 + 避碰算法 + 舵机/电调指令生成 |
| P2-04 | SensorFusion 模块 (原生 Rust) | 3d | 多传感器卡尔曼滤波 + 航迹管理 + 威胁等级评估 |
| P2-05 | WeaponInterface 模块 (原生 Rust) | 2d | 武器 BIT 监控 + 发射命令封装 + 状态查询 |
| P2-06 | AudioDsp 管线 (原生 Rust FFT) | 3d | STFT/MFCC 特征提取 + 宽带/窄带分析 |
| P2-07 | ONNX 声学分类器集成 | 2d | tract-onnx 或 ort crate, 模型推理 |
| P2-08 | 仿真验证 — NavControl/SensorFusion/WeaponI/F 在 ArkSIM 中闭环测试 | 3d | 路径规划 + 航迹融合 + 射击诸元精度验证 |

### Phase 3: 可靠性增强 (Week 10-12)

| 任务 | 工时 | 产出 |
|------|------|------|
| P3-01 | Workflow 持久化 (SQLite schema v6) | 3d | `workflow_definitions` + `workflow_runs` 表 + migrate |
| P3-02 | WorkflowEngine 集成持久化 | 2d | register/create_run/completed 写 SQLite |
| P3-03 | Kernel boot 恢复未完成 workflows | 1d | boot_with_config 中 restore |
| P3-04 | 离线情报缓存队列 (PendingReport) | 2d | SQLite 表 + synced 标记 + 去重 |
| P3-05 | 增量同步引擎 | 2d | 链路恢复后 CA 批量 OFP 回传 |

### Phase 4: Agent 开发与集成 (Week 11-16)

| 任务 | 工时 | 产出 |
|------|------|------|
| P4-01 | TCA (战术指挥官 Agent) Manifest + System Prompt | 3d | 多阶段战术决策流程 |
| P4-02 | SMA (传感器管理 Agent) Manifest + System Prompt | 3d | 多传感器调度 + 电磁静默策略 |
| P4-03 | NA (导航规划 Agent) Manifest + System Prompt | 3d | 航线规划 + 避碰 + 能耗优化 |
| P4-04 | FCA (火控 Agent) Manifest + System Prompt | 2d | 射击诸元 + 武器 BIT + 安全门控协调 |
| P4-05 | CA (通信管理 Agent) Manifest + System Prompt | 2d | 带宽分配 + 中断缓存 + 增量同步 |
| P4-06 | Patrol Workflow 定义 | 1d | Sequential workflow: CA→TCA→NA→SMA 循环 |
| P4-07 | Track Workflow 定义 | 1d | FanOut: SMA + FCA + NA → TCA 融合 |
| P4-08 | Engage Workflow 定义 | 1d | TCA→ApprovalManager(block)→FCA→ApprovalManager(block)→fire |
| P4-09 | Survive Workflow 定义 | 1d | LinkLost trigger → auto_approve → energy_priority |
| P4-10 | Scuttle Workflow 定义 | 1d | SelfDestructGuard verify → broadcast abandon → terminate |

### Phase 5: 测试与交付 (Week 17-20)

| 任务 | 工时 | 产出 |
|------|------|------|
| P5-01 | 单元测试（所有新增模块） | 3d | 测试覆盖率 >90% |
| P5-02 | DDS + NavControl 集成测试 (HIL 仿真) | 3d | 闭环控制验证 |
| P5-03 | 武器门控安全测试（quorum + timeout + 继承验证） | 2d | 安全渗透测试 |
| P5-04 | 72h 压力测试 (无内存泄漏、无 agent crash 累积) | 3d | 稳定性报告 |
| P5-05 | OFP 卫星链路模拟测试 (600ms RTT + 随机丢包) | 2d | 链路质量报告 |
| P5-06 | 通信中断 72h 自主运行测试 | 2d | 自主模式验证 |
| P5-07 | 安全审计 (能力继承/审批链/审计链) | 2d | 审计报告 |
| P5-08 | 文档交付 (部署/运维/应急) | 3d | 完整文档集 |

---

## 7. 组件级详细任务

### 6.1 CommunicationMonitor

```
crates/openfang-runtime/src/comm_monitor.rs  (新增)

职责:
- 周期性 OFP Ping 岸基节点
- 连续 N 次失败 → publish EventPayload::System(SystemEvent::LinkLost)
- 链路恢复 → publish EventPayload::System(SystemEvent::LinkRestored)
- TriggerEngine 注册 LinkLost → 触发 TCA 切换 auto_approve_autonomous

配置:
  ping_interval_secs: u64 (default: 30)
  failure_threshold: u32 (default: 5)
  peer_ids: Vec<PeerId>

API:
  CommMonitor::new(kernel: Weak<OpenFangKernel>, config: CommMonitorConfig) -> Self
  CommMonitor::start(self) -> JoinHandle<()>  // 后台 tokio task
  CommMonitor::link_status() -> LinkStatus    // Connected | Degraded | Lost
```

### 6.2 Multi-Party Approval Quorum

```
crates/openfang-kernel/src/approval.rs  (修改)

改造 PendingRequest:
  struct PendingRequest {
      request: ApprovalRequest,
-     sender: tokio::sync::oneshot::Sender<ApprovalDecision>,
+     sender: tokio::sync::oneshot::Sender<ApprovalDecision>,
+     required_signers: usize,          // 最少批准人数
+     approvals: DashSet<String>,        // 已批准者列表
+     denials: DashSet<String>,          // 已否决者列表
  }

新增方法:
  ApprovalManager::add_signature(request_id, signer_id, signature) -> Result<QuorumStatus>
    - 验证 HMAC 签名
    - 添加到 approvals/denials
    - 若 approvals 达到 required_signers → 释放 oneshot(Approved)
    - 若 denials 非空 → 释放 oneshot(Denied)

QuorumStatus enum:
  Pending       // 等待更多签名
  Approved      // 达到法定人数
  Denied        // 有人否决
  InvalidSig    // 签名验证失败
```

### 6.3 SelfDestructGuard

```
crates/openfang-kernel/src/self_destruct.rs  (新增)

职责:
- 验证 OFP 自毁消息包含 ≥3 方 HMAC 签名
- 验证签名者身份在白名单内 (SC, SO, HEC)
- 验证指令时间戳偏差 < 60s (防重放)
- 常数时间 HMAC 比较（subtle crate）

API:
  SelfDestructGuard::verify(message: &SelfDestructMessage, shared_secrets: &[(String, &str)]) -> Result<(), SelfDestructError>

SelfDestructMessage:
  reason: String
  timestamp: DateTime<Utc>
  signatures: Vec<(PeerId, String)>  // (身份, HMAC)
```

### 6.4 DdsBridge

```
crates/openfang-runtime/src/dds_bridge.rs  (新增, feature = "dds")

职责:
- 封装 rustdds DataWriter/DataReader
- 提供 publish/subscribe/subscribe_callback 三种模式
- QoS 配置 (RELIABLE/BEST_EFFORT, KEEP_LAST, DEADLINE, LIVELINESS)
- DDS DomainParticipant 生命周期管理

API:
  DdsBridge::new(domain_id: u16) -> Result<Self>
  DdsBridge::create_publisher<T: Serialize>(topic: &str, qos: QosProfile) -> Result<DataWriter<T>>
  DdsBridge::create_subscriber<T: Deserialize>(topic: &str, qos: QosProfile) -> Result<DataReader<T>>
  DataWriter::write(&self, data: &T, timestamp: i64) -> Result<()>
  DataReader::take_next(&self) -> Result<Option<T>>

Topic 定义 (IDL):
  nav/NavPosition: { lat: f64, lon: f64, heading: f64, speed: f64, depth: f64, timestamp: u64 }
  nav/NavCommand: { target_heading: f64, target_speed: f64, target_depth: f64, mode: NavMode }
  sensor/RadarTrack: { id: u32, range: f64, bearing: f64, velocity: f64, confidence: f32 }
  sensor/EOImage: { width: u32, height: u32, format: ImageFormat, data: Vec<u8> }
  sensor/AcousticEvent: { freq_peak: f64, bandwidth: f64, classification: String, confidence: f32 }
  sensor/SensorCommand: { sensor_type: SensorType, mode: SensorMode, params: HashMap<String, f64> }
  weapon/WeaponStatus: { weapon_id: u32, state: WeaponState, bit_result: BitResult }
  weapon/WeaponCommand: { weapon_id: u32, command: WeaponCmdType, params: Vec<f64> }
  platform/Heartbeat: { node_id: String, uptime_secs: u64, cpu_pct: f32, mem_mb: f64 }
  platform/Alert: { severity: AlertLevel, source: String, message: String, timestamp: u64 }
```

### 6.5 Workflow 持久化

```
crates/openfang-memory/src/schema.rs  (修改)

V5 → V6 migration 新增表:
  CREATE TABLE workflow_definitions (
      id TEXT PRIMARY KEY,
      name TEXT NOT NULL,
      description TEXT NOT NULL DEFAULT '',
      steps_json TEXT NOT NULL,       -- serde_json::to_string(&workflow.steps)
      created_at TEXT NOT NULL
  );

  CREATE TABLE workflow_runs (
      id TEXT PRIMARY KEY,
      workflow_id TEXT NOT NULL REFERENCES workflow_definitions(id),
      workflow_name TEXT NOT NULL,
      input TEXT NOT NULL,
      state TEXT NOT NULL DEFAULT 'pending',  -- pending|running|completed|failed
      step_results_json TEXT NOT NULL DEFAULT '[]',
      output TEXT,
      error TEXT,
      started_at TEXT NOT NULL,
      completed_at TEXT,
      updated_at TEXT NOT NULL
  );

crates/openfang-kernel/src/workflow.rs  (修改):
  WorkflowEngine::new(memory: Arc<MemorySubstrate>)  // 新增 memory 参数
  register() → 写入 SQLite workflow_definitions
  create_run() → 写入 SQLite workflow_runs (state=pending)
  execute_run() → 更新 step_results_json 和 state
  list_workflows() → 从 SQLite 加载
  restore_runs() → kernel boot 时恢复 state=running 的 runs
```

### 6.6 PlatformAdapterRegistry

```
crates/openfang-platform/src/registry.rs  (新增)

职责:
- 管理 primary + secondary adapter 实例
- 支持 platform_id → adapter_id 路由映射
- 混合模式（仿真 + 硬件在环）多 adapter 并存

API:
  AdapterRegistry::new() -> Self
  AdapterRegistry::set_primary(adapter: Box<dyn PlatformAdapter>)
  AdapterRegistry::add_secondary(adapter: Box<dyn PlatformAdapter>)
  AdapterRegistry::route_commands(&[PlatformCommand]) -> Vec<CommandResult>
  AdapterRegistry::poll_all() -> Result<WorldSnapshot>
  AdapterRegistry::adapter_for_platform(&str) -> Option<&dyn PlatformAdapter>
```

### 6.7 ActionCollector

```
crates/openfang-runtime/src/action_collector.rs  (新增)

职责:
- Agent tool 调用产生的 PlatformCommand 暂存队列
- SimStepLoop / BackgroundExecutor 在每轮决策结束后 drain 并发送

API:
  ActionCollector::new() -> Self
  ActionCollector::push(command: PlatformCommand)
  ActionCollector::drain() -> Vec<PlatformCommand>
  ActionCollector::pending_count() -> usize
```

### 6.8 离线情报缓存

```
crates/openfang-memory/src/schema.rs  (修改)

V6 migration 新增表:
  CREATE TABLE pending_reports (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      report_type TEXT NOT NULL,       -- contact|threat|bda|status
      payload_json TEXT NOT NULL,
      priority INTEGER NOT NULL DEFAULT 0,  -- 0=normal, 1=urgent, 2=critical
      created_at TEXT NOT NULL,
      synced INTEGER NOT NULL DEFAULT 0,   -- 0=pending, 1=synced
      synced_at TEXT,
      dedup_hash TEXT NOT NULL,            -- SHA256(payload) for dedup
      UNIQUE(dedup_hash)
  );

crates/openfang-runtime/src/report_queue.rs  (新增):
  ReportQueue::enqueue(report_type, payload, priority) → dedup check → insert
  ReportQueue::pending() → Vec<PendingReport> (synced=0, ordered by priority DESC, created_at ASC)
  ReportQueue::mark_synced(ids: &[i64]) → UPDATE synced=1, synced_at=NOW
  ReportQueue::sync_all(ca_agent_id) → OFP send pending reports → mark synced
```

---

## 8. 代码量估算（修正版）

| 组件 | 原 PRD 估计 | 修正后估计 | 差异 |
|------|:--------:|:-------:|:----:|
| Phase 0: Feature flags + 条件编译 | - | ~200 行 | 新增 |
| Phase 1A-01: openfang-platform (trait + registry) | - | ~300 行 | 新增 trait crate |
| Phase 1A-02: platform.rs 领域类型 | - | ~400 行 | WorldSnapshot/PlatformCommand 等 |
| Phase 1A-03~08: openfang-platform-arksim (adapter 实现) | (原 ~1800) | **~1200 行** | bridge/state_mapper/command_mapper + prost gen |
| Phase 1A 小计 | - | **~1900 行** | (trait layer reshapes previous estimate) |
| Phase 1-01: Capability 变体 (5个) | 200 | **250** | +match/serde/manifest |
| Phase 1-02: Multi-party Quorum 改造 | - | **300** | 遗漏 |
| Phase 1-03: SelfDestructGuard | 100 | **250** | +独立模块+测试 |
| Phase 1-04: CommunicationMonitor | - | **300** | 遗漏 |
| Phase 1-05: ApprovalPolicy 武器配置 | (含在 200) | 50 | 配置项 |
| Phase 1-08~09: DirectCommandChannel + RuleEngine | - | ~400 行 | 规则引擎 + TriggerCondition + predef rules |
| Phase 2-01: openfang-platform-dds (adapter 实现) | - | **500** | DdsAdapter: impl PlatformAdapter + publisher/subscriber |
| Phase 2-02: DDS Topic IDL + QoS | (含在 500) | 300 | 与 ArkSIM state mapper 语义对齐 |
| Phase 2-03: NavControl (原 WASM→Rust) | (含在 500) | 600 | 原生层重写 |
| Phase 2-04: SensorFusion (原 WASM→Rust) | (含在 500) | 800 | 原生层重写 |
| Phase 2-05: WeaponInterface (原 WASM→Rust) | (含在 500) | 400 | 原生层重写 |
| Phase 2-06: AudioDsp 管线 (原 WASM→Rust) | 300 (host_func) | **400** | 原生层重写 |
| Phase 2-07: ONNX 分类器 | (含在 300) | 250 | 独立模块 |
| Phase 2-08: 仿真闭环验证 | - | — | (测试工时，非代码) |
| Phase 3-01: Workflow 持久化 | - | **400** | 遗漏 |
| Phase 3-02: PendingReport 队列 | - | **250** | 遗漏 |
| Phase 3-03: 增量同步引擎 | - | 200 | 遗漏 |
| Phase 4: Agent Manifests + Workflows | - | ~2500 | 新增 (5 Agent + 5 Workflow) |
| Phase 5: 测试代码 | - | ~3000 | 单元+集成+压力 |
| UMAA 对齐 (新增) | - | **~2430 行** | HealthMonitor/ORA/Nav分离/TrackMgr/MissionConfig/Package/22 tools |
| UAV 集群协同 (新增) | - | **~2500 行** | UavState/13 cmd/FMA Agent/5 Workflow/DCC rules/tools |
| **总计** | **~1100 行** | **~16,930 行** | **约 15 倍** |

> 注：原 PRD 估计仅覆盖 Capability + WASM host functions + rustdds。修正版覆盖全部遗漏组件 + 安全改造 + 测试。

---

## 9. 验证门禁

每个 Phase 完成后必须通过以下门禁：

```bash
# 1. 编译（tactical variant）
cargo build --no-default-features -F tactical --lib

# 2. 全量测试
cargo test --workspace

# 3. 零 clippy 警告
cargo clippy --workspace --all-targets -- -D warnings

# 4. 体积验证
ls -lh target/release/openfang  # < 20MB
```

Phase 5 额外门禁：
```bash
# 5. 72h 压力测试
# 6. HIL 仿真通过
# 7. 安全渗透测试通过
```

---

## 10. 风险与缓解（更新）

| 风险 ID | 风险描述 | 概率 | 影响 | 等级 | 缓解措施 |
|---------|---------|:---:|:---:|:---:|---------|
| R-01 | Qwen 3.6 27B 在 Jetson 上推理速度不满足战术时限 | 中 | 高 | 🔴 | 预量化基准；备选 Qwen3.6-35B-A3B MoE |
| R-02 | LLM 工具调用幻觉致武器误操作 | 低 | 致命 | 🔴 | Multi-party quorum 硬门控；武器操作必须 ≥2 人批准 |
| R-03 | 卫星链路延迟/中断致指令丢失 | 高 | 中 | 🟡 | CommMonitor → 自主模式 + 情报缓存 + 增量同步 |
| R-04 | 敌方电子干扰致通信完全中断 | 中 | 高 | 🔴 | 全自主模式；数传电台备份；CommMonitor 分级检测 |
| R-05 | rustdds 与 FastDDS 互操作兼容问题 | 低 | 中 | 🟡 | 早期集成测试；eCAL 备选方案 |
| R-06 | Multi-party quorum 改造引入 ApprovalManager 回归 | 中 | 高 | 🔴 | 保留原始测试；新增 quorum 单元测试；渐进式 refactor |
| **R-07** | **WASM→Rust 原生实时层改动量超预期** | **中** | **中** | 🟡 | 先做 DdsBridge + NavControl PoC (Phase 2 前 3 天) 验证延迟 |
| **R-08** | **Workflow 持久化回退破坏现有 MemorySubstrate API** | **低** | **中** | 🟡 | Schema v6 新增表不修改现有；revert 只需删除新增表 |
| R-09 | Jetson 功耗/散热超限 | 中 | 中 | 🟡 | 功耗预算管控；动态降频；被动散热 |
| **R-10** | **DCC 规则误触发致错误操作（如虚警箔条投放）** | **中** | **中** | 🟡 | cooldown + max_fires 限制；Agent 事后禁用；RuleSet 白名单仅防御性动作 |
| **R-11** | **DCC 绕过导致 LLM 丧失态势感知** | **低** | **中** | 🟢 | 所有 DCC 动作发布 EventBus；LLM tick 时在状态摘要中看到 |

> **R-07** 和 **R-08** 为本次审阅新增风险。

---

## 11. ArkSIM 仿真集成

> **架构定位**: 本章的 `ArkSimBridge` + `StateMapper` + `ActionBuilder` 对应 `openfang-platform-arksim` crate 的实现细节。
> `SimStepLoop` 对应 `ArkSimAdapter::poll_state()` + `send_commands()` 的循环调用模式。
> Agent 层通过 `PlatformAdapter` trait 调用，不直接使用本章的底层类型。



### 11.1 协议分析

ArkSIM 使用双向 protobuf 通道，定义在 `protobuf/` 目录：

```
protobuf/
├── afsimproto.proto          # ArkSIM 原生状态消息 + 平台/传感器/武器定义
└── AfsimActionsProto.proto   # 外部控制指令 (import afsimproto)
```

**输入通道（ArkSIM → Agent）：** `StateMessage`

```
StateMessage
├── platforms[]           → PlatformState (位置/速度/姿态/燃油/损伤/航迹)
│   ├── locationLLA/WCS  → [lat, lon, alt] / [x, y, z]
│   ├── velocityNED/ECS  → [vn, ve, vd] / [vx, vy, vz]
│   ├── orientationNED   → [heading, pitch, roll] (radians)
│   ├── tracks[]         → TrackState (ID/类型/敌我/位置/速度/距离/方位/质量)
│   ├── weapons{}        → map<string, WeaponState> (弹药名称→类型/剩余数量)
│   ├── SensorStates[]   → SensorState (ID/类型/频率/带宽/模式/视场/损伤)
│   ├── JammerWeapons[]  → JammerWeapon (name/host_id/yaw/pitch/beam[])
│   ├── auxdata[]        → AuxData (自定义键值)
│   ├── fuel/maxFuel     → 燃油当前/最大 (kg)
│   ├── damageFactor     → 损伤系数 (0.0-1.0)
│   ├── currentTarget    → 当前目标名称
│   └── spatialDomain    → land/air/surface/subsurface/space
├── Weapons[]            → ActiveWeaponState (飞行中武器: 位置/速度/目标/损伤)
├── time                 → 仿真时间 (秒)
└── endTime              → 仿真结束时间
```

**输出通道（Agent → ArkSIM）：** `ActionsFromOutside`

```
ActionsFromOutside — 29 种指令类型
├── 平台控制 (6): SetControl/ReleaseControl/DesiredHeading/DesiredAltitude/DesiredVelocity/GoToLocation/FollowRoute
├── 传感器 (4): TurnOnSensor/TurnOffSensor/ChangeSensorMode/GetSensorCurrentMode
├── 武器 (4): FireAtTarget/FireSlavoAtTarget/FireChaff/UpdateTarget
├── 电子战 (3): StartJamming/StopJamming/ChangeJammingMode
├── 通信 (3): TurnOnComm/TurnOffComm/SendMsgToPlatform/SendMsgToCommandChain
├── 导弹 (5): MissileWaypoint/MissileLoiter/MissileTerminalActivate/MissileRetarget/MissileCoordinationSync
├── 指挥链 (1): ChangeCommander
└── Aux (1): AfsimAuxCommand (透传键值指令)
```

### 11.2 集成架构

```
┌──────────────────────────────────────────────────────────────┐
│                    OpenFang Agent OS                          │
│                                                              │
│  ┌──────────┐   ┌──────────────┐   ┌────────────────────┐   │
│  │ TCA      │   │ Workflow     │   │ Trigger Engine     │   │
│  │ Agent    │   │ Engine       │   │ (events)           │   │
│  └────┬─────┘   └──────┬───────┘   └────────┬───────────┘   │
│       │                │                     │               │
│       │        ┌───────┴──────────┐          │               │
│       └───────→│  EventBus        │←─────────┘               │
│                │  (state_update)  │                           │
│                └───────┬──────────┘                           │
│                        │                                      │
│  ┌─────────────────────┼──────────────────────────────────┐  │
│  │  openfang-arksim    │                                  │  │
│  │                     ▼                                  │  │
│  │  ┌─────────────────────────────────────────────┐      │  │
│  │  │            SimStepLoop                       │      │  │
│  │  │  ┌───────────┐  ┌──────────┐  ┌──────────┐ │      │  │
│  │  │  │ recv      │→ │ map to   │→ │ publish  │ │      │  │
│  │  │  │ StateMsg  │  │ internal │  │ Event    │ │      │  │
│  │  │  └───────────┘  └──────────┘  └──────────┘ │      │  │
│  │  │                    Agent loop decides       │      │  │
│  │  │  ┌───────────┐  ┌──────────┐  ┌──────────┐ │      │  │
│  │  │  │ collect   │← │ build    │← │ agent    │ │      │  │
│  │  │  │ actions   │  │ Actions  │  │ decisions│ │      │  │
│  │  │  └─────┬─────┘  └──────────┘  └──────────┘ │      │  │
│  │  └────────┼────────────────────────────────────┘      │  │
│  │           │ send ActionsFromOutside                   │  │
│  └───────────┼──────────────────────────────────────────┘  │
└──────────────┼──────────────────────────────────────────────┘
               │ TCP (length-prefixed protobuf frames)
               ▼
┌──────────────────────────────────────────────────────────────┐
│                     ArkSIM Engine                             │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐                 │
│  │ Physics  │   │ Sensors  │   │ Weapons  │                 │
│  │ Engine   │   │ Models   │   │ Models   │                 │
│  └────┬─────┘   └────┬─────┘   └────┬─────┘                 │
│       └───────────────┼──────────────┘                       │
│                       ▼                                      │
│              StateMessage (per tick)                         │
└──────────────────────────────────────────────────────────────┘
```

### 11.3 Crate 设计：`openfang-arksim`

```
crates/openfang-arksim/
├── Cargo.toml              # prost + prost-types + tokio
├── build.rs                # prost-build: compile proto → src/gen/
├── src/
│   ├── lib.rs              # crate root + re-exports
│   ├── gen/mod.rs          # generated prost code (gitignored)
│   ├── bridge.rs           # ArkSimBridge: TCP connect/send/recv
│   ├── frame.rs            # 长度前缀帧编码/解码
│   ├── state_mapper.rs     # StateMessage → PlatformSnapshot/Waypoint/SensorReading
│   ├── action_builder.rs   # ActionBuilder: fluent API 构建 ActionsFromOutside
│   ├── sim_loop.rs         # SimStepLoop: 仿真步进调度
│   └── types.rs            # 内部领域类型 (与 DDS topic 类型对齐)
```

#### 9.3.1 `bridge.rs` — TCP 连接与帧协议

```rust
/// 长度前缀帧格式: [4 bytes LE u32 payload_len] [protobuf payload]
pub struct ArkSimBridge {
    stream: TcpStream,
    read_buf: Vec<u8>,
}

impl ArkSimBridge {
    /// 连接到 ArkSIM 引擎
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;  // 禁用 Nagle，低延迟
        Ok(Self { stream, read_buf: vec![0u8; 65536] })
    }

    /// 接收 StateMessage（阻塞直到完整帧到达）
    pub async fn recv_state(&mut self) -> Result<StateMessage> {
        let payload = self.read_frame().await?;
        Ok(StateMessage::decode(payload.as_slice())?)
    }

    /// 发送 ActionsFromOutside
    pub async fn send_actions(&mut self, actions: &ActionsFromOutside) -> Result<()> {
        let payload = actions.encode_to_vec();
        self.write_frame(&payload).await?;
        Ok(())
    }
}
```

#### 9.3.2 `state_mapper.rs` — 仿真状态到内部类型

```rust
/// 平台快照 — 与 DDS nav/NavPosition topic 语义对齐
pub struct PlatformSnapshot {
    pub name: String,
    pub side: String,             // "blue" / "red"
    pub domain: String,           // surface / air / subsurface
    pub lat: f64, pub lon: f64, pub alt: f64,
    pub heading_rad: f64,         // 北向顺时针, radians
    pub pitch_rad: f64,
    pub speed_ms: f64,            // 地速 m/s
    pub fuel_pct: f64,            // 0.0-1.0
    pub damage: f64,              // 0.0-1.0
    pub tracks: Vec<TrackInfo>,
    pub sensors: Vec<SensorInfo>,
    pub weapons: Vec<WeaponInfo>,
    pub current_target: Option<String>,
}

impl StateMapper {
    pub fn from_state_message(msg: &StateMessage) -> SimulationState {
        SimulationState {
            time: msg.time,
            platforms: msg.platforms.iter()
                .map(|p| Self::map_platform(p))
                .collect(),
            active_weapons: msg.weapons.iter()
                .map(|w| Self::map_active_weapon(w))
                .collect(),
        }
    }
}
```

#### 9.3.3 `action_builder.rs` — 流畅 API 构建控制指令

```rust
/// 构建器模式 — 对应 ArkSIM 29 种指令
pub struct ActionBuilder {
    actions: ActionsFromOutside,
}

impl ActionBuilder {
    pub fn new() -> Self { /* ... */ }

    /// 设置平台期望航向
    pub fn set_desired_heading(
        &mut self, platform: &str, heading_rad: f64,
        velocity_ms: Option<f64>, turn_direction: Option<TurnDir>,
    ) -> &mut Self { /* 构建 DesiredHeading 消息 */ }

    /// 设置平台期望速度
    pub fn set_desired_velocity(
        &mut self, platform: &str, velocity_ms: f64, accel: f64,
    ) -> &mut Self { /* ... */ }

    /// 设置航路
    pub fn follow_route(
        &mut self, platform: &str, waypoints: &[(f64, f64, f64, f64)],
        // (lat, lon, alt, speed)
    ) -> &mut Self { /* ... */ }

    /// 转向传感器
    pub fn turn_on_sensor(&mut self, platform: &str, sensor_id: &str) -> &mut Self { /* ... */ }

    /// 切换传感器模式
    pub fn change_sensor_mode(
        &mut self, platform: &str, sensor_id: &str, mode: &str,
    ) -> &mut Self { /* ... */ }

    /// 开火
    pub fn fire_at_target(
        &mut self, platform: &str, weapon_id: &str, track_id: &str,
    ) -> &mut Self { /* ... */ }

    /// 齐射
    pub fn fire_salvo_at_target(
        &mut self, platform: &str, weapon_id: &str, track_id: &str, salvo_size: u32,
    ) -> &mut Self { /* ... */ }

    /// 开始干扰
    pub fn start_jamming(
        &mut self, platform: &str, jammer_id: &str,
        freq_hz: f64, bandwidth_hz: f64, track_id: &str,
    ) -> &mut Self { /* ... */ }

    /// 停止干扰
    pub fn stop_jamming(
        &mut self, platform: &str, jammer_id: &str,
    ) -> &mut Self { /* ... */ }

    /// 发射箔条
    pub fn fire_chaff(
        &mut self, platform: &str, weapon_id: &str, count: u32, interval_s: f64,
    ) -> &mut Self { /* ... */ }

    /// 更换指挥链
    pub fn change_commander(
        &mut self, platform: &str, new_commander: &str,
    ) -> &mut Self { /* ... */ }

    /// 发送消息（艇间通信）
    pub fn send_msg_to_platform(
        &mut self, from_id: &str, to_id: &str, message: &str,
    ) -> &mut Self { /* ... */ }

    /// 构建最终 protobuf 消息
    pub fn build(self) -> ActionsFromOutside { self.actions }
}
```

#### 9.3.4 `sim_loop.rs` — 仿真步进调度

```rust
/// 仿真步进模式：每步 = recv StateMessage → agent 决策 → send ActionsFromOutside
pub struct SimStepLoop {
    bridge: ArkSimBridge,
    state_mapper: StateMapper,
    step_count: u64,
}

impl SimStepLoop {
    /// 运行单步仿真
    /// 1. 接收 ArkSIM 状态
    /// 2. 发布 EventPayload::SimulationState 到 EventBus
    /// 3. 等待 TCA Agent 决策（通过 BackgroundExecutor tick 触发）
    /// 4. 从 ActionCollector 收集所有挂起的动作
    /// 5. 发送 ActionsFromOutside 回 ArkSIM
    pub async fn step(&mut self, kernel: &Arc<OpenFangKernel>) -> Result<StepResult> {
        // 1. 接收状态
        let state_msg = self.bridge.recv_state().await?;
        let sim_state = self.state_mapper.from_state_message(&state_msg);

        // 2. 发布事件
        kernel.event_bus.publish(Event::new(EventPayload::SimulationState(sim_state)));

        // 3. 触发 TCA agent tick (同步等待本轮决策完成)
        let tca_id = kernel.registry.find_by_name("tca")?;
        let response = kernel.send_to_agent(tca_id, 
            format!("[SIM_TIME={:.1}s] Evaluate tactical situation and decide actions.",
                state_msg.time)
        ).await?;

        // 4. 收集动作（由 agent tool calls 填充）
        let actions = kernel.action_collector.drain()?;

        // 5. 发送
        self.bridge.send_actions(&actions).await?;
        self.step_count += 1;

        Ok(StepResult { step: self.step_count, time: state_msg.time })
    }
}
```

### 11.4 Agent Tools 注册

Agent 通过以下 tool 与仿真环境交互：

| Tool 名称 | 功能 | 对应 ArkSIM 指令 |
|-----------|------|:---:|
| `sim_get_state` | 获取当前仿真状态摘要（文本化） | — (只读) |
| `sim_set_heading` | 设置平台航向 | E_SetDesiredHeading |
| `sim_set_speed` | 设置平台速度 | E_SetDesiredVelocity |
| `sim_set_altitude` | 设置平台高度 | E_SetDesiredAltitude |
| `sim_goto_location` | 导航到指定 LLA 坐标 | E_GoToLocation |
| `sim_follow_route` | 沿航路点序列航行 | E_FollowRoute |
| `sim_sensor_on` | 开启传感器 | E_TurnOnSensor |
| `sim_sensor_off` | 关闭传感器 | E_TurnOffSensor |
| `sim_sensor_mode` | 切换传感器模式 | E_ChangeSensorMode |
| `sim_fire_at_target` | 对目标开火 | E_FireAtTarget |
| `sim_fire_salvo` | 对目标齐射 | E_FireSlavoAtTarget |
| `sim_fire_chaff` | 发射箔条诱饵 | E_FireChaff |
| `sim_jam_start` | 开启电子干扰 | E_StartJamming |
| `sim_jam_stop` | 停止电子干扰 | E_StopJamming |
| `sim_jam_mode` | 切换干扰模式 | E_ChangeJammingMode |
| `sim_send_message` | 向其他平台发送消息 | E_SendMsgToPlatform |
| `sim_change_commander` | 更换指挥链上级 | E_ChangeCommander |

所有工具的实现模式：

```rust
// 示例: sim_set_heading
fn tool_sim_set_heading(
    kernel: &Arc<OpenFangKernel>,
    agent_id: AgentId,
    params: &serde_json::Value,
) -> Result<String> {
    let platform = params["platform"].as_str().unwrap();
    let heading_deg = params["heading_deg"].as_f64().unwrap();
    
    // 构建 ArkSIM 指令
    let mut builder = kernel.arksim.action_builder();
    builder.set_desired_heading(
        platform,
        heading_deg.to_radians(),
        params["speed_ms"].as_f64(),
        None,
    );
    
    // 排入待发送队列（SimStepLoop 在 step 末尾统一发送）
    kernel.arksim.action_queue().push(builder.build());
    
    Ok(format!("Heading set to {}° for {}", heading_deg, platform))
}
```

### 11.5 仿真驱动开发流程

```
Phase 1A (Week 2-3):   ArkSim Bridge — 连通性
                       ↓
Phase 2  (Week 6-9):   Rust 原生控制算法在 ArkSIM 中验证
                       ├── NavControl  → 通过 sim_set_heading/speed 验证路径规划
                       ├── SensorFusion → StateMessage.tracks 验证多传感器融合
                       └── WeaponI/F   → sim_fire_at_target 验证射击诸元
                       ↓
Phase 4  (Week 13-18): Agent 闭环在 ArkSIM 中完整测试
                       ├── Patrol Workflow 在仿真中运行
                       ├── Track Workflow  验证目标识别链
                       └── Engage Workflow 验证火力链 + quorum
                       ↓
Phase 5  (Week 19-22): HIL 仿真 → DDS 硬件迁移
```

### 11.6 配置示例

```toml
# openfang.toml — tactical profile
[arksim]
enabled = true
host = "127.0.0.1"
port = 5000
step_timeout_secs = 30      # 单步超时
agent_tick_timeout_secs = 8 # Agent LLM 推理超时 (NFR-R04)

# Agent manifest 指定仿真模式
[[agent]]
name = "tca"
schedule = { mode = "sim_step" }  # 仿真步进驱动（非 Continuous）
```

---

```

---

> **文档维护**: 本文件随 Phase 推进持续更新。每个 Phase 完成后更新进度状态。

| 文件 | 操作 | Phase |
|------|------|:----:|
| `crates/openfang-platform/Cargo.toml` | **新增** — Platform Adapter trait crate | P1A |
| `crates/openfang-platform/src/lib.rs` | **新增** — PlatformAdapter trait | P1A |
| `crates/openfang-platform/src/registry.rs` | **新增** — AdapterRegistry | P1A |
| `crates/openfang-platform/src/command.rs` | **新增** — PlatformCommand enum | P1A |
| `crates/openfang-platform/src/capabilities.rs` | **新增** — PlatformCapabilities 位图 | P1A |
| `crates/openfang-types/src/platform.rs` | **新增** — 领域类型 (WorldSnapshot 等) | P1A |
| `protobuf/*.proto` | **新增** (已存在) — ArkSIM 协议定义 | P1A |
| `crates/openfang-platform-arksim/Cargo.toml` | **新增** — ArkSIM adapter crate | P1A |
| `crates/openfang-platform-arksim/build.rs` | **新增** — prost 编译脚本 | P1A |
| `crates/openfang-platform-arksim/src/lib.rs` | **新增** — ArkSimAdapter | P1A |
| `crates/openfang-platform-arksim/src/bridge.rs` | **新增** — TCP 帧处理 | P1A |
| `crates/openfang-platform-arksim/src/state_mapper.rs` | **新增** — StateMessage→WorldSnapshot | P1A |
| `crates/openfang-platform-arksim/src/command_mapper.rs` | **新增** — PlatformCommand→protobuf | P1A |
| `crates/openfang-runtime/src/platform_tools.rs` | **新增** — Agent platform_* tools | P1A |
| `Cargo.toml` | 修改 — 添加 feature flags | P0 |
| `crates/*/Cargo.toml` | 修改 — 条件依赖 | P0 |
| `crates/openfang-types/src/capability.rs` | 修改 — 新增武器能力变体 | P1 |
| `crates/openfang-kernel/src/approval.rs` | 修改 — quorum 改造 | P1 |
| `crates/openfang-kernel/src/self_destruct.rs` | **新增** | P1 |
| `crates/openfang-runtime/src/comm_monitor.rs` | **新增** | P1 |
| `crates/openfang-runtime/src/direct_channel.rs` | **新增** — DirectCommandChannel + RuleEngine | P1 |
| `crates/openfang-runtime/src/direct_channel/conditions.rs` | **新增** — TriggerCondition 评估器 | P1 |
| `crates/openfang-platform-dds/Cargo.toml` | **新增** — DDS adapter crate | P2 |
| `crates/openfang-platform-dds/src/lib.rs` | **新增** — DdsAdapter impl | P2 |
| `crates/openfang-platform-dds/src/publisher.rs` | **新增** — Command→DDS | P2 |
| `crates/openfang-platform-dds/src/subscriber.rs` | **新增** — DDS→WorldSnapshot | P2 |
| `crates/openfang-runtime/src/nav_control.rs` | **新增** | P2 |
| `crates/openfang-runtime/src/sensor_fusion.rs` | **新增** | P2 |
| `crates/openfang-runtime/src/weapon_iface.rs` | **新增** | P2 |
| `crates/openfang-runtime/src/audio_dsp.rs` | **新增** | P2 |
| `crates/openfang-runtime/src/action_collector.rs` | **新增** — PlatformCommand 暂存队列 | P1A |
| `crates/openfang-runtime/src/report_queue.rs` | **新增** | P3 |
| `crates/openfang-memory/src/schema.rs` | 修改 — v5→v6 migration | P3 |
| `crates/openfang-kernel/src/workflow.rs` | 修改 — 持久化集成 | P3 |
| `crates/openfang-runtime/src/kernel_handle.rs` | 修改 — 新增 platform_registry() 方法 | P1A |
| `crates/openfang-kernel/src/kernel.rs` | 修改 — 集成新组件 | P1-P4 |
| `agents/tca/manifest.toml` | **新增** | P4 |
| `agents/sma/manifest.toml` | **新增** | P4 |
| `agents/na/manifest.toml` | **新增** | P4 |
| `agents/fca/manifest.toml` | **新增** | P4 |
| `agents/ca/manifest.toml` | **新增** | P4 |
| `docs/plan-tactical-vessel.md` | **本文件** | — |

---


---


---

## 12. UMAA 架构对齐与完善

> **参考标准**: Unmanned Maritime Autonomy Architecture (UMAA) v5.0 — 
> 美国海军无人海事系统标准化架构。本节将 OpenFang 组件映射到 UMAA 服务模型，
> 识别差距，并提出完善方案。

### 12.1 UMAA 服务映射

UMAA 定义了一套标准的无人系统服务体系。以下是 OpenFang 当前组件与 UMAA 服务的对应关系：

| UMAA 服务 | OpenFang 对应 | 匹配度 | 差距 |
|-----------|-------------|:---:|------|
| **Mission Management** | TCA Agent + Workflow Engine | ★★★★☆ | 缺少任务状态机 (UMAA MissionStatus 更细粒度) |
| **Route Planning** | NA Agent + NavControl | ★★★☆☆ | UMAA 区分 Route Planning 与 Vehicle Control 两层 |
| **Vehicle Control** | DDS Adapter (publisher) | ★★★☆☆ | 缺少标准化 VehicleCommand 接口抽象 |
| **Sensor Management** | SMA Agent + SensorFusion | ★★★☆☆ | 缺少 SensorTasking 模式 (UMAA: search/track/classify/cue) |
| **Weapon Management** | FCA Agent + WeaponInterface | ★★★☆☆ | 缺少 MissionPackage 概念 (可互换武器载荷) |
| **Payload Management** | FMA Agent (UAV) | ★★☆☆☆ | UAV 载荷作为整体，未细化为可互换 package |
| **Communications** | CA Agent + CommMonitor | ★★★★☆ | 缺少 LinkQuality 标准化报告 |
| **Track Management** | SensorFusion | ★★★☆☆ | 缺少标准化 TrackCorrelation / TrackIdentification |
| **Data Recording** | Merkle AuditLog + MemorySubstrate | ★★★★☆ | 缺少按任务组织的 DataProduct 模型 |
| **Health Monitoring** | Supervisor | ★★☆☆☆ | 缺少标准化 HealthReport / BIT 结果上报 |
| **Configuration Management** | config.toml | ★☆☆☆☆ | 缺少运行时配置切换、MissionConfig 概念 |
| **Operational Restrictions** | ApprovalManager + DCC | ★★★☆☆ | 缺少 Geofence / ROE / SpeedLimit 标准化约束 |
| **Navigation Service** | NavControl (隐含) | ★★☆☆☆ | 未分离位置估计(DeadReckoning/GPS fusion)与航线规划 |
| **Support Services** | 分散在各模块 | ★★☆☆☆ | 缺少统一的 Logging / TimeSync / Watchdog 服务 |

### 13.2 需新增的 UMAA 驱动组件

#### 12.2.1 Health Monitoring Service

当前 `Supervisor` 仅做 agent crash 计数和重启。UMAA 要求系统级健康监控：

```rust
// crates/openfang-runtime/src/health_monitor.rs  (新增)

pub struct HealthMonitor {
    components: DashMap<String, ComponentHealth>,  // 按组件跟踪健康
    bit_scheduler: BitScheduler,                    // 周期性 BIT (Built-In Test)
    alert_thresholds: HealthThresholds,
}

pub struct ComponentHealth {
    pub status: HealthStatus,          // Nominal / Degraded / Failed / Unknown
    pub last_bit_result: Option<BitResult>,
    pub error_count_since_boot: u32,
    pub uptime_s: f64,
    pub resource_usage: ResourceUsage, // CPU/Memory/Disk
}

pub enum HealthStatus {
    Nominal,        // 正常
    Degraded,       // 降级运行 (如传感器部分失效)
    Inoperable,     // 不可用 (如武器系统离线)
    Maintenance,    // 维护中
}

pub struct BitResult {
    pub component: String,
    pub test_name: String,
    pub passed: bool,
    pub fault_code: Option<String>,
    pub timestamp: f64,
    pub recommended_action: Option<String>,  // "Restart", "SwitchToBackup", "AbortMission"
}

// UMAA 兼容的 HealthReport
pub struct HealthReport {
    pub platform_id: String,
    pub overall_status: HealthStatus,
    pub components: Vec<ComponentHealth>,
    pub active_alerts: Vec<Alert>,
    pub generated_at: f64,
}
```

**新增 Agent Tools**:
- `platform_get_health_report` — 获取系统健康报告
- `platform_run_bit` — 触发指定组件 BIT
- `platform_get_component_status` — 查询单组件状态

#### 12.2.2 Operational Restrictions Manager

UMAA 要求标准化约束管理：交战规则 (ROE)、地理围栏 (Geofence)、速度/深度限制：

```rust
// crates/openfang-runtime/src/op_restrictions.rs  (新增)

pub struct OperationalRestrictions {
    roe: RulesOfEngagement,
    geofences: Vec<Geofence>,
    platform_limits: PlatformLimits,
    env_constraints: EnvironmentConstraints,
}

pub struct RulesOfEngagement {
    pub weapon_release_authority: WeaponReleaseLevel,  // Hold / SelfDefense / Commander / Free
    pub engagement_zones: Vec<EngagementZone>,           // 可交战区域
    pub restricted_targets: Vec<String>,                 // 禁止攻击目标类型
    pub warning_before_engage: bool,
    pub self_defense_threshold: ThreatLevel,
}

pub enum WeaponReleaseLevel {
    WeaponsHold,       // 禁止任何武器使用
    WeaponsTight,      // 仅自卫 (需人工确认)
    WeaponsFree,       // 指挥官授权自由攻击
}

pub struct Geofence {
    pub name: String,
    pub boundary: Vec<(f64, f64)>,     // LLA 多边形
    pub restriction: GeofenceType,     // KeepIn / KeepOut / AltitudeCeiling / SpeedLimit
    pub violation_action: ViolationAction, // Warn / AutoCorrect / AbortMission
}

pub struct PlatformLimits {
    pub max_speed_ms: f64,
    pub max_depth_m: f64,
    pub min_altitude_m: f64,           // UAV
    pub max_acceleration_ms2: f64,
    pub endurance_limit_s: f64,
}
```

**新增 Agent Tools**:
- `platform_get_roe` — 获取当前交战规则
- `platform_set_roe_level` — TCA 调整武器释放级别
- `platform_get_geofence_status` — 查询围栏状态
- `platform_check_geofence_violation` — 检查当前位置是否违规

**新增 DCC 规则**:
```toml
[[direct_command_rules]]
name = "auto_hold_on_geofence_violation"
condition = { type = "GeofenceViolation", violation = "KeepOut" }
action = { type = "SetSpeed", speed_ms = 0 }
priority = "Critical"
cooldown_ms = 1000

[[direct_command_rules]]
name = "auto_abort_on_roe_change_to_hold"
condition = { type = "ROEChange", new_level = "WeaponsHold" }
action = { type = "WeaponSafeAll" }
priority = "Critical"
cooldown_ms = 0
```

#### 12.2.3 Navigation Service (独立)

UMAA 将导航服务从航线规划中分离：

```rust
// crates/openfang-runtime/src/navigation.rs  (新增/从 NavControl 分离)

pub struct NavigationService {
    position_estimate: PositionEstimate,
    dead_reckoning: DeadReckoning,
    gps_receiver: Option<GpsData>,
    ins_data: Option<InsData>,
}

pub struct PositionEstimate {
    pub lat: f64, pub lon: f64, pub alt: f64,
    pub heading_deg: f64,
    pub speed_ms: f64,
    pub accuracy: PositionAccuracy,   // CEP 圆概率误差 (米)
    pub source: NavSource,            // GPS / INS / DeadReckoning / VisualOdometry
    pub timestamp: f64,
    pub is_valid: bool,
}

impl NavigationService {
    /// 多源融合位置估计
    pub fn fuse(&mut self, gps: Option<GpsData>, ins: Option<InsData>) -> PositionEstimate {
        // GPS 有效 → 直接使用 + 校准 INS
        // GPS 丢失 → INS 积分 (累计误差递增)
        // GPS+INS 皆丢 → 纯航位推算 (DeadReckoning)
    }
}
```

**新增 Agent Tools**:
- `platform_get_nav_status` — 获取导航状态 (精度/源/有效性)
- `platform_set_nav_source` — 切换导航源 (GPS/INS/DR)

#### 12.2.4 Track Management Service (标准化)

UMAA 定义标准化航迹管理接口 (TrackCorrelation / TrackIdentification / TrackFusion)：

```rust
// crates/openfang-runtime/src/track_manager.rs  (从 SensorFusion 分离)

pub struct TrackManager {
    local_tracks: HashMap<String, Track>,       // 本地航迹
    remote_tracks: HashMap<String, RemoteTrack>, // 友邻平台航迹 (OFP)
    correlation_matrix: TrackCorrelationMap,
}

impl TrackManager {
    /// 航迹关联 — 将新 sensor contact 关联到已有 track 或创建新 track
    pub fn correlate(&mut self, contact: SensorContact) -> CorrelationResult;

    /// 航迹识别 — 基于多传感器特征融合判定目标类型
    pub fn identify(&mut self, track_id: &str) -> IdentificationResult;

    /// 航迹融合 — 将远程 track (OFP) 与本地 track 进行数据融合
    pub fn fuse_remote(&mut self, remote: RemoteTrack) -> FusionResult;

    /// 航迹质量管理
    pub fn evaluate_quality(&self, track_id: &str) -> TrackQuality;
}

pub struct TrackQuality {
    pub existence_prob: f64,         // 航迹存在概率
    pub identification_confidence: f64, // 识别置信度
    pub position_accuracy: f64,      // 位置精度 (CEP 米)
    pub age_s: f64,                  // 航迹年龄
    pub update_rate_hz: f64,         // 更新频率
    pub staleness: TrackStaleness,
}
```

**新增 Agent Tools**:
- `platform_get_track` — 获取指定 track 详细信息
- `platform_get_track_correlation` — 查看 track 关联关系
- `platform_mark_track_identification` — 人工标注 track 识别 (岸基确认)

#### 12.2.5 Configuration Management Service

UMAA 要求支持 Mission Configuration 概念：一个 mission 携带完整的平台配置快照：

```rust
// crates/openfang-types/src/config.rs  (扩展)

pub struct MissionConfig {
    pub mission_id: String,
    pub platform_config: PlatformConfigSnapshot,
    pub agent_configs: Vec<AgentConfigSnapshot>,
    pub roe: RulesOfEngagement,
    pub geofences: Vec<Geofence>,
    pub comm_plan: CommunicationPlan,
    pub contingency_plans: Vec<ContingencyPlan>,  // 应急计划
    pub activated_at: f64,
}

pub struct ContingencyPlan {
    pub name: String,
    pub trigger: ContingencyTrigger,  // CommLost / LowFuel / ThreatLevelChange
    pub actions: Vec<ContingencyAction>, // SwitchToBackup / RTB / DCC_RuleEnable
    pub priority: u32,
}
```

**新增 Agent Tools**:
- `platform_activate_mission_config` — 激活指定 mission 配置
- `platform_switch_to_contingency` — 切换应急计划
- `platform_get_active_config` — 获取当前生效配置

#### 12.2.6 Mission Package Management

UMAA 的核心概念之一：武器/传感器作为可互换"任务包"管理：

```rust
// crates/openfang-types/src/platform.rs  (扩展)

pub struct MissionPackage {
    pub package_id: String,
    pub package_type: PackageType,      // ISR / Strike / MCM / ASW / SUW / MultiMission
    pub sensors: Vec<SensorAsset>,
    pub weapons: Vec<WeaponAsset>,
    pub estimated_endurance_impact_s: f64,  // 对续航的影响
    pub compatibility: Vec<String>,     // 兼容平台列表
}

pub enum PackageType {
    ISR,            // 情报监视侦察 (SAR/EOIR/ESM)
    Strike,         // 打击 (反舰/对陆攻击)
    MCM,            // 反水雷
    ASW,            // 反潜
    SUW,            // 水面战
    MultiMission,   // 多任务
}

impl MissionPackage {
    /// 验证 package 与当前平台兼容
    pub fn validate_compatibility(&self, platform: &PlatformState) -> Result<(), String>;
    
    /// 估算任务持续时间 (基于负载燃料消耗)
    pub fn estimate_endurance(&self, fuel_kg: f64) -> f64;
}
```

**新增 Agent Tools**:
- `platform_get_mission_packages` — 列出可用任务包
- `platform_activate_mission_package` — 激活指定任务包
- `platform_get_package_status` — 查询任务包就绪状态

### 13.3 UMAA 自主等级对齐

UMAA 定义 5 级自主权，OpenFang 需映射其行为模式：

| UMAA Level | 描述 | OpenFang 实现 |
|:---:|------|--------------|
| **L1** Human Operated | 人工远程操控 | 不适用 (本方案无遥控模式) |
| **L2** Human Delegated | 人工授权自主执行单项任务 | ApprovalManager 门控 (武器操作) + TCA 接收高级指令 |
| **L3** Human Supervised | 人工监督，自主执行多项任务 | TCA Agent + DCC 防御性自主 + CommunicationMonitor 通信监督 |
| **L4** Human-on-the-Loop | 人在回路外，自主执行，异常时人工介入 | CommunicationMonitor 检测中断 → auto_approve_autonomous = true |
| **L5** Fully Autonomous | 完全自主，无需人工 | 通信中断 72h 全自主模式 + 情报缓存后同步 |

**模式切换机制**:

```rust
pub enum AutonomyMode {
    HumanSupervised(L3),       // 默认模式: Agent 决策 + 岸基监督
    HumanOnTheLoop(L4),        // 通信降级: Agent 全自主 + 定期状态报告
    FullyAutonomous(L5),       // 通信中断: Agent 完全自主 + 本地缓存
}

impl AutonomyMode {
    /// 根据通信链路状态自动切换
    pub fn from_link_status(status: LinkStatus) -> Self {
        match status {
            LinkStatus::Connected    => Self::HumanSupervised(L3),
            LinkStatus::Degraded     => Self::HumanOnTheLoop(L4),
            LinkStatus::Lost         => Self::FullyAutonomous(L5),
        }
    }
}
```

### 13.4 完善后的 Agent 矩阵

| Agent | UMAA 对齐 | 职责 | 新增工具数 |
|-------|----------|------|:---:|
| TCA | Mission Management | 战术决策 + 自主等级管理 + ROE 控制 | +4 |
| SMA | Sensor Management | 传感器调度 + SensorTasking (search/track/classify) | +2 |
| NA | Route Planning + Navigation | 航线规划 + 位置估计融合 | +3 |
| FCA | Weapon Management | 射击诸元 + MissionPackage 管理 | +2 |
| CA | Communications | 带宽分配 + LinkQuality 报告 + 中继管理 | +2 |
| **FMA** | Payload / Fleet Management | UAV 集群调度 + 任务分配 | +14 |
| **HMA** (新增) | Health Monitoring | 系统健康报告 + BIT + 故障诊断 | +4 |
| **ORA** (新增) | Operational Restrictions | ROE 管理 + Geofence + 平台限制 | +4 |

### 13.5 新增设计决策

| 决策 | 内容 |
|------|------|
| ADR-021 | 引入 UMAA 兼容的 Health Monitoring Service (HMA Agent) |
| ADR-022 | 引入 UMAA 兼容的 Operational Restrictions Manager (ORA Agent) |
| ADR-023 | Navigation Service 从 NavControl 分离，独立为位置估计服务 |
| ADR-024 | Track Management 从 SensorFusion 分离，标准化 TrackCorrelation/Identification |
| ADR-025 | 引入 MissionConfig 和 ContingencyPlan 概念 (UMAA Configuration Management) |
| ADR-026 | 引入 MissionPackage 可互换载荷管理 (UMAA Payload Management) |
| ADR-027 | 自主等级 (L3/L4/L5) 根据 CommunicationMonitor 链路状态自动切换 |
| ADR-028 | Geofence 违规和 ROE 变更通过 DCC 规则自动响应 |

### 13.6 代码量影响

| 新增/修改组件 | 代码量 |
|-------------|:---:|
| HealthMonitor (HMA) | +350 行 |
| OpRestrictionsManager (ORA) | +400 行 |
| NavigationService (独立) | +250 行 |
| TrackManager (标准化) | +350 行 |
| MissionConfig + ContingencyPlan | +200 行 |
| MissionPackage 管理 | +200 行 |
| AutonomyMode 状态机 | +100 行 |
| 新增 DCC 规则 (geofence + ROE) | +80 行 |
| 新增 Agent Tools (~22 个) | +500 行 |
| **UMAA 对齐总计** | **+~2430 行** |

---

## 13. 异构无人集群协同控制

## 13. 异构无人集群协同控制

> **场景**: 无人艇搭载 N 架异构 UAV（侦察型 x2 + 打击型 x2 + 通信中继 x1），
> 艇载 TCA Agent 需统一调度艇载武器 + 空中集群，形成跨域协同打击链。

### 13.1 能力缺口分析

扩展到"艇 + 多 UAV"集群后，需新增四大能力维度：

**平台生命周期管理**: UAV 发射/回收控制、状态追踪(油量/弹药/损伤)、甲板资源管理、损毁/失联处理
**任务分配与协同**: 侦察/打击/中继任务分配、目标交接(Cueing)、战斗损伤评估(BDA)、动态任务重分配、多机同时攻击(TOT)
**编队与空域管理**: 编队保持、空域去冲突、通信中继模式、电磁静默编队
**跨域协同打击链**: 艇-UAV 协同 ISR、艇压制防空+UAV 穿透攻击、传感器交叉提示、武器接力制导

### 13.2 领域类型扩展

在 `openfang-types/src/platform.rs` 中新增：

```rust
pub struct UavState {
    pub base_platform_id: String,     // 母艇 ID
    pub uav_role: UavRole,            // Recon / Strike / Relay / Decoy / MultiRole
    pub endurance_remaining_s: f64,   // 剩余续航 (秒)
    pub max_endurance_s: f64,
    pub payload: UavPayload,
    pub recovery_status: RecoveryStatus,
    pub launch_readiness: LaunchReadiness,
    pub comm_link_quality: f64,       // 0.0-1.0
    pub current_mission: Option<UavMission>,
}

pub struct UavMission {
    pub mission_id: String,
    pub mission_type: MissionType,    // AreaSearch / TrackTarget / StrikeTarget / BDA / CommRelay / ReturnToBase
    pub assigned_uav_id: String,
    pub status: MissionStatus,        // Assigned / EnRoute / OnStation / Executing / Complete / Aborted / Lost
    pub waypoints: Vec<Waypoint>,
    pub target_id: Option<String>,
    pub constraints: MissionConstraints,  // max_duration, min_fuel_reserve, comm_required, abort_on_comm_loss
}

pub struct FormationDefinition {
    pub name: String,
    pub formation_type: FormationType, // LineAbreast / EchelonLeft / Vee / Diamond / Column
    pub reference_platform_id: String,
    pub spacing_m: f64,
    pub altitude_stack_m: f64,
}
```

### 13.3 新增 13 个 PlatformCommand 变体

| 类别 | 命令 | 用途 |
|------|------|------|
| 发射/回收 | `LaunchUav`, `RecoverUav` | UAV 甲板操作 |
| 任务控制 | `AssignMission`, `AbortMission`, `ReturnToBase` | 任务生命周期 |
| 目标交接 | `HandoffTarget` | 跨平台传递 track |
| 编队 | `FormUp`, `BreakFormation`, `FormationManeuver` | 集群编队控制 |
| 协同打击 | `CoordinatedStrike`, `WeaponGuidanceHandoff` | TOT 同步 + 在途武器制导交接 |
| 甲板管理 | `DeckReconfigure` | 装弹/加油/维护配置 |
| 通信中继 | `RelayEnable`, `RelayDisable` | UAV 中继模式开关 |

### 13.4 新增 Agent: FMA (Fleet Management Agent)

FMA 作为独立 Agent，接收 TCA 高级指令，负责 UAV 级别调度：

```
TCA (战术指挥官)          FMA (舰队管理)
    │ "搜索区域 Alpha"       │ → 分配 Recon-UAV-01 执行 AreaSearch
    │ "打击目标 Bravo"       │ → 分配 Strike-UAV-02 攻击 + TOT 协调
    │ ◄ UAV-01 发现目标      │
    │ "重分配 Strike-UAV-02" │ → 更新任务
```

### 13.5 新增 5 个 Workflow

- `CoordinatedStrike`: TCA→FMA→WeaponInterface→FMA BDA→TCA
- `ReconToStrike`: SMA→UAV识别→TCA→FMA→FCA TOT→BDA
- `FleetRecovery`: CA→FMA→RTB序列→DeckManager
- `AutoTaskReallocate`: Trigger UAV Lost→FMA重分配→TCA确认
- `CommRelayHandoff`: CommMonitor→FMA指派RelayUAV→RelayEnable

### 13.6 新增 4 条 DCC 规则

```toml
[[direct_command_rules]]
name = "auto_rtb_on_low_fuel"
condition = { type = "UavFuelCritical", min_reserve_pct = 0.12 }
action = { type = "ReturnToBase", urgency = "Emergency" }
priority = "Critical"

[[direct_command_rules]]
name = "auto_abort_on_comm_loss"
condition = { type = "UavCommLost", timeout_s = 30 }
action = { type = "ReturnToBase", urgency = "Emergency" }
priority = "Critical"

[[direct_command_rules]]
name = "auto_retask_on_uav_loss"
condition = { type = "UavLost", uav_role = "Strike" }
action = { type = "NotifyAgent", target_agent = "fma" }
priority = "High"

[[direct_command_rules]]
name = "auto_launch_recon_on_contact"
condition = { type = "ContactDetected", confidence = 0.6, range_m = 30000 }
action = { type = "LaunchUav", uav_role = "Recon" }
priority = "High"
```

### 13.7 新增 14 个 Agent Tools

`platform_launch_uav`, `platform_recover_uav`, `platform_assign_uav_mission`, `platform_abort_uav_mission`, `platform_rtb_uav`, `platform_handoff_target`, `platform_form_up`, `platform_break_formation`, `platform_coordinated_strike`, `platform_weapon_guidance_handoff`, `platform_deck_reconfigure`, `platform_relay_enable`, `platform_relay_disable`, `platform_get_fleet_status`

### 13.8 对现有架构影响

| 组件 | 改动 |
|------|------|
| `PlatformAdapter` trait | **不变** — 签名已支持任意 PlatformCommand |
| `WorldSnapshot` | +100 行 (UavState字段) |
| `PlatformCommand` | +200 行 (13 变体) |
| Adapters (ArkSim/DDS) | +300 行 (命令映射) |
| DCC | +200 行 (4 规则 + 3 条件类型) |
| FMA Agent | +500 行 (manifest + prompt) |
| Workflows | +300 行 (5 定义) |
| Agent Tools | +400 行 (14 tool) |
| 领域类型 | +500 行 |
| **总计** | **+~2500 行** |

### 13.9 设计决策

| 决策 | 内容 |
|------|------|
| ADR-016 | FMA 为独立 Agent，接收 TCA 高级指令，负责 UAV 级别调度 |
| ADR-017 | UAV 作为 `PlatformState` 的扩展字段，统一 WorldSnapshot 模型 |
| ADR-018 | 目标交接携带完整 track 数据而非引用 |
| ADR-019 | 编队为"松散编队"：FMA 下发约束，各 UAV 自主保持 |
| ADR-020 | UAV 失联 30s 后自动触发任务重分配 |

---

> **文档维护**: 本文件随 Phase 推进持续更新。


## 附录 B: 不动部分

以下 OpenFang 组件不修改、不裁剪、不重构：

- `openfang-cli` — 用户活跃开发中
- `openfang-types` 核心类型（仅扩展 Capability 枚举）
- `openfang-memory` SQLite 基础结构（仅新增表）
- `openfang-runtime` LLM driver 架构（仅扩展 tool 列表）
- `openfang-wire` OFP 协议（直接使用）
- `openfang-api` 路由层（仅调试用）

---

> **文档维护**: 本文件随 Phase 推进持续更新。每个 Phase 完成后更新进度状态。
