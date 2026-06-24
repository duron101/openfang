# 编队协同服务（TCS）与单机自主（ELU）交互契约（标准文档）

## 1. 概述
本文档用于定义并约束 **编队协同服务（Tactical Coordination Service, TCS）** 与 **单机自主执行单元（Execution Logic Unit, ELU）** 之间的交互契约。

**定位（重要）**：本文档不是对“当前实现”的总结，而是我们在工程上可落地的 **最佳设计规范（Normative Spec）**。未来迭代以本规范为依据。

**ABMS 首席架构师高级演进说明**：
在 ABMS（联合全域指挥控制）体系下，TCS（协同编排）与 ELU（单机自主）的交互摒弃了传统的单向“长机下发、僚机执行”的硬性控制，演进为**意图驱动、动态授权委派（LOA）、战术自愈重组、以及高对抗环境下的去中心化协同体系**。为此，本契约在 2.x 规范中重点升级并完善以下 ABMS 核心机制：
- **去中心化长机继任协议（Leader Succession Protocol）**：解决强对抗干扰、单点战损下编队的生存问题。UTA 包含结构化继任序列，当僚机评估长机失联时，可自主提升或重组。
- **动态自主度委派与安全门控（LOA Gating & Safeties）**：支持 1-4 级自主度（Level of Autonomy）分控制面授权。ELU 根据本地传感器、链路质量及 ROE 条件，动态裁剪行为动作包线。
- **断链突防与自愈执行策略（Resilient Link-Loss Policies）**：定义强干扰断链下僚机自主交战、协同搜索与安全返航规则，允许 ELU 在失网状态下继续闭环完成最后有效意图。
- **异构感知多传感器目标质量交付（Target Quality & Covariance Handover）**：在 L3 协同配合信号中，目标航迹共享（Track Snapshot）新增传感器融合置信度与协方差矩阵，支持多机多弹的高精度打击时间/空间对齐。

本规范覆盖：
- **TCS -> ELU** 的指令面（Command Plane）：`UnitTaskAssignment (UTA)`
- **ELU -> TCS/全局总线** 的事件/状态面（Event/Status Plane）：`unit.state`、`tcn.signal`
- **ELU -> 仿真/控制网关** 的动作面（Action Plane）：`ActionEnvelopeV2`
- **ELU 订阅的外部态势/物理真值输入**：`platformstate/weaponstate/situation evaluation/threat_destroyed`

**权威契约来源（Single Source of Truth）**
- 本目录三份产物构成未来的“契约主规范”：
  - `ELU_TCS_交互契约_标准文档.md`（本文件：规范条款）
  - `elu_tcs_contracts.proto`（跨语言契约索引 + 强类型语义）
  - `elu_tcs_contracts.schema.json`（JSON wire 校验 + 示例 + required/约束）

实现可以滞后，但**不得反向修改规范以迁就实现**。

## 1.1 规范性用语
为避免歧义，本文采用 RFC 2119 风格的规范性关键词：
- **MUST**：必须满足，否则视为不符合本规范。
- **SHOULD**：强烈建议满足；若不满足，需要明确理由并记录。
- **MAY**：可选。

## 1.2 演进与治理（核心原则）
- **Forward-only（只增不改）**：所有契约（proto/schema/示例）只允许新增字段；禁止重命名/复用语义。
- **Schema 可校验**：所有 NATS+JSON 消息在生产环境 MUST 通过对应 JSON Schema 校验。
- **契约注册表**：开放字段（`parameters`/`opaque`/`diagnostics` 等）MUST 配套 key 注册表（命名、类型、必填性）。
- **可仲裁真值**：`unit.state` 被视为 ELU 对外发布的“逻辑真值”，用于仲裁与观测；不应被 debug/overlay 替代。

## 2. 角色与职责边界

### 2.1 TCS（协同编排/下发）
- 输入：DesiredStrategy（来自 Brain/上游）+ RecentSignals + UnitState（可选，视闭环形态）
- 输出：
  - 面向每个 unit 的 `UTA`（topic：`tcn.unit.assignment.{unit_id}`)
  - 可选：`PlayStatus`（topic：`tcn.play.status.{play_id}`，用于可观测性/HMI）

### 2.2 ELU（单机自主执行）
- 输入：
  - `UTA`（本机任务/步进/约束/协同关系）
  - `tcn.signal`（协同信号：READY/FIRE_TURN/WEAPON_AWAY/...）
  - `tcn.status.platform.*`（平台物理真值，含本机与编队伙伴）
  - `tcn.status.weapon.*`（武器状态：导弹在飞等）
  - `tcn.topic.situation.evaluation`（融合态势/威胁评估）
  - `tcn.event.threat.destroyed`（威胁销毁/击杀确认事件）
- 输出：
  - `tcn.unit.state.{unit_id}`（本机战术执行逻辑真值）
  - `tcn.signal`（本机发出的协同信号）
  - `tcn.sim.actions.unit.{unit_id}`（动作指令：面向仿真/控制网关）

### 2.2.1 ABMS 级去中心化长机继任协议 (Succession Protocol)
- **触发背景**：强对抗、高威胁电子干扰环境下，长机（Team Lead / TCS 节点）可能面临断链、压制或战损。
- **配置与策略真源**：TCS 在 `UTA.task_meta` 中随任务下发有序的 **`succession_list`（长机继任序列，例如 `["G-01", "S-02", "S-03"]`）**。
- **判定与接管逻辑（ELU）**：
  - ELU 本地周期监控来自长机 `tcn.unit.state.{lead_unit_id}` 的上报间隔及 `tcn.signal` 的周期心跳。
  - 若长机状态信号丢失超过 `succession_timeout_ms`（默认 5000ms），ELU 触发继任协议：
    - 检查自身在 `succession_list` 中的优先级。
    - 若自身属于当前存活单元中的**第一顺位**，ELU 自动发布 L1 阶段控制信号 `LEAD_SUCCESSION_CLAIMED` 广播接管编队。
    - 提升自身角色（激活本地 TCS 决策实例），承担“协同编排”职责，并基于当前的 `slot_roster` 为其余僚机重新分发 `UTA`。
    - 若自身非第一顺位，则本地启动短暂计时器等待顺位更高者发送 `LEAD_SUCCESSION_CLAIMED`。若再次超时，则自动顺延判定。

### 2.2.2 ABMS 级自主度分级委派与安全门控 (LOA Gating)
- **自主度分级（Level of Autonomy, LOA）**：本契约标准化四级委派授权，并在 `UTA.execution_command` 下发，约束单机自主权限边界：
  - `LOA_1_MANUAL`（全人工/遥控模式）：ELU 不做本地战术决策，仅作为执行指令的转发通道，全部动作由上级或操作员直控。
  - `LOA_2_SEMI_AUTO`（半自主/人在回路确认模式）：ELU 负责本地自主机动与传感器搜索，但在释放武器（开火）等高危动作前，**MUST** 等待并校验 TCS 下发的交战授权信号 `WRA_AUTHORIZED`。
  - `LOA_3_AUTONOMOUS_EXEC`（自主执行模式）：ELU 在 UTA 约束和当前 ROE/ACM 包线内自主规划、机动、交战与有源自防御，无需实时控制，决策结果周期通过 `unit.state` 上报。
  - `LOA_4_FAIL_SAFE_AUTO`（全自主断链自愈模式）：仅在链路彻底中断且满足安全红线时，由 ELU 自发激活，按断链保护策略执行自防御突防或静默撤离。
- **安全门控（Local Gating Rules）**：
  - 即使处于 `LOA_3`，ELU 的动作解算也 **MUST** 本地硬校验当前的物理限制（如 `PlatformCapability.current_emcon_level` 为 A 时，本地拦截雷达开机；`weapon_status=HOLD` 时本地硬锁死开火动作）。

### 2.2.3 ABMS 强对抗断链突防自愈机制 (Link-Loss & Fail-Safe)
- **断链判定**：
  - 当 ELU 接收 `tcn.unit.assignment.{unit_id}`（UTA）的更新时差超过 `link_loss_timeout_ms`（默认 8000ms）时，本地标记 `link_state=DISCONNECTED`，进入“断链自愈”状态。
- **自愈执行策略（Link-Loss Action Policy）**：
  - 僚机根据 `UTA.execution_command.fallback_policy` 的行为树策略执行自愈：
    - **继续突防（Continue Intrusion）**：若断链前已越过交战开火边界且 ROE 为 FREE，允许自主使用被动红外（IRST）跟踪目标，在预定投放区完成弹药释放并撤离。
    - **安全撤离（Safe Egress）**：若尚未进入交战区，或平台健康度降低，ELU 立即切换至 `degraded_mode.fallback_behavior`，按最后有效 ACM 空域包线高度层退出，静默返航。

## 2.3 四层八维协同框架与五层信号体系（集成说明）

本节依据：
- `cmy_docs/设计文档/协同控制设计精要3.md`
- `cmy_docs/设计文档/四层八维协同框架工程实现评估.md`

目标：将“**四层八维协同框架**”与“**五层信号体系**”落实到当前可落地的 wire contract（NATS+JSON）上，形成**统一的语义索引**与**字段/Topic 映射**。

### 2.3.1 四层八维协同框架（四层+八维）

四层八维（不包含基础通信层）定义：
- 第一层：战术行动协同
  - 时间协同（2.1）
  - 空间协同（2.2）
  - 火力协同（2.3）
- 第二层：能力使用协同
  - 传感器协同（2.4）
  - 电磁协同（2.5）
  - 能量协同（2.6）
- 第三层：资源状态协同
  - 资源协同（2.7）
- 第四层：态势认知协同
  - 威胁协同（2.8）

基础层（通信信号协同，2.9）作为“协同语言/通信承载”，通过 `tcn.signal` 为核心通道实现。

### 2.3.2 四层八维 → Topic/字段映射（权威契约不改动，做语义映射）

说明：当前 `contracts-go` 的 UTA/Signal/UnitState/Actions 已经具备承载能力，但多数“协同维度”的字段以：
- 强类型字段（如 `tempo`、`spatial_constraints`、`engagement`、`payload`、`policies`）承载
- 或通过 `parameters` / `x` / `opaque` 扩展承载

本表用于约束“放哪里/怎么放”，避免不同团队把同一语义散落在多个 map 里。

#### A) 第一层：战术行动协同
- 时间协同（Time / 何时行动）
  - **UTA.TaskMeta.Tempo**：`uta.task_meta.tempo.mode/start_at_ms/window_ms/interval_ms/order`
  - **UTA.Navigation.Procedures + TrajectoryWaypoint.RTA/RequiredArrivalTime/TimeTolerance**：用于 RTA/时间窗口落点
  - **Signal（L2）**：`TOT_MINUS_5`、`WEAPON_AWAY` 等（通过 `tcn.signal`）

- 空间协同（Space / 何地行动）
  - **UTA.Navigation.SpatialConstraints.KeepOutZones**：禁入区/几何约束
  - **UTA.Navigation.FallbackTrajectory / TrajectoryProfile**：协同空间路径的 fallback
  - **Actions**：`go_to_location/follow_route` 作为执行层动作输出（注意：协同层定义“区域/容差”，不做微操）

- 火力协同（Firepower / 打击分配）
  - **UTA.TaskMeta.Relationships**：A射B导/射手-制导配对的硬约束（GUIDER/SHOOTER）
  - **UTA.Engagement**：weapon_status、primary_target、weapon_authorization、auto_engage_conditions
  - **Signal（L2）**：`WEAPON_AWAY`、`KILL_CONFIRMED/MISS_CONFIRMED`、`TARGET_DESTROYED`（可通过 threat_destroyed 输入转化）

#### B) 第二层：能力使用协同
- 传感器协同（Sensor / 感知分工）
  - **UTA.Payload.Sensors**：radar_state/radar_mode/scan_sector/designated_target_id
  - **Signal.payload.track**：用于协同共享 track（关键实现：GUIDER_READY 携带 track）

- 电磁协同（Electromagnetic / 频谱管理、EMCON）
  - **UTA.Payload.ElectronicWarfare**：jammer_state/mode/target_frequency_bands/coordination_params
  - **UTA.Payload.Communications + UTA.Policies**：emcon_level/report_frequency 等
  - **Signal（L3/L4）**：EMCON 切换、协同中止等事件通过 `tcn.signal` 通知

- 能量协同（Energy / 机动能力）
  - **UTA.Policies / UTA.ExecutionCommand.Parameters / UTA.Navigation.ProcedureSpec.Params**：承载 min_mach/min_specific_energy/low_energy_action 等策略参数
  - **UnitState.resources/diagnostics（扩展区）**：上报能量状态/阈值触发（生产建议：逐步强类型化）

#### C) 第三层：资源状态协同
- 资源协同（Resource / 燃油、弹药、健康）
  - **UTA.Policies.ResourceThresholds/AbortCriteria**：阈值（bingo_fuel/min_ammo/min_fuel 等）
  - **UnitState.resources/health**：上报燃油/弹药/健康（当前为 map 扩展区）
  - **Signal（L4）**：`FUEL_CRITICAL`、`ABORT_MISSION` 等异常处置信号

#### D) 第四层：态势认知协同
- 威胁协同（Threat / 统一评估与协同应对）
  - **SituationEvaluation（输入）**：`tcn.topic.situation.evaluation`
  - **ThreatDestroyed（输入）**：`tcn.event.threat.destroyed`
  - **Signal（L5/L4）**：`THREAT_WARNING`、`DATALINK_LOST`、`COORDINATION_ABORTED` 等
  - **UnitState.events/diagnostics（扩展区）**：上报威胁处置阶段、协同规避结果（生产建议：定义稳定 key 注册表）

### 2.3.3 五层信号体系（L1~L5）与 SignalMessage 映射

统一承载通道：`tcn.signal`（`contracts-go/signal/v1.SignalMessage`）。

五层定义（摘录自设计文档）：
- L1：阶段控制信号（State Machine Control）
- L2：战术动作信号（Tactical Action）
- L3：协同配合信号（Coordination）
- L4：异常处理信号（Exception Handling）
- L5：态势通报信号（Situation Awareness）

建议映射规则：
- `signal.signal_id`：信号名称（如 `TOT_MINUS_5`、`WEAPON_AWAY`、`PHASE_0_READY`)
- `signal.level`：填写 `L1/L2/L3/L4/L5`（字符串；若历史实现已占用该字段，可放入 `payload.enum["signal_level"]` 但推荐直用 level）
- `signal.kind`：建议填写该信号所属域（`PHASE/TEMPORAL/FIREPOWER/SENSOR/EMCON/ENERGY/RESOURCE/THREAT/...`)
- `signal.severity`：异常/告警类信号强建议填（如 `INFO/WARN/ERROR/CRITICAL`)
- `signal.dedupe_key`：生产级强建议必填，用于去重与幂等
- `signal.sequence`：生产级强建议单调递增（per-unit per-correlation）
- `signal.target.scope` + `signal.target.unit_ids`：广播/点对点区分

重发建议（源文档“关键信号重发3次”）：
- 对 L1/L4 的关键控制信号：建议由发送方进行有限次数重发，并以 `dedupe_key` 去重。

## 2.4 Topic 命名与环境隔离（最佳实践）

### 2.4.1 基本原则
- Topic MUST 能区分：
  - **环境（env）**：prod/test/sim
  - **数据来源（datasource）**：real/simulator
  - **消息类别（plane）**：assignment/signal/unit_state/actions/status/event

### 2.4.2 推荐 Topic 规范（建议作为未来统一命名）
- **Command Plane**：`tcn.cmd.{env}.unit.assignment.{unit_id}`
- **Signal Plane**：`tcn.signal.{env}`（广播域）
- **Unit State**：`tcn.state.{env}.unit.{unit_id}`
- **Actions**：`tcn.actions.{env}.{datasource}.unit.{unit_id}`
- **Platform/Weapon（物理真值）**：`tcn.truth.{env}.platform.{platform_id}` / `tcn.truth.{env}.weapon.{weapon_id}`
- **Situation Evaluation**：`tcn.situation.{env}.evaluation`
- **Threat Destroyed**：`tcn.event.{env}.threat.destroyed`

说明：
- 当前系统若仍使用旧 subject，MAY 通过桥接/转发逐步迁移；但新模块上线 SHOULD 直接采用推荐规范。

## 3. Topic/消息清单（权威）

### 3.1 TCS -> ELU（Command）
- **tcn.unit.assignment.{unit_id}**
  - **消息体**：`contracts-go/uta/v1.UnitTaskAssignment`
  - **用途**：对单机下发 per-slot 任务、步进指针、协同关系、导航/交战/策略约束

### 3.2 ELU -> TCS（Event & State）
- **tcn.unit.state.{unit_id}**
  - **消息体**：`contracts-go/unitstate/v1.UnitTacticalState`
  - **用途**：对外发布单机的“逻辑真值”（当前步、等待原因、退化模式、最近转移等）
  - **约束**：`header.strategy_version` 必须 > 0（未接受到 UTA 前 ELU 会跳过发布）

- **tcn.signal**
  - **消息体**：`contracts-go/signal/v1.SignalMessage`
  - **用途**：协同信号总线（READY / FIRE_TURN / WEAPON_AWAY / KILL_CONFIRMED 等）

### 3.3 ELU -> 仿真/控制网关（Actions）
- **tcn.sim.actions.unit.{unit_id}**
  - **消息体**：`shared-go/actionscontract.ActionEnvelopeV2`
  - **用途**：动作指令（导航/传感器/开火/导弹改制导等），由 gateway 负责 JSON->Proto 转换与语义合并

### 3.4 可观测性（Debug/Status）
- **tcn.play.status.{play_id}**（可选）
  - **消息体**：`contracts-go/playstatus/v1.PlayStatus`
  - **用途**：TCS 发布 play 的宏观状态（ACTIVE/BLOCKED/ABORTED）供 HMI/日志对齐

### 3.5 ELU 订阅的外部输入（Physical/Global Truth）
- **tcn.status.platform.{platform_id}**
  - **消息体**：`contracts-go/platformstate/v1.PlatformState`
  - **来源**：bus-bridge

- **tcn.status.weapon.{weapon_id}**
  - **消息体**：`contracts-go/weaponstate/v1.WeaponState`
  - **来源**：bus-bridge / 上游数据面

- **tcn.topic.situation.evaluation**
  - **消息体**：`contracts-go/situation/v1.SituationEvaluation`
  - **来源**：融合/评估服务（例如 kg-go）

- **tcn.event.threat.destroyed**
  - **消息体**：`contracts-go/threat/v1.ThreatDestroyed`
  - **来源**：数据面推理（例如 kg-go），用于 kill confirmed

## 4. 结构体契约详解（字段级）

> 说明：以下“字段说明”以 `contracts-go` 为权威。本文档不重复所有字段的解释性注释，仅补充“跨服务语义/约束/常见坑”。

### 4.1 `common.Header`（TCS-facing 通用头）
来源：`services/contracts-go/common/header.go`

字段：
- `message_id`：消息唯一标识（字符串）。建议 UUID。
- `timestamp`：毫秒时间戳。
- `source`：来源服务标识（例：`elu` / `tcs` / `brain`）。
- `schema_version`：语义版本（例：`1.0.0`）。
- `strategy_version`：策略版本（unit.state 强制要求 >0；其它消息可选）。
- `correlation_id`：同一战术闭环内的关联 ID（跨消息对齐用）。
- `trace_id`：链路 trace id（可与 NATS header `trace_id` 对齐）。

### 4.2 `common.BusHeader`（数据面总线头）
用于 platform/weapon 等物理真值消息：字段名与 TCS-facing 不一致，属于“上游既有格式”。

### 4.3 `uta/v1.UnitTaskAssignment`（TCS -> ELU）
来源：`services/contracts-go/uta/v1/unit_task_assignment.go`

#### 4.3.1 顶层结构
- `header`：`uta/v1.MessageHeader`（注意：该 header 与 `common.Header` **不是同一个结构**）
- `task_meta`：任务元信息（play_id、tactic_template_id、role_slot、slot_roster、relationships、tempo 等）
- `execution_command`：执行控制面（step 指针 `current_step_id`、coordination 语义、parameters 动态注入等）
- `navigation`：导航/机动/程序集合（`procedures` step->ProcedureSpec）
- `sync_config`：步进同步图（transition gate AST 等）
- `payload`：传感器/电战/通信指令（可选）
- `engagement`：交战授权/主目标/发射约束（可选）
- `policies`：生存/资源阈值/退化模式等（可选）

#### 4.3.2 关键语义与强约束
- **`execution_command.current_step_id` 必须存在于 `navigation.procedures`**
- **去中心化继任规则**：
  - `task_meta.succession_list`：长机继任序列（有序 `string[]`），声明首选、备选继任单元。
  - ELU 需监控来自该序列中排在自己之前的单元的心跳。当排在自己之前的全部失联时，触发角色晋升和本地决策引擎。
- **委派自主度 (LOA) 控制**：
  - `execution_command.loa_level`：可配置为 `"LOA_1_MANUAL"` / `"LOA_2_SEMI_AUTO"` / `"LOA_3_AUTONOMOUS_EXEC"` / `"LOA_4_FAIL_SAFE_AUTO"`。
  - ELU 的决策判定（如是否自主开火、是否自主改出、是否自主重组等）**MUST** 受该级别硬约束管控。
- **协同关系**
  - `task_meta.slot_roster`：play 级角色槽 -> unit_id 列表
  - `task_meta.relationships`：针对当前 unit、当前目标的硬约束（GUIDER/SHOOTER 配对）
  - ELU 要求：当 required relationship 缺失时，必须拒绝关键动作/READY/FIRE（硬失败而非软 fallback）
- **动态参数注入与动态 ROE 校验**：`execution_command.parameters`
  - 例：TCS 在协调计划存在时，会为每个 unit 注入 `parameters["target.name"]`。
  - **动态交战规则（ROE）覆写（ABMS 级）**：
    - `parameters["roe.max_firing_range_km"]`：最大开火距离（覆写机载默认）。
    - `parameters["roe.restricted_zones"]`：禁击区/无打击物理实体集合。
    - `parameters["roe.active_iff_required"]`：是否强制要求敌我识别双重比对通过方能释放武器。
- **sync_config.gates**：门控条件由 ELU 执行（SERIAL 模式下由 ELU 自主推进）

### 4.4 `signal/v1.SignalMessage`（ELU/TCS 双向）
来源：`services/contracts-go/signal/v1/signal.go`

- topic：`tcn.signal`
- `header`：`common.Header`
- `signal`：
  - `signal_id`：信号类型（字符串枚举，现为开放集）
  - `dedupe_key`：去重 key（可选，但实装建议强制使用，见第 6 节）
  - `sequence`：序列号（可选）
  - `source`：发送方上下文（unit_id/play_id/step_id/role_slot 等）
  - `target`：作用域（scope/unit_ids/play_id 等）
  - `payload`：
    - `enum`/`numeric`：轻量键值对
    - `track`：`TrackSnapshot`（强类型轨迹快照，**用于编队协同共享 track**）
      - **ABMS 感知协同增强字段（MUST）**：
        - `confidence`：目标探测/融合可信度（0~1）。
        - `covariance[]`：多维不确定性协方差矩阵（浮点数组，供射手机载火控解算射击包线）。
        - `sensor_type`：传感器物理源（`RADAR`/`IRST`/`ESM`/`FUSED`）。
        - `emcon_level`：源节点辐射水平，便于僚机在被动跟踪下保持射手静默。
    - `opaque`：扩展字段（非强约束）

**现状关键实现点（ELU）**：
- ELU 收到 `GUIDER_READY` 时，会从 `payload.track` 抽取并注入 WorldModel，解决“数据链路断裂”下的被动制导。

### 4.5 `unitstate/v1.UnitTacticalState`（ELU -> 外部）
来源：`services/contracts-go/unitstate/v1/unit_state.go`

- topic：`tcn.unit.state.{unit_id}`
- `header`：`common.Header`（强制 `strategy_version>0`）
- `unit`：unit_id/team_id/platform_id
- `task`：从 UTA 派生的 task ref（play_id/tactic_template_id/role_slot/priority 等）
- `execution`：
  - `coordination_mode`：`SERIAL`（推荐）或 `STEP_LOCK`（兼容/废弃）
  - `current_step_id`：对外可见的当前步（TCS 仲裁/观测依赖）
  - `waiting` / `last_transition` / `degraded`：闭环调试与仲裁关键字段
  - **ABMS 韧性运行状态上报（新增）**：
    - `link_state`：`CONNECTED`（连通） / `DISCONNECTED`（断链，自发激活 fallback 状态）。
    - `succession_status`：`MEMBER`（正常僚机成员） / `PROMOTING`（晋升中） / `LEADER_ACTIVE`（已自主提升为长机并托管编队）。
- `events`：一次性事件数组（ELU 发布成功后会清空，避免重复）
- `diagnostics`：诊断扩展（包含 last_assignment_ts、self kinematics、last_setpoint 等）

### 4.6 `actionscontract.ActionEnvelopeV2`（ELU -> Gateway）
来源：`services/shared-go/actionscontract/contract.go` + `payloads.go`

- topic：`tcn.sim.actions.unit.{unit_id}`（默认；本质是“actions out”总线，命名可被配置覆盖）
- `schema_version`：`2.x.y`
- `timestamp_ms`
- `trace_id`
- `source`：`service=elu unit_id team_id`
- `target.agent_id`：严格 one-agent
- `x.commands[]`：业务级动作列表
  - `kind`：动作类型（go_to_location / fire_at_target / missile_retarget / change_commander / ...）
  - `args`：对应 payload（JSON raw）

**动作语义合并**：
- Continuous 类（desired_heading/go_to_location/follow_route 等）允许网关“last-wins 合并”。
- Discrete 类（fire/cue/retarget 等）不得被 ELU 发布限流跳过。

### 4.7 `platformstate/v1.PlatformState`（bus-bridge -> 总线）
- topic：`tcn.status.platform.{platform_id}`
- header：`common.BusHeader`
- 字段：platform_id、lla、heading、velocity、fuel 等

### 4.8 `weaponstate/v1.WeaponState`（bus-bridge -> 总线）
- topic：`tcn.status.weapon.{weapon_id}`
- header：`common.BusHeader`
- 字段：weapon_id、host_id、current_target、mode/effect、lla/velocity 等

### 4.9 `situation/v1.SituationEvaluation`（融合评估 -> 总线）
- topic：`tcn.topic.situation.evaluation`
- 注意：当前字段存在大写 key（`Generated/Platforms/Platform/ThreatID/...`），这是上游既有格式。

### 4.10 `threat/v1.ThreatDestroyed`（推理 -> 总线）
- topic：`tcn.event.threat.destroyed`
- 用于 kill confirmed 输入（ELU 内部会将其转化为协同信号的一部分逻辑）

## 5. 典型消息流（最小闭环）

### 5.1 “TCS 下发 UTA -> ELU 执行 -> 信号闭环”
1) TCS 发布：`tcn.unit.assignment.{unit_id}`（UTA）
2) ELU 接收并更新本机 assignment store
3) ELU 执行 step graph（SERIAL），到达 gate/emission 时发布：`tcn.signal`
4) TCS/其他 ELU 订阅 `tcn.signal`，驱动协同与下一轮编译
5) ELU 周期发布：`tcn.unit.state.{unit_id}`（必须 strategy_version>0 且 current_step_id 非空）

### 5.2 “导弹制导协同（示例）”
- SHOOTER/ GUIDER 通过 UTA.relationships 建立硬配对
- GUIDER_READY 信号携带 `payload.track`，SHOOTER 侧注入 worldmodel，用于选择目标/发射/改制导

### 5.3 “长机战损/失联后的去中心化自主继任协议（ABMS 级）”
1) 长机（G-01）心跳在 NATS 通道 `tcn.state.prod.unit.G-01` 丢失超过 5 秒。
2) 僚机（S-02）本地检测自身在 `succession_list` 中的顺位为第二（第一顺位已失联或未上线）。
3) S-02 发布 L1 控制信号：`LEAD_SUCCESSION_CLAIMED` (topic: `tcn.signal.prod`)，内容包含自身 `unit_id="S-02"` 及 `succession_version`。
4) 其余僚机（S-03）接收信号，在本地将策略长机源指针从 `G-01` 变更为 `S-02`。
5) S-02 的本地 TCS 引擎激活，向 NATS 发送新的 `UTA`（topic: `tcn.cmd.prod.unit.S-03`），使 S-03 能够在新主控下继续飞行。
6) S-02 的 `unit.state` 状态变更：`succession_status="LEADER_ACTIVE"`。

### 5.4 “断链突防下的自主交战与自愈（ABMS 级）”
1) 僚机（S-03）由于强电子干扰导致接收 `tcn.cmd.prod.unit.S-03` 连续超时超过 8 秒。
2) S-03 标记自身状态为 `link_state="DISCONNECTED"`。
3) S-03 评估本地武器库及攻击距离：若当前正处于交战包线且具有高可信目标，由于 `fallback_policy` 为继续突防，其调用动作：
   - 激活被动红外（IRST）传感器：通过 `ActionEnvelopeV2` 发送 `ChangeSensorModeArgs`。
   - 自主规划终端机动：发布 `GoToLocationArgs` 引导飞机到达投放点。
   - 自主投放：发布 `FireAtTargetArgs`。
4) 投放完成后，S-03 切换至 `degraded_mode.fallback_behavior="SAFE_EGRESS"`，根据机载高度层和当前 EMCON-A 限制静默退出。

## 6. 生产级最佳实践规范（必须遵循）

### 6.1 幂等与去重（MUST）
- **Signal MUST 提供 `dedupe_key`**，消费者 MUST 以 dedupe_key 去重。
  - 推荐构造：`{correlation_id}:{source.unit_id}:{signal_id}:{source.step_id}:{shot_id?}`
- **Signal SHOULD 提供 `sequence`**，并在 `unit_id + correlation_id` 维度下单调递增。
- **关键控制类信号（L1/L4）MUST 支持有限重发**：
  - 发送方 SHOULD 重发 1~3 次。
  - 接收方 MUST 幂等处理（dedupe_key 去重）。

### 6.2 版本一致性与单调规则（MUST）
- **strategy_version MUST 单调递增**（同一 unit 的策略序列）。
- **UnitTacticalState.header.strategy_version MUST > 0**。
- **ELU MUST 拒绝回滚的 strategy_version**（除非明确进入回放/仿真模式，并在 header.source/datasource 标注）。
- **UTA 的变化分类（推荐）**：任何会导致行为改变的变化都 MUST 递增 strategy_version（避免“软变化”引入不可观测分歧）。

### 6.3 信号分层与框架映射（MUST）
- Signal.level MUST 使用五层信号体系：L1/L2/L3/L4/L5。
- Signal.kind SHOULD 对齐八维协同维度：TEMPORAL/SPATIAL/FIREPOWER/SENSOR/EMCON/ENERGY/RESOURCE/THREAT。
- 对于跨层关键动作（例如“火力协同中的改制导”），SHOULD 同时发布：
  - L2（动作发生）+ L3（协同告知）或 L4（异常处置）信号，具体由任务模板定义。

### 6.4 开放字段治理（MUST）
- `UTA.execution_command.parameters`、`Signal.payload.opaque`、`UnitTacticalState.diagnostics/resources/...` 等开放字段：
  - MUST 使用命名空间（推荐 `dot.case`，例如 `target.name`、`missile.asset_id`）。
  - MUST 提供 key 注册表（key、类型、必填性、来源、消费者）。
  - SHOULD 逐步强类型化进入 proto/schema。

### 6.5 Actions 规范（MUST/SHOULD）
- Actions Envelope MUST 满足：`target.agent_id` 唯一（一个 envelope -> 一个 agent）。
- Continuous 类动作（heading/altitude/velocity/go_to/follow_route）MAY 被网关 last-wins 合并。
- Discrete 类动作（fire/retarget/change_commander/agent_control）MUST 不被合并覆盖。
- 所有动作 MUST 能在 JSON Schema 中被校验（kind 与 args 的匹配）。

### 6.6 ABMS 级去中心化协同安全红线 (ABMS Safety Redlines - MUST)
- **多机去中心化防脑裂（Split-Brain Prevention）**：在发生长机失联时，僚机在自动触发 Succession 流程时，**MUST** 严格按下发的 `succession_list` 静态顺位判定。在未收到前序顺位明确的 `LEAD_SUCCESSION_CLAIMED` 声明前，非第一顺位僚机绝不允许自发宣告提升为长机，防止多个长机并存导致战术冲突。
- **高时延与断链信号实效性（Stale Signal Expiration）**：由于干扰和多路径延迟，重入网可能导致“僵尸信号”或“过期指令”到达（如突防期间积压的信号在断链恢复后一次性到达）。所有 `tcn.signal` 和 `UTA` **MUST** 在本地进行时效性校验：
  - 任何到达的信号或策略，若其 `header.timestamp_ms` 与当前机载 GPS / PTP 授时时间相差超过 `30000ms`（30秒），ELU **MUST** 本地丢弃该消息。
- **本地 ROE 与电磁状态硬门禁**：即使 ELU 处于 `LOA_3` 或 `LOA_4`，当物理处于辐射管制（EMCON Level A）或敌我识别（IFF）系统离线（`iff_ok == false`）时，机载飞控与武器层（FCS 面）**MUST** 对 ELU 解算出的主动雷达制导/武器释放指令实施本地拦截，作为物理安全的最后底线。

## 7. 附：示例 JSON（节选）

### 7.1 UTA（ABMS 级增强骨架示例）
```json
{
  "header": {
    "message_id": "uta-001",
    "timestamp_ms": 1730000000000,
    "strategy_version": 1,
    "correlation_id": "C1"
  },
  "task_meta": {
    "play_id": "P1",
    "tactic_template_id": "T1",
    "role_slot": "SLOT_SHOOTER",
    "slot_roster": {
      "SLOT_SHOOTER": { "items": ["S-02"] },
      "SLOT_GUIDER": { "items": ["G-01"] }
    },
    "relationships": [
      {
        "kind": "GUIDER_FOR_TARGET",
        "target_name": "K1",
        "peer_unit_id": "G-01",
        "primary": true
      }
    ],
    "succession_list": ["G-01", "S-02", "S-03"],
    "tempo": { "mode": "NONE" }
  },
  "execution_command": {
    "coordination": {
      "pointer_model": "PER_SLOT",
      "sync_mechanism": "SIGNAL_ONLY",
      "signal_scope": "TACTIC_INSTANCE"
    },
    "current_step_id": "S_STEP_1",
    "loa_level": "LOA_3_AUTONOMOUS_EXEC",
    "parameters": {
      "target.name": "K1",
      "shot.id": "SHOT-001",
      "roe.max_firing_range_km": 85.0,
      "roe.active_iff_required": "true"
    }
  },
  "navigation": {
    "planning_mode": "EXECUTE_TRAJECTORY",
    "procedures": {
      "S_STEP_1": { "objective_type": "ENGAGE", "procedure_type": "FIRE" }
    }
  },
  "sync_config": {
    "transitions": [
      {
        "from_step": "S_STEP_1",
        "to_step": "S_STEP_2",
        "gate": { "type": "SIGNAL", "signal_id": "GUIDER_READY" },
        "timeout": {
          "timeout_ms": 15000,
          "on_timeout": { "kind": "TRIGGER_FALLBACK" }
        }
      }
    ]
  },
  "coordination_tags": [
    {
      "layer": "TACTICAL_ACTION",
      "dimension": "FIREPOWER",
      "sub_kind": "A_SHOOT_B_GUIDE"
    }
  ]
}
```

### 7.2 Signal（ABMS 级高置信度感知 Track 示例）
```json
{
  "header": {
    "message_id": "sig-001",
    "timestamp_ms": 1730000000000,
    "source": "elu",
    "schema_version": "1.0.0",
    "strategy_version": 1,
    "correlation_id": "C1"
  },
  "signal": {
    "signal_id": "GUIDER_READY",
    "level": "L3",
    "kind": "SENSOR",
    "severity": "INFO",
    "dedupe_key": "C1:G-01:GUIDER_READY:G_STEP_WAIT:SHOT-001",
    "sequence": 12,
    "source": {
      "unit_id": "G-01",
      "play_id": "P1",
      "step_id": "G_STEP_WAIT"
    },
    "target": { "scope": "TACTIC_INSTANCE", "play_id": "P1" },
    "payload": {
      "track": {
        "track_id": "g01:2",
        "target_name": "K1",
        "lla": [30.0, 120.0, 8000.0],
        "quality": 0.9,
        "confidence": 0.98,
        "covariance": [15.2, 0.0, 0.0, 0.0, 15.2, 0.0, 0.0, 0.0, 30.5],
        "sensor_type": "FUSED",
        "emcon_level": "B"
      }
    }
  }
}
```

### 7.3 Actions（ELU -> Gateway；连续控制 + 离散动作 + 改制导）

#### 7.3.1 连续控制（可合并 last-wins）
```json
{
  "schema_version": "2.1.0",
  "timestamp_ms": 1730000000500,
  "trace_id": "T-abc",
  "source": { "service": "elu", "unit_id": "S-02", "team_id": "0" },
  "target": { "agent_id": "S-02" },
  "x": {
    "mode": "AUTO",
    "commands": [
      {
        "kind": "go_to_location",
        "args": {
          "latitude": 30.01,
          "longitude": 120.02,
          "altitude_m": 8000,
          "speed_ms": 250
        }
      },
      { "kind": "desired_heading", "args": { "heading_deg": 95.0 } }
    ]
  }
}
```

#### 7.3.2 离散动作（不得被合并覆盖）
```json
{
  "schema_version": "2.1.0",
  "timestamp_ms": 1730000000600,
  "source": { "service": "elu", "unit_id": "S-02" },
  "target": { "agent_id": "S-02" },
  "x": {
    "commands": [
      { "kind": "fire_at_target", "args": { "track_id": "g01:2" } }
    ]
  }
}
```

#### 7.3.3 改制导（导弹在飞）
```json
{
  "schema_version": "2.1.0",
  "timestamp_ms": 1730000000700,
  "source": { "service": "elu", "unit_id": "G-01", "team_id": "0" },
  "target": { "agent_id": "G-01" },
  "x": {
    "commands": [
      {
        "kind": "missile_retarget",
        "args": {
          "asset_id": "missile_001",
          "team": 0,
          "new_target_id": "xq58a_001:2",
          "coordination_notify": true
        }
      }
    ]
  }
}
```

### 7.4 UnitState（ABMS 级高韧性状态骨架）
```json
{
  "header": {
    "message_id": "state-001",
    "timestamp_ms": 1730000001000,
    "source": "elu",
    "schema_version": "1.0.0",
    "strategy_version": 1,
    "correlation_id": "C1"
  },
  "unit": { "unit_id": "S-02", "team_id": "0" },
  "execution": {
    "coordination_mode": "SERIAL",
    "current_step_id": "S_STEP_2",
    "status": "WAITING",
    "link_state": "CONNECTED",
    "succession_status": "MEMBER"
  }
}
```

---

**治理要求**：
- 本目录三份产物（文档/proto/schema）MUST 同步更新并保持一致。
- 每次新增 signal_id/action kind/parameters key MUST 同步更新：
  - 文档（规范条款+示例）
  - proto（字段注释/枚举或注册表说明）
  - JSON Schema（required/约束/examples）
