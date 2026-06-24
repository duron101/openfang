# OpenFang 远洋无人艇自主大脑 — 产品需求文档 (PRD)

> 版本: v1.0 | 日期: 2026-06-06 | 状态: Draft  
> 方法论: BMAD (Business-Model-Analysis-Design)
> 基础框架: OpenFang v0.3.24 (Rust, 14 crates, 137K LOC)

---

> **范围说明（2026-06-08 更新）**：本文档聚焦**水面无人艇（USV）**单平台自主大脑。
> 无人机相关需求已拆分到独立文档，避免单机 / 集群控制逻辑混杂：
> - 单机无人机（CCA / 低速 LSUAV）：[`prd-uav-single.md`](prd-uav-single.md) / [`plan-uav-single.md`](plan-uav-single.md)
> - 无人机母机（舰队 / 收放）：[`prd-uav-mothership.md`](prd-uav-mothership.md) / [`plan-uav-mothership.md`](plan-uav-mothership.md)
> - ArkSIM 仿真集成（蓝军 Deferred）：[`plan-arksim.md`](plan-arksim.md)
>
> 三者共享 Phase 0「活线接入」基础设施（`PlatformConfig` → `AdapterRegistry` → `platform_*` 工具 → `PlatformControlLoop`），详见各 plan 文档。

---

## 目录

1. [执行摘要](#1-执行摘要)
2. [干系人分析与指挥链](#2-干系人分析与指挥链)
3. [作战场景与任务剖面](#3-作战场景与任务剖面)
4. [功能需求矩阵](#4-功能需求矩阵)
5. [非功能需求与约束](#5-非功能需求与约束)
6. [系统接口定义](#6-系统接口定义)
7. [架构决策记录](#7-架构决策记录)
8. [风险矩阵与缓解](#8-风险矩阵与缓解)
9. [实施路线图](#9-实施路线图)
10. [附录: OpenFang 框架适配分析](#10-附录)

---

## 1. 执行摘要

### 1.1 项目目标

基于 OpenFang Agent 操作系统构建远洋无人艇的自主大脑，实现：

1. **任务自主调度**：机动控制、航路规划、能量管理、传感器控制、载荷发射、航迹处理的自主编排
2. **实时控制**：通过 Rust/WASM + DDS 实现微秒级设备控制与状态反馈
3. **远程指挥**：通过卫星/数传链路接收打击/返航/自毁/机动指令，上报情报与态势
4. **安全保障**：多层武器安全门控、人工审批、审计追踪

### 1.2 技术选型

| 层级 | 技术 | 角色 |
|------|------|------|
| **战术推理** | Qwen 3.6 27B (Q4_K_M) + llama.cpp | LLM Agent 决策 |
| **驱动框架** | OpenFang (OpenAIDriver) | Agent loop、工具调用、工作流引擎 |
| **实时控制** | Rust WASM + rustdds (FastDDS 兼容) | 艇内设备 pub/sub |
| **远程通信** | OFP (HMAC-SHA256) + A2A over 卫星/数传 | 岸基 ↔ 艇载 C2 |
| **音频处理** | WASM DSP 管线 + ONNX 轻量分类器 | 声纳/水听器信号处理 |
| **安全门控** | OpenFang ApprovalManager + 新增武器 Capability | 武器/自毁多层确认 |
| **审计** | OpenFang Merkle 哈希链 | 全操作可追溯 |

### 1.3 核心架构决策

- **双层架构**：慢回路 (LLM Agent, 秒级) 做战术决策；快回路 (WASM+DDS, 微秒级) 做实时控制
- **音频解耦**：声学信号处理不进 LLM，走独立 WASM DSP 管线，输出结构化事件给 LLM
- **武器门控**：利用 OpenFang 已有的 ApprovalManager + 新增武器 Capability 类型
- **双协议**：OFP 做岸艇 C2，DDS 做艇内设备总线

---

## 2. 干系人分析与指挥链

### 2.1 干系人全景

#### 第一圈：直接决策与指挥

| 编号 | 角色 | 授权级别 | 通信链路 | 核心关注点 |
|------|------|---------|---------|-----------|
| S1 | 岸基指挥官 (SC) | 最高 — 武器/自毁/任务变更审批 | 卫星 + 数据链 | 态势时效性、武器确认链、可追溯性 |
| S2 | 岸基情报分析员 (IA) | 只读 + 传感器任务指派 | 卫星 | 航迹质量、情报格式标准化、历史回放 |
| S3 | 艇载战术指挥官 Agent (TCA) | 受约束自主 — 战术机动自主，武器需批准 | 艇内 KernelHandle | 任务编排正确性、威胁评估时效性 |
| S4 | 岸基安全官 (SO) | 对 SC 武器决策有独立否决权 | 卫星 | 武器互锁状态、ROE 合规、自毁合法性 |

#### 第二圈：艇载执行

| 编号 | 角色 | 授权级别 | 关注点 |
|------|------|---------|--------|
| S5 | 火控 Agent (FCA) | 无独立发射权 | 射击诸元精度、武器 BIT、发射安全检查 |
| S6 | 导航规划 Agent (NA) | 全自主机动 | 路径最优性、碰撞风险、海况影响 |
| S7 | 传感器管理 Agent (SMA) | 全自主控制 | 多传感器协同、电磁静默、航迹管理 |
| S8 | 通信管理 Agent (CA) | 全自主通信 | 带宽分配、中断缓存、通信保密 |

#### 第三圈：外部与环境

| 编号 | 角色 | 接口 | 关注点 |
|------|------|------|--------|
| S9 | 上级指挥节点 (HEC) | 通过 SC 或卫星直达 | 战场态势融合、联合作战协调 |
| S10 | 友邻无人平台 (PUP) | OFP P2P / 数据链 | 协同搜索、目标交接、编队保持 |
| S11 | 敌方威胁 (ADV) | 非干系人，但约束设计 | → 电磁静默、抗干扰、低可探测性 |

### 2.2 作战指挥链

```
上级指挥节点 (HEC)
  │ 战略指令、ROE 更新
  ▼
岸基指挥官 (SC) ←→ 岸基安全官 (SO)  [否决权]
  │ OFP Protocol + TLS
  ▼
艇载通信管理 Agent (CA)
  ├─→ 战术指挥官 Agent (TCA)
  │     ├─→ 传感器管理 Agent (SMA) ←→ DDS ←→ 雷达/声纳/ESM
  │     ├─→ 导航规划 Agent (NA)    ←→ DDS ←→ 舵机/电调/惯导
  │     └─→ 火控 Agent (FCA)       ←→ DDS ←→ 武器接口
  └─→ 岸基情报分析员 (IA) ← 情报回传
```

### 2.3 信息流与延迟要求

| 流向 | 内容 | 协议 | 延迟要求 | 优先级 |
|------|------|------|---------|--------|
| HEC → SC | 战略指令、ROE | 军标 / A2A | < 60s | P2 |
| SC → TCA | 打击/返航/自毁/机动指令 | OFP+HMAC | < 3s | P0 |
| TCA → SC | 目标情报、态势报告 | OFP+JSON | < 5s | P1 |
| TCA → FCA | 武器发射指令 | Internal | < 50ms | P0 |
| SMA → TCA | 目标航迹、威胁评估 | DDS | < 10ms | P0 |
| NA → TCA | 位置、航速、能耗 | DDS | < 50ms | P1 |
| SMA ↔ 传感器 | 原始/处理数据 | DDS | < 1ms | P0 |

---

## 3. 作战场景与任务剖面

### 3.1 场景 1: 区域巡逻与目标搜索 (Patrol)

**触发条件**: 岸基 SC 下发巡逻任务 (坐标区域、巡逻参数)  
**持续时间**: 72 小时 (典型)  
**自主程度**: 高 — TCA 自主规划巡逻航线，SMA 自主管理传感器

**序列**:
1. CA 接收巡逻任务参数 → TCA 解析
2. TCA 启动 Workflow: `PatrolOrchestration`
3. NA 计算最优航线 (考虑能耗、隐蔽)
4. SMA 配置传感器策略 (搜索模式、扫描周期)
5. **循环** (每 5 分钟):
   - SMA 上报接触列表 → TCA 评估威胁
   - 若发现可疑目标 → 触发 `TargetInvestigation` 子流程
   - 若能耗超标 → NA 调整航线
6. 每小时: CA 通过卫星压缩上报态势摘要

**异常分支**:
- 通信中断 > 30 分钟: TCA 切换为完全自主模式，继续巡逻，缓存情报
- 探测到高威胁信号: TCA 触发规避机动，SMA 切换为无源监听

### 3.2 场景 2: 目标识别与跟踪 (Track)

**触发条件**: SMA 检测到未知接触  
**持续时间**: 10 分钟 — 数小时  
**自主程度**: 中高 — TCA 自主跟踪，识别结果回传岸基确认

**序列**:
1. SMA 检测到接触 → 发布 `contact_detected` 事件
2. TCA 启动 Workflow: `TargetInvestigation`
3. **并行** (fan_out):
   - SMA: 调度雷达/光电/ESM 协同探测
   - FCA: 计算拦截概率和武器准备时间
   - NA: 计算最优跟踪航线
4. Audio Processor WASM: 若水听器可用 → 宽带/窄带分析 → 特征匹配
5. TCA 融合多传感器数据 → 目标分类 (军舰/商船/潜艇/杂波)
6. CA 压缩目标特征数据 → 卫星回传岸基 IA
7. 岸基确认/更正分类 → 回传 TCA

### 3.3 场景 3: 交战决策与武器发射 (Engage)

**触发条件**: 岸基 SC 下发打击指令，或 TCA 在预授权条件下判定威胁  
**持续时间**: 30 秒 — 5 分钟 (决策链)  
**自主程度**: 低 — 严格人工审批链

**序列**:
1. TCA 评估满足交战条件 → 发出 `engagement_ready` 事件
2. TCA → FCA: 传递目标参数 → 解算射击诸元
3. FCA → ApprovalManager: 请求 `weapon_arm` → **阻塞等待岸基批准**
4. CA 发送 ApprovalRequest `{type: "weapon_launch", risk: "Critical"}` 到 SC+SO
5. SC 批准 + SO 未否决 → ApprovalManager 返回 Approved
6. FCA → ApprovalManager: 请求 `weapon_launch` → **第二次阻塞**
7. SC+SO 确认 → 执行发射
8. TCA 进入 `battle_damage_assessment` 子流程
9. 全操作记录 Merkle 审计链

**安全门控**:
- `weapon_arm`: 需 SC 批准 (timeout=300s, 超时自动拒绝)
- `weapon_launch`: 需 SC+SO 双人批准 (timeout=120s)
- `self_destruct`: 需 SC+SO+HEC 三人 HMAC 签名

### 3.4 场景 4: 通信中断自主生存 (Survive)

**触发条件**: 卫星链路中断 > 阈值  
**持续时间**: 数小时 — 数天  
**自主程度**: 最高 — 全自主

**序列**:
1. CA 检测链路中断 > 配置阈值
2. TCA 切换 `ScheduleMode::Continuous` → 提升自主决策频率
3. ApprovalPolicy 切换: `auto_approve_autonomous = true` (仅非武器操作)
4. 情报缓存到本地 SQLite，待链路恢复后批量回传 (增量同步)
5. 能源管理优先级重排 (传感器 → 推进 → 武器待机)

### 3.5 场景 5: 自毁程序 (Scuttle)

**触发条件**: 岸基 SC 下发自毁指令  
**持续时间**: 120 秒决策窗口  
**自主程度**: 零 — 完全人工

**序列**:
1. CA 接收 OFP 消息 `{type: "self_destruct", signatures: [SC_HMAC, SO_HMAC, HEC_HMAC]}`
2. TCA 验证 3 方 HMAC 签名 (常数时间比较)
3. SelfDestructGuard 验证:
   - 签名数量 ≥ 3
   - 签名者身份合法
   - 指令未过期 (timestamp 偏差 < 60s)
4. TCA 广播 `abandon_ship` 事件
5. 所有 Agent 进入终止序列，记录最终状态
6. 执行自毁

---

## 4. 功能需求矩阵

### 4.1 核心功能 (P0 — 系统不可用则功能无效)

| ID | 功能 | 描述 | 实现方式 | 依赖 |
|----|------|------|---------|------|
| F-001 | 多 Agent 任务编排 | TCA 调度子 Agent 执行机动/航路/传感器/武器任务 | OpenFang WorkflowEngine (sequential/fan_out/conditional/loop) | Kernel |
| F-002 | 自主导航与避碰 | 实时航线规划、障碍物规避、能耗优化 | NA Agent + WASM path_planner skill | DDS→GPS/IMU/雷达 |
| F-003 | 传感器数据融合 | 雷达+光电+ESM+声纳多源融合→统一航迹 | SMA Agent + WASM fusion skill | DDS→各传感器 |
| F-004 | 武器安全门控 | 武器解保/发射需岸基人工审批 | ApprovalManager + 新增 WeaponArm/WeaponLaunch Capability | OFP 链路 |
| F-005 | 岸艇 C2 通信 | 指令接收、情报上报、状态同步 | OFP Protocol (HMAC-SHA256) + A2A | 卫星/数传物理层 |
| F-006 | 自毁安全确认 | 三方 HMAC 签名验证 | 新增 SelfDestructGuard | OFP + Capability |
| F-007 | 操作审计追踪 | 所有武器/导航关键操作链上记录 | Merkle 哈希链 (已有 audit.rs) | SQLite |

### 4.2 扩展功能 (P1 — 增强但非阻断)

| ID | 功能 | 描述 | 实现方式 |
|----|------|------|---------|
| F-101 | 声学信号分类 | 声纳/水听器信号实时分类 | WASM audio_processor skill + ONNX 模型 |
| F-102 | 电磁静默管理 | 根据威胁等级切换有源/无源传感器 | SMA Agent + TriggerEngine (event:threat_level) |
| F-103 | 通信中断自主 | 链路中断后全自主运行 + 情报缓存 | CA Agent + 本地 SQLite 缓存 + 增量同步 |
| F-104 | 能源优化调度 | 根据任务优先级分配动力/传感器/武器能耗 | NA Agent + WASM energy_manager skill |
| F-105 | 友邻协同 | 与无人机/无人艇/无人潜航器协同搜索 | OFP P2P + A2A discover |
| F-106 | 战斗损伤评估 | 攻击后自动评估效果 | TCA Workflow (SMA→FCA→IA) |

### 4.3 增强功能 (P2 — 锦上添花)

| ID | 功能 | 描述 |
|----|------|------|
| F-201 | 预测性维护 | 基于设备 BIT 数据预测故障 |
| F-202 | 自适应交战规则 | 根据战场态势动态调整自主权限边界 |
| F-203 | 多艇编队协同 | 多无人艇分布式任务分配 |

---

## 5. 非功能需求与约束

### 5.1 实时性要求

| 需求 | 指标 | 测量方法 |
|------|------|---------|
| NFR-R01 | 舵机/电调控制延迟 < 1ms | DDS topic 往返时间 |
| NFR-R02 | 传感器数据融合延迟 < 10ms | 原始数据到航迹输出的端到端延迟 |
| NFR-R03 | 武器发射指令延迟 < 50ms | TCA→FCA→武器接口的 KernelHandle 调用链 |
| NFR-R04 | LLM 战术推理延迟 < 8s | Agent loop 单轮 (含 tool calling) |
| NFR-R05 | 岸艇指令往返延迟 < 3s | OFP Ping/Pong over 卫星 |
| NFR-R06 | 系统冷启动时间 < 5s | 从 openfang start 到首轮 agent loop 就绪 |

### 5.2 安全要求

| 需求 | 指标 |
|------|------|
| NFR-S01 | 武器操作必须有岸基人工批准 (单人最低) |
| NFR-S02 | 自毁操作必须三方 HMAC 签名 (SC+SO+HEC) |
| NFR-S03 | 所有武器操作记录 Merkle 审计链 |
| NFR-S04 | Agent 能力继承: 子 Agent 不能越权父 Agent |
| NFR-S05 | 通信链路 HMAC-SHA256 双向认证, 常数时间比较 |
| NFR-S06 | 关键内存区域使用 Zeroizing 自动擦除 |
| NFR-S07 | WASM Skill 运行在双重计量沙箱 (Fuel+Epoch) |

### 5.3 可靠性要求

| 需求 | 指标 |
|------|------|
| NFR-D01 | 系统无故障运行时间 > 720 小时 (30 天) |
| NFR-D02 | Agent crash 后 Supervisor 自动重启 (max 3 次/小时) |
| NFR-D03 | 通信中断后本地自主运行 > 72 小时 |
| NFR-D04 | SQLite 数据持久化, 掉电不丢失 |
| NFR-D05 | Session repair 自动修复损坏的 LLM 对话历史 |

### 5.4 环境约束

| 约束 | 描述 |
|------|------|
| C-E01 | 艇载计算平台: Jetson AGX Orin 64GB 或等效 |
| C-E02 | 推理功耗预算: < 150W (含 GPU + CPU) |
| C-E03 | 系统总功耗: < 300W (含计算 + 传感器 + 通信) |
| C-E04 | 工作温度: -20°C ~ +60°C |
| C-E05 | 抗冲击/振动: MIL-STD-810G |
| C-E06 | 电磁兼容: MIL-STD-461G |
| C-E07 | 卫星通信带宽: 典型 256 Kbps, 突发 2 Mbps |
| C-E08 | 数传电台: 典型 9.6 Kbps (窄带备份) |

### 5.5 软件约束

| 约束 | 描述 |
|------|------|
| C-S01 | 二进制体积 < 100MB (含 OpenFang + DDS + WASM skills) |
| C-S02 | 内存占用 (空闲) < 500MB |
| C-S03 | 模型加载内存 (Qwen 3.6 Q4_K_M + KV cache) < 22GB |
| C-S04 | 不依赖云端 API (完全本地推理) |
| C-S05 | 不改动 openfang-cli crate (用户活跃开发中) |

---

## 6. 系统接口定义

### 6.1 艇内接口 (DDS Topics)

| Topic | 发布者 | 订阅者 | QoS | 数据类型 |
|-------|--------|--------|-----|---------|
| `nav/position` | GPS/IMU | NA, TCA | BEST_EFFORT, KEEP_LAST(5) | `NavPosition` |
| `nav/attitude` | IMU | NA | BEST_EFFORT, KEEP_LAST(5) | `Attitude` |
| `nav/cmd` | NA | 舵机/电调 | RELIABLE, DEADLINE(50ms) | `NavCommand` |
| `sensor/radar/tracks` | 雷达处理器 | SMA | BEST_EFFORT, KEEP_LAST(20) | `RadarTrack[]` |
| `sensor/eo/image` | 光电处理器 | SMA | BEST_EFFORT | `EOImage` |
| `sensor/sonar/raw` | 声纳 | Audio WASM | BEST_EFFORT, KEEP_LAST(1) | `AudioFrame` |
| `sensor/sonar/events` | Audio WASM | SMA | RELIABLE | `AcousticEvent` |
| `sensor/cmd` | SMA | 各传感器 | RELIABLE, DEADLINE(100ms) | `SensorCommand` |
| `weapon/status` | 武器接口 | FCA | RELIABLE, TRANSIENT_LOCAL | `WeaponStatus` |
| `weapon/cmd` | FCA | 武器接口 | RELIABLE, DEADLINE(50ms) | `WeaponCommand` |
| `platform/heartbeat` | 各节点 | CA | BEST_EFFORT, LIVELINESS(1s) | `Heartbeat` |
| `platform/alert` | 任意 | CA, TCA | RELIABLE | `Alert` |

### 6.2 岸艇接口 (OFP + A2A)

| 消息类型 | 方向 | 协议 | 格式 |
|---------|------|------|------|
| 态势报告 | 艇→岸 | OFP RouteMessage | JSON: `{contacts, position, fuel, alerts}` |
| 打击指令 | 岸→艇 | OFP RouteMessage + HMAC | JSON: `{target, weapon_type, authorization}` |
| 返航指令 | 岸→艇 | OFP RouteMessage | JSON: `{waypoints, reason}` |
| 自毁指令 | 岸→艇 | OFP RouteMessage + 3xHMAC | JSON: `{signatures: [SC, SO, HEC], timestamp}` |
| 机动指令 | 岸→艇 | OFP RouteMessage | JSON: `{heading, speed, depth}` |
| 审批请求 | 艇→岸 | A2A tasks/send | A2A Task |
| 审批响应 | 岸→艇 | A2A tasks/{id} | A2A TaskStatus |
| 心跳 | 双向 | OFP Ping/Pong | UDP |

### 6.3 Agent 间接口 (KernelHandle)

| 调用 | 调用者 | 被调用者 | 描述 |
|------|--------|---------|------|
| `spawn_agent_checked()` | TCA | — | 创建子 Agent (能力继承校验) |
| `send_to_agent()` | TCA | SMA/NA/FCA | 任务指令下发 |
| `memory_store/recall()` | 任意 | 共享 KV | 跨 Agent 状态共享 |
| `task_post/claim/complete()` | 任意 | 任务队列 | 任务分发与完成 |
| `publish_event()` | 任意 | TriggerEngine | 事件广播 (如 threat_level_change) |
| `knowledge_*` | SMA/TCA | 知识图谱 | 目标数据库 (实体+关系) |

---

## 7. 架构决策记录

### ADR-001: 选用 Qwen 3.6 27B 而非 Gemma 4 12B

**状态**: 已决定  
**背景**: 需要在艇载边缘设备上运行 LLM 做战术推理和工具调用  
**决策**: Qwen 3.6 27B (Q4_K_M, ~16.8GB)

**理由**:
1. Qwen 3.6 的 qwen3_coder 工具调用解析器比 Gemma 4 的 6-token 机制更成熟
2. Gemma 4 的 `reasoning_content` 陷阱会导致 agent loop 静默失败（工具调用被路由到错误字段）
3. Qwen 3.6 在 SWE-bench Verified 达到 77.2%，证明多步工具链能力
4. 中文战术指令优化 (C-Eval 91.4%)
5. 音频处理已通过 WASM DSP 管线解耦，Gemma 4 的原生音频优势被消解

**后果**: 需要 ≥20GB 可用显存 (Jetson AGX Orin 64GB 满足)

### ADR-002: 音频信号处理不进 LLM

**状态**: 已决定  
**背景**: 声纳/水听器信号需要实时处理和分类  
**决策**: WASM DSP 管线 + ONNX 分类模型 → 结构化事件 → LLM

**理由**:
1. LLM 原生音频处理延迟在秒级，不满足实时性
2. 解耦后 DSP 管线微秒级延迟，分类器毫秒级
3. 特征提取可控 (MFCC、谱质心、谐波分析)
4. 分类模型可独立更新，无需重训 LLM

**后果**: 需扩展 WASM host functions (audio_capture, audio_classify)

### ADR-003: 武器操作采用 OpenFang 已有 ApprovalManager

**状态**: 已决定  
**背景**: 武器发射必须经过人工审批  
**决策**: 扩展 ApprovalManager 配置 + 新增 Capability 类型，不改动审批架构

**理由**:
1. ApprovalManager 已有完整的 request→block→resolve 流程
2. 支持超时自动拒绝、每人最多 5 个待审批、可通过 API/UI 批复
3. Capability 模型支持能力继承验证，子 Agent 不会越权
4. Merkle 审计链已有，无需额外开发

**后果**: 需新增 ~200 行 Capability 变体代码 + ~100 行 SelfDestructGuard

### ADR-004: 艇内通信采用 DDS，岸艇通信采用 OFP

**状态**: 已决定  
**背景**: 艇内需要微秒级设备控制，岸艇需要安全可靠的远程 C2  
**决策**: DDS (RTPS/UDP) 做艇内实时总线，OFP (TCP/UDP+HMAC) 做岸艇 C2

**理由**:
1. DDS 的 pub/sub 模型天然适合传感器数据分发
2. DDS QoS 可精确控制每个 topic 的可靠性/延迟/历史
3. OFP 已有的 HMAC-SHA256 双向认证满足军事通信安全
4. 两者互不冲突 (不同端口、不同协议)

**后果**: 需集成 rustdds crate (~500行) + 扩展 WASM host functions

### ADR-005: 通信中断后全自主运行

**状态**: 已决定  
**背景**: 卫星链路在远洋可能中断数小时  
**决策**: 链路中断检测 → TCA 切换 `auto_approve_autonomous=true` (仅非武器) → 本地情报缓存 → 链路恢复后增量同步

**理由**:
1. OpenFang BackgroundExecutor 已有 Continuous mode，天然支持自主运行
2. ApprovalManager 的 auto_approve_autonomous 开关可热切换
3. 本地 SQLite 可无限缓存，增量同步可用简单的时间戳方案

---

## 8. 风险矩阵与缓解

| 风险 ID | 风险描述 | 概率 | 影响 | 等级 | 缓解措施 |
|---------|---------|------|------|------|---------|
| R-01 | Qwen 3.6 27B 在 Jetson 上推理速度不满足战术决策时限 | 中 | 高 | 🔴 | 预量化性能基准测试；备选 Qwen3.6-35B-A3B MoE (更快但精度稍低) |
| R-02 | LLM 工具调用幻觉导致武器误操作 | 低 | 致命 | 🔴 | ApprovalManager 硬门控；武器操作无论如何都需要人工批准 |
| R-03 | 卫星链路延迟/中断导致指令丢失 | 高 | 中 | 🟡 | OFP 消息缓存+重传；A2A 异步任务模式；中断后自主运行 |
| R-04 | 敌方电子干扰导致通信完全中断 | 中 | 高 | 🔴 | 全自主模式；数传电台备份链路；通信静默策略 |
| R-05 | rustdds 与 FastDDS 互操作存在兼容问题 | 低 | 中 | 🟡 | 早期集成测试；必要时使用 eCAL 替代方案 |
| R-06 | WASM 沙箱性能不足以运行 DSP | 低 | 低 | 🟢 | 关键 DSP 可提到 Rust 原生层；WASM 仅做控制逻辑 |
| R-07 | OpenFang 框架升级导致自定义扩展失效 | 中 | 低 | 🟢 | 扩展点尽量使用官方 trait 接口；定期 rebase |
| R-08 | Jetson 功耗/散热超限 | 中 | 中 | 🟡 | 功耗预算严格管控；动态降频；被动散热设计 |

---

## 9. 实施路线图

### Phase 1: 基础框架集成 (Week 1-3)

| 任务 | 工时 | 产出 |
|------|------|------|
| llama.cpp + Qwen 3.6 27B 部署在 Jetson | 3d | 本地推理可达 >15 tok/s |
| OpenFang config.toml 配置 custom provider | 1d | Agent loop 通过 OpenAIDriver 调用本地模型 |
| 新增 Capability 变体 (WeaponArm, WeaponLaunch, SelfDestruct) | 2d | 能力模型支持武器操作 |
| ApprovalManager 配置 (武器审批策略) | 1d | 武器操作门控生效 |
| 编写战术指挥官 Agent Manifest + System Prompt | 3d | TCA Agent 可运行并调用工具 |

### Phase 2: 实时控制层 (Week 4-7)

| 任务 | 工时 | 产出 |
|------|------|------|
| rustdds crate 集成到 openfang-runtime | 3d | DDS 发布/订阅可用 |
| 新增 WASM host functions (dds_publish, dds_subscribe, audio_*) | 3d | Skill 可通过 host_call 访问 DDS 和音频 |
| DDS Topic 定义 (nav/sensor/weapon/heartbeat) | 2d | IDL 定义 + QoS 配置 |
| WASM DSP 管线 (FFT + 特征提取) | 5d | 音频到结构化事件转换 |
| ONNX 声学分类器集成 | 3d | 目标分类置信度输出 |

### Phase 3: 岸艇通信 (Week 8-10)

| 任务 | 工时 | 产出 |
|------|------|------|
| OFP 卫星链路适配 (长 RTT 优化) | 3d | Ping/Pong 在 600ms RTT 下正常工作 |
| A2A 审批集成 (ApprovalRequest → A2A Task) | 2d | 岸基可通过 A2A 批复武器操作 |
| 通信中断自主模式 | 3d | 中断检测→缓存→增量同步 |
| 自毁三方 HMAC 签名验证 | 2d | SelfDestructGuard 功能完整 |

### Phase 4: Agent 开发与集成 (Week 11-16)

| 任务 | 工时 | 产出 |
|------|------|------|
| 传感器管理 Agent (SMA) | 5d | 多传感器调度 + 航迹融合 |
| 导航规划 Agent (NA) | 5d | 航线规划 + 避碰 + 能耗优化 |
| 火控 Agent (FCA) | 4d | 射击诸元解算 + 武器 BIT 监控 |
| 通信管理 Agent (CA) | 3d | OFP 消息路由 + 带宽分配 |
| TCA 编排 Workflow 定义 | 3d | Patrol/Track/Engage/Survive/Scuttle 工作流 |
| 各 Agent System Prompt 编写 | 2d | 专家级操作流程 |
| 端到端集成测试 (HIL 仿真) | 5d | 所有场景通过 |

### Phase 5: 测试与交付 (Week 17-20)

| 任务 | 工时 | 产出 |
|------|------|------|
| 硬件在环 (HIL) 仿真测试 | 5d | 传感器/武器仿真器验证 |
| 压力测试 (72h 连续运行) | 3d | 无内存泄漏、无 agent crash 累积 |
| 安全审计 (能力继承/审批链/审计链) | 3d | 渗透测试报告 |
| 文档交付 | 3d | 部署手册、运维手册、应急程序 |

---

## 10. 附录: OpenFang 框架适配分析

### 10.1 直接复用 (零代码改动)

| 组件 | 用途 |
|------|------|
| WorkflowEngine | 战术任务编排 (Patrol→Track→Engage) |
| BackgroundExecutor | Agent 自主运行 (Continuous/Periodic mode) |
| TriggerEngine | 事件驱动反应 (威胁检测→规避) |
| ApprovalManager | 武器操作人工审批 |
| Capability 能力模型 | Agent 权限控制 + 继承验证 |
| Merkle 审计链 | 武器操作全记录 |
| Session Repair | LLM 对话历史自动修复 |
| Loop Guard | 防止 Agent 工具调用死循环 |
| OpenAIDriver | 对接 llama.cpp (自定义 base_url) |
| A2A Protocol | 岸基审批接口 |
| SQLite Memory Substrate | 情报缓存 + 状态持久化 |
| OFP Protocol | 岸艇 C2 通信 |

### 10.2 需新增 (轻量扩展)

| 组件 | 新增内容 | 预计代码量 |
|------|---------|-----------|
| Capability 类型 | WeaponArm, WeaponLaunch, WeaponAbort, PayloadControl, SelfDestruct | ~200 行 |
| SelfDestructGuard | 三方 HMAC 签名验证 | ~100 行 |
| WASM host functions | dds_publish, dds_subscribe, audio_* | ~300 行 |
| rustdds 绑定层 | DDS 发布/订阅/RPC 封装 | ~500 行 |
| ApprovalPolicy 配置 | 武器相关工具的审批策略 | 配置项 |

### 10.3 不动部分

- openfang-cli (用户活跃开发中)
- openfang-desktop
- openfang-channels (40 个适配器，无人艇场景不需要)
- openfang-migrate

---

> **文档维护**: 本 PRD 随项目推进持续更新。架构决策变更需追加 ADR 并在决策记录中交叉引用。
