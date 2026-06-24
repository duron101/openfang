# 单机自主（ELU）与飞控系统（FCS）交互契约（标准文档）

> **契约版本**：v2.2（Forward-only 增量，不修改既有字段语义）  
> **v2.2 新增**（与 `ELU功能单元和API定义.md` v2.2 同步评审）：
>
> - 动作字典新增 `arm_reflex_profile`（自卫反射弧策略武装，§4.2）；火力类动作 args 新增可选 `pre_delegation_id`（断链预授权交战令牌透传）。
> - `ActuationAck` 新增拒绝码 `ACTUATION_REJECT_REASON_PREDELEGATION_INVALID`、`ACTUATION_REJECT_REASON_REFLEX_UNSUPPORTED`（§4.3）。
> - `FcsHealth` 新增导航退化块 `nav{nav_mode, estimated_cep_m, drift_rate_m_s}` 与反射弧状态块 `reflex{armed_profile_id, last_event}`（§4.5）。
> - `PlatformCapability` 新增 `weapon_stores[].cost_class`（弹药成本档，经济性门控 SSOT）与 `reflex_capability{supported_profiles[], max_decoy_quota}`（§4.6）。
> - 新增 §6.7 消息认证与零信任（`auth_tag`）、§6.8 反射弧职责治理。

## 1. 概述

本文档定义并约束 **单机自主执行单元（Execution Logic Unit, ELU）** 与 **飞行控制系统 / 机载控制网关（Flight Control System, FCS / Control Gateway）** 之间的交互契约。

**定位（重要）**：本文档是工程上可落地的 **最佳设计规范（Normative Spec）**，不是对当前实现的总结。它是 `ELU_TCS` 契约中"动作面（Action Plane）+ 物理真值面（Physical Truth Plane）"在 **实装飞控边界** 的专门化与收敛。

**与 `ELU_TCS` 契约的关系**

- `ELU_TCS` 解决 **协同面**：TCS（编队协同）↔ ELU 的任务委派/信号/状态（`UTA`/`tcn.signal`/`unit.state`）。
- 本契约解决 **执行面**：ELU ↔ FCS 的动作下发/物理真值回传/控制权交接。
- `ELU_TCS` 中的 **Gateway（仿真/控制网关）** 在实装中即 **FCS 适配器**；仿真器（AFSIM/ArkSim）与真实飞控是同一抽象边界的两个适配器实现（端口-适配器 / HAL 模式）。

本规范基于 **ABMS（先进战场管理系统）** 架构要求进行升级，全面覆盖以下执行面与协同面边界：

- **ELU -> FCS** 的动作面（Action Plane）：`ActionEnvelopeV2`，下发控制包线范围内的指令。
- **FCS -> ELU** 的物理真值面（Truth Plane）：`PlatformState`、`WeaponState`，提供可置信、可仲裁的世界模型基础。
- **FCS -> ELU** 的回执面（Ack Plane）：`ActuationAck`，新增 **ABMS 规则链异常码（RejectReason）**，在飞控边界强拦截违反 ROE/ACM/EMCON 的指令。
- **ELU <-> FCS** 的控制权面（Control Authority Plane）：`ControlAuthorityState`，支持 **飞行/火力/传感器/电子战四面分级托管** 与人机协同（HMT）。
- **FCS -> ELU** 的健康面（Health Plane）：`FcsHealth`，集成 IFF 状态、高精度授时同步（Time Sync）及链路/总线质量。
- **FCS -> ELU** 的能力面（Capability Plane）：`PlatformCapability`，集成 **结构化弹载清单（WeaponStore）**、当前 EMCON（电磁管制）级别与活动 IFF 模式。

**权威契约来源（Single Source of Truth）**

- 本目录三份产物构成"契约主规范"（与 `ELU_TCS` 同治理）：
  - `ELU_FCS_交互契约_标准文档.md`（本文件：规范条款）
  - `elu_fcs_contracts.proto`（跨语言契约索引 + 强类型语义）
  - `elu_fcs_contracts.schema.json`（JSON wire 校验 + 示例 + required/约束；**待生成，与本文件同步**）

实现可以滞后，但**不得反向修改规范以迁就实现**。

## 1.1 规范性用语

采用 RFC 2119 风格：**MUST**（必须）/ **SHOULD**（强烈建议）/ **MAY**（可选）。

## 1.2 演进与治理（核心原则）

- **Forward-only（只增不改）**：所有契约（proto/schema/示例）只允许新增字段；禁止重命名/复用语义/复用字段号。
- **Schema 可校验**：所有 wire 消息在生产环境 MUST 通过对应 JSON Schema 校验（动作面 `kind` 与 `args` 必须匹配）。
- **契约注册表**：开放字段（`x`/`opaque`/`aux` 等）MUST 配套 key 注册表（命名、类型、必填性、来源、消费者）。
- **可仲裁真值**：`PlatformState`/`WeaponState` 是 FCS 对 ELU 发布的物理真值，是 ELU 世界模型的权威输入；ELU 的派生量（占位、机动解算）不得反向写回为真值。
- **平台无关红线**：ELU 决策核 MUST 只依赖本契约抽象，**禁止**直接 `import` 任何具体飞控 SDK / `aceproto` / `afsimproto`；适配差异全部收敛在 FCS 适配器内。

## 2. 角色与职责边界

### 2.1 ELU（单机自主执行）

- 输入（订阅）：
  - `fcs.truth.platform.{platform_id}`（本机物理真值：位置/姿态/三维速度/燃油，符合 WGS-84 与 MSL 授时标准）
  - `fcs.truth.weapon.{weapon_id}`（武器/在飞弹状态，支持中制导/改制导追踪）
  - `fcs.ack.unit.{unit_id}`（动作回执：包含 ABMS 规则层拦截拒绝码与 command_id）
  - `fcs.health.unit.{unit_id}`（平台健康状态：新增 敌我识别 IFF 状态、高精度授时同步时钟差）
  - `fcs.capability.unit.{unit_id}`（平台能力声明：新增 弹药库 WeaponStore、EMCON 辐射级别限制、活动 IFF 模式）
- 输出（发布）：
  - `fcs.actions.unit.{unit_id}`（`ActionEnvelopeV2` 动作指令，携带幂等 command_id）
- 职责：消费物理真值与健康包线 + TCS 任务（来自 `ELU_TCS`），动态评估约束，解算并下发合规的动作指令；维护飞行/火力/传感器/电战控制权状态机；链路降级或冲突时进入 fail-safe。

### 2.2 FCS / 控制网关（执行与物理真值）

- 输入：`ActionEnvelopeV2` 动作指令。
- 输出：物理真值（`PlatformState`/`WeaponState`）、动作回执（`ActuationAck`）、健康（`FcsHealth`）、能力（`PlatformCapability`）。
- 职责：把平台无关动作翻译为具体物理作动/有源辐射/武器管理/敌我转发指令；执行连续动作合并与离散动作去重；校验动作是否违反当前 ROE/ACM/EMCON 规则，若有违例进行物理拦截并返回拒绝 Ack；在 ELU 失联或越权时按 fail-safe 安全收回各控制面。
- **反射弧职责（v2.2）**：对毫秒级自卫反射（激光告警→光学快门、导弹逼近告警→自动箔条/规避），FCS 在 ELU 预先武装的反射策略（`arm_reflex_profile`）授权范围内**自主触发执行**，不等待 ELU 软件环路；事后 MUST 经 `FcsHealth.reflex.last_event` 回报触发类型、时刻与诱饵消耗。

### 2.3 职责边界红线（MUST）

- FCS **不做战术决策**：只执行/回报，不自行选择目标、不自行规划任务。
- ELU **不绕过 FCS** 直接驱动作动器：所有物理控制 MUST 经 `ActionEnvelopeV2`。
- 控制权 **同一时刻唯一**：ELU 与人/自动驾驶/上层不得同时控制同一 agent（见 §6.3）。
- **反射弧边界（v2.2）**：毫秒级自卫反射的**执行权**属 FCS 域，**策略权**属 ELU（经 `arm_reflex_profile` 武装/解除）；FCS 不得执行未武装类别的反射动作，ELU 不得在软件 tick 环路内仿冒硬件反射；反射动作 MUST NOT 包含任何硬杀伤武器释放。
- **预授权令牌边界（v2.2）**：`pre_delegation_id` 的签发权属 TCS（经 `ELU_TCS`），ELU 仅校验与透传，FCS 仅做存在性/有效期二次校验（纵深防御），三方均不得本地签发或续期令牌。

## 2.4 Topic 命名与环境隔离（最佳实践）

### 2.4.1 基本原则

Topic MUST 能区分：**环境（env：prod/test/sim）**、**数据来源（datasource：real/simulator）**、**消息类别（plane：actions/truth/ack/health/capability）**。

### 2.4.2 推荐 Topic 规范

- **Action Plane**：`fcs.actions.{env}.{datasource}.unit.{unit_id}`
- **Truth Plane**：`fcs.truth.{env}.platform.{platform_id}` / `fcs.truth.{env}.weapon.{weapon_id}`
- **Ack Plane**：`fcs.ack.{env}.unit.{unit_id}`
- **Health Plane**：`fcs.health.{env}.unit.{unit_id}`
- **Capability Plane**：`fcs.capability.{env}.unit.{unit_id}`

说明：旧系统若使用 `tcn.sim.actions.unit.`* / `tcn.status.platform.*` 等 subject，MAY 通过桥接逐步迁移；新模块 SHOULD 直接采用本规范。

## 3. Topic / 消息清单（权威）

### 3.1 ELU -> FCS（Action）

- **fcs.actions.unit.{unit_id}**
  - 消息体：`ActionEnvelopeV2`
  - 用途：导航/传感器/开火/电战/改制导/控制权切换等执行级动作。

### 3.2 FCS -> ELU（Truth）

- **fcs.truth.platform.{platform_id}**
  - 消息体：`PlatformState`
  - 来源：飞控/惯导/燃油系统。
- **fcs.truth.weapon.{weapon_id}**
  - 消息体：`WeaponState`
  - 来源：武器管理系统（SMS）/导弹数据链。

### 3.3 FCS -> ELU（Ack）

- **fcs.ack.unit.{unit_id}**
  - 消息体：`ActuationAck`
  - 用途：对一帧动作的受理/拒绝回执（含 `launch_request_id` 绑定、拒绝原因）。

### 3.4 FCS -> ELU（Health & Capability）

- **fcs.health.unit.{unit_id}**：`FcsHealth`（链路/总线/作动健康，用于 fail-safe 判定）。
- **fcs.capability.unit.{unit_id}**：`PlatformCapability`（可用武器/传感器/机动包线；ELU 据此裁剪动作空间）。

## 4. 结构体契约详解（字段级）

> 字段语义以 `elu_fcs_contracts.proto` 为权威。本节补充跨服务语义/约束/常见坑。

### 4.1 `ActionEnvelopeV2`（ELU -> FCS）

（沿用 `ELU_TCS` 既有定义，保持跨边界一致）

- `schema_version`：MUST 以 `"2."` 开头（例如 `"2.1.0"`）。
- `timestamp_ms`：动作产生时刻（毫秒）。
- `trace_id`：链路追踪 ID（可选）。
- `source`：`{service="elu", unit_id, team_id}`。
- `target.agent_id`：**MUST 唯一**（一个 envelope -> 一个 agent）。
- `x.commands[]`：动作列表（见 §4.2）。

### 4.2 动作字典（`ActionCommand.kind` → 语义；左列与 NGBM `actions.py` / `ELU_TCS` ActionKindEnum 对齐）


| kind                                                 | 类别         | 语义                 | args 关键字段                                                                                                                 |
| ---------------------------------------------------- | ---------- | ------------------ | ------------------------------------------------------------------------------------------------------------------------- |
| `agent_control`                                      | Discrete   | 控制权 set/release    | `action: set_agent_outside_control | release_outside_control`                                                             |
| `desired_heading`                                    | Continuous | 航向                 | `heading_deg`,`speed_ms?`,`turn_direction?`                                                                               |
| `desired_altitude`                                   | Continuous | 高度                 | `altitude_m`,`rate_ms?`                                                                                                   |
| `desired_velocity`                                   | Continuous | 速度                 | `speed_ms`,`linear_accel?`                                                                                                |
| `go_to_location`                                     | Continuous | 飞向点                | `latitude`,`longitude`,`altitude_m`,`speed_ms?`                                                                           |
| `follow_route`                                       | Continuous | 航路                 | `route_name`,`waypoints[]`                                                                                                |
| `cue_to_target`                                      | Discrete   | 传感器引导              | `track_id`,`component_id?`                                                                                                |
| `sensor_action` / `change_sensor_mode`               | Discrete   | 传感器操作/模式           | `action`/`mode`,`component_id?`                                                                                           |
| `fire_at_target`                                     | Discrete   | 开火                 | `track_id`,`component_id?`,`pre_delegation_id?`(v2.2 断链预授权令牌)                                                             |
| `fire_slavo_at_target`                               | Discrete   | 齐射                 | `track_id`,`slavo_size`,`component_id?`,`pre_delegation_id?`(v2.2)                                                        |
| `fire_chaff`                                         | Discrete   | 箔条                 | `number_parcels`,`drop_interval`,`ejectors?`                                                                              |
| `start_jamming`/`stop_jamming`/`change_jamming_mode` | Discrete   | 电子战                | `mode{frequency,bandwidth,beam_number}`,`track_id?`                                                                       |
| `missile_terminal_activate`                          | Discrete   | 末制导激活              | `asset_id`,`team`,…                                                                                                       |
| `missile_retarget`                                   | Discrete   | 在飞弹改制导             | `asset_id`,`team`,`new_target_id`,`coordination_notify?`                                                                  |
| `change_commander`                                   | Discrete   | 指挥链切换              | `name`,`command_chain`,`commander`                                                                                        |
| `arm_reflex_profile`                                 | Discrete   | 自卫反射弧策略武装/解除（v2.2） | `profile_id`,`armed`,`threat_classes[]`,`allowed_responses[]`,`decoy_quota{}`,`auto_maneuver_g_limit?`,`valid_duration_s` |
| `arksim_aux_command`                                 | Discrete   | 通用键值/仿真专用          | `platform_aux[]`,`kill_command?`                                                                                          |


`**arm_reflex_profile` 语义补充（v2.2，MUST）**：

- `threat_classes[]` ∈ `{LASER_DAZZLE, HPM, MISSILE_TERMINAL, GUN_TRACKING}`；`allowed_responses[]` ∈ `{APERTURE_SHUTTER, AUTO_DECOY, AUTO_BREAK_MANEUVER}`。
- FCS 受理后即获得在该策略范围内**毫秒级自主触发**对应反射动作的授权；超出 `decoy_quota` 或 `valid_duration_s` 后 MUST 自动解除武装并经 `FcsHealth.reflex` 上报。
- `AUTO_BREAK_MANEUVER` 触发的规避过载 MUST NOT 超过 `auto_maneuver_g_limit` 与平台 `max_g` 的较小值。
- 不支持的 profile MUST 以 `ACTUATION_REJECT_REASON_REFLEX_UNSUPPORTED` 拒绝，禁止静默忽略。

`**pre_delegation_id` 语义补充（v2.2，MUST）**：

- 仅在 TCS 链路丢失期间的火力类动作携带；FCS MUST 校验该令牌曾经由 TCS 经合法信道预置（存在性 + 有效期），校验失败以 `ACTUATION_REJECT_REASON_PREDELEGATION_INVALID` 拒绝。
- FCS 不解释令牌的目标类别/空间包络语义（该校验由 ELU `AGRA_SPGS_PREDELEGATE_ENGAGEMENT_V3` 完成）；FCS 校验是纵深防御第二闸门，不替代第一闸门。

**合并语义（MUST）**：

- Continuous 类：FCS MAY 做 **last-wins 合并**（同帧多条只保留最新）。
- Discrete 类（fire/retarget/cue/agent_control 等）：**MUST 不被合并/丢弃**，逐条执行。

### 4.3 `ActuationAck`（FCS -> ELU，本契约新增）

- `correlation_id` / `trace_id`：与触发动作对齐。
- `accepted`：是否整体受理。
- `rejected_commands[]`：被拒动作 `{index, kind, command_id, reject_code, detail_reason}`。
  - **标准拒绝码（`reject_code`，详见 proto 枚举）**：
    - `ACTUATION_REJECT_REASON_SAFETY_LIMIT`：触及物理/过载/包线极限（如 Max G 限制）。
    - `ACTUATION_REJECT_REASON_ROE_VIOLATION`：违反战术规则/ROE拦截（如非交战目标）。
    - `ACTUATION_REJECT_REASON_ACM_VIOLATION`：违反空域管控拦截（如闯入禁飞区）。
    - `ACTUATION_REJECT_REASON_EMCON_RESTRICTION`：违反辐射管制拦截（如 EMCON A 下禁止雷达开机）。
    - `ACTUATION_REJECT_REASON_OUT_OF_AMMO`：对应武器无弹药/挂架失效。
    - `ACTUATION_REJECT_REASON_HARDWARE_FAULT`：机载总线/硬件故障。
    - `ACTUATION_REJECT_REASON_LOA_INSUFFICIENT`：当前控制面权限不归属 ELU 所有。
    - `ACTUATION_REJECT_REASON_DUPLICATE`：幂等指令重复，拦截不予执行。
    - `ACTUATION_REJECT_REASON_PREDELEGATION_INVALID`（v2.2）：断链开火携带的 `pre_delegation_id` 不存在、已过期或未经 TCS 预置。
    - `ACTUATION_REJECT_REASON_REFLEX_UNSUPPORTED`（v2.2）：`arm_reflex_profile` 请求的威胁类别/响应类型超出平台反射弧硬件能力。
- `launch_request_id`：开火类动作的发射请求绑定（贯穿 fire→launch_confirm，复用 NGBM `AMFireAtAction.launch_request_id` 语义）。
- `effective_ts_ms`：FCS 实际生效时间戳。
- 约束：开火/改制导等 Discrete 动作 **MUST** 返回 ack；Continuous 动作可选。

### 4.4 `ControlAuthorityState`（ELU <-> FCS，HMT 分层控制权）

- **控制面权属隔离（MUST）**：摒弃单一体权，拆分为四大独立控制面，支持细粒度的人机协同（Teaming）：
  - `flight_plane`：飞行控制权（航向/高度/速度/航路），持权者：`ELU`/`PILOT`/`AUTOPILOT`/`UPLINK`。
  - `weapon_plane`：火力控制权（交战开火/改制导/箔条），持权者：`ELU`/`PILOT`/`UPLINK`。
  - `sensor_plane`：传感器控制权（主动雷达搜索/光电引导/IFF），持权者：`ELU`/`PILOT`。
  - `ew_plane`：有源电子干扰控制权（干扰频段/发射天线），持权者：`ELU`/`PILOT`。
- `since_ts_ms`：当前切换发生的起始时间戳。
- `reason`：切换原因说明（例：`PILOT_OVERRIDE`、`SAFE_HEARTBEAT_TIMEOUT`、`MISSION_COMPLETED`）。
- 约束：任何控制面的切换 **MUST** 经由 `agent_control` 动作发起，并由 FCS 发送 `ControlAuthorityState` 广播进行闭环确认。

### 4.5 `FcsHealth`（FCS -> ELU，新增授时与识别）

- `link.rtt_ms` / `link.frame_loss_pct` / `link.last_ack_age_ms` / `link.degraded`。
- `bus_ok`、`actuator_ok`、`sensor_ok`、`weapon_bus_ok`。
- **敌我识别健康（`iff_ok`）**：敌我识别询问应答机硬件工作状态（ABMS 安全红线）。
- **授时同步（`time_sync`，ABMS 协同底座）**：
  - `time_synchronized`：高精度时钟（PTP/GPS）是否锁定。
  - `skew_us`：微秒级时钟偏离差。ELU 可视偏差大小评估多平台 TOT（Time on Target）对齐风险。
- **导航退化真值（`nav`，v2.2 新增，PNT 拒止支撑）**：
  - `nav_mode`：`GPS_INS`（卫导/惯导融合正常）/ `INS_ONLY`（卫导失效，纯惯导）/ `DEGRADED`（惯导亦异常）。
  - `estimated_cep_m`：当前导航解的圆概率误差估计（米）。ELU 自适应 LOA 降级（远距开火/主动照射禁止）以本字段为唯一权威输入。
  - `drift_rate_m_s`：纯惯导漂移速率估计。ELU 据此预判精度恶化趋势与编队相对基准切换时机。
  - 约束：本块是导航质量的 SSOT；ELU MUST NOT 自行估计并持久化平行 CEP 值。
- **反射弧状态（`reflex`，v2.2 新增，定向能/近距威胁支撑）**：
  - `armed_profile_id`：当前生效的反射策略 ID（空值表示未武装）。
  - `last_event`：最近一次反射触发事件 `{type, triggered_ts_ms, response_taken, expended{chaff, flare}}`。
  - 约束：反射触发后 FCS MUST 在下一个 health 发布周期内回报 `last_event`；诱饵消耗同步反映到 `PlatformCapability`（库存 SSOT 不变），ELU 仅消费、不预扣。

### 4.6 `PlatformCapability`（FCS -> ELU，结构化战库与辐射管制）

- `asset_id`：平台 ID。
- **结构化弹药库清单（`weapon_stores[]`）**：
  - `type`：武器型号（如 AAM-1）。
  - `count` = 剩余可用实弹数量。
  - `ready` = 导弹和导引头是否就绪（如制冷完成、锁相成功）。
  - `stations[]` = 挂架物理通道编号。
  - `cost_class`（v2.2 新增）= 弹药成本档 `HIGH` / `MEDIUM` / `LOW`。蜂群饱和场景下 ELU 弹药经济性门控（交换比规则）的唯一权威来源；ELU MUST NOT 自建弹药价值表。
- `sensors[]`：可用物理传感器（如 radar-x, irst-1）。
- `max_g`、`speed_envelope{min,max}`、`alt_envelope{min,max}`、`supports_jamming`。
- **当前电磁管制等级（`current_emcon_level`）**：ABMS EMCON 等级（A, B, C, D）。
- **活动 IFF 模式（`active_iff_modes[]`）**：当前物理处于发射/应答状态的敌我识别模式（"1", "2", "3C", "4", "5", "S"）。
- **反射弧能力声明（`reflex_capability`，v2.2 新增）**：
  - `supported_profiles[]`：平台硬件支持的反射威胁类别与响应类型组合（如 `{threat: LASER_DAZZLE, responses: [APERTURE_SHUTTER]}`）。
  - `max_decoy_quota`：单个反射策略允许授权的诱饵消耗上限 `{CHAFF: n, FLARE: n}`。
  - 约束：ELU 下发 `arm_reflex_profile` 前 MUST 据此裁剪策略空间，超出声明的武装请求视为契约违例。
- 约束：ELU **MUST** 根据 `weapon_stores` 的剩余弹量和 `current_emcon_level` 的辐射限令裁剪自身的动作解算空间，下发超包线或违反电磁管制的动作直接视为契约违例。

### 4.7 `PlatformState`（FCS -> ELU）

（沿用 `ELU_TCS` 定义）`platform_id`、`latitude/longitude/altitude_msl`、`heading_deg`、`velocity_n/e/down`、`ground_speed_ms`、`vertical_speed_ms`、`fuel_level_percent`。

### 4.8 `WeaponState`（FCS -> ELU）

（沿用 `ELU_TCS` 定义）`weapon_id`、`host_id`、`current_target`、`mode`/`effect`、`latitude/longitude/altitude_msl`、`velocity_`*。

## 5. 典型消息流（最小闭环）

### 5.1 observe → decide → actuate → ack

1. FCS 周期发布 `fcs.truth.platform.*` / `fcs.truth.weapon.*`（物理真值）。
2. ELU 更新世界模型，结合 TCS 任务（`ELU_TCS`）解算动作。
3. ELU 发布 `fcs.actions.unit.{unit_id}`（`ActionEnvelopeV2`）。
4. FCS 执行并回 `fcs.ack.unit.{unit_id}`（`ActuationAck`）。
5. 效果在下一帧 truth 中体现，闭环。

### 5.2 控制权交接（握手）

1. ELU 根据任务需要发布 `agent_control` 动作，指定对特定控制面（如飞行面、火力面）进行申请接管：`args.action ∈ { "set_agent_outside_control", "release_outside_control" }`。
2. FCS 校验自主级别与当前 ROE 许可，确认受理并下发对应的分面 Ack。
3. FCS 广播发布最新的 `ControlAuthorityState` 消息，反映出各控制面的实际所属状态（如：`flight_plane=ELU`, `weapon_plane=PILOT`）。
4. 任务结束或异常情况（如心跳超时、飞行员强行 override）：ELU 发布 `release_outside_control` 主动释放控制权，或 FCS 触发 fail-safe 强制收回，各控制面安全交回 PILOT / AUTOPILOT，并广播最新的控制权状态。

### 5.3 链路/作动失效 fail-safe

1. `FcsHealth.degraded=true` 或 `last_ack_age_ms` 超阈值。
2. ELU 进入 LOA 回退（按最后有效 ROE/ACM 维持安全机动，**不擅自开火**）。
3. FCS 侧若 ELU 心跳丢失超阈值，MUST 收回控制权交回 autopilot（安全航线/盘旋）。
4. **预授权例外（v2.2）**：第 2 条的开火禁令存在唯一例外——开火动作携带 TCS 预先签发且仍在有效期内的 `pre_delegation_id`，并已通过 ELU 侧四重包络校验。FCS 对该令牌执行存在性/有效期二次校验，校验失败仍以 `PREDELEGATION_INVALID` 拒绝。链路恢复后所有令牌即刻失效。

### 5.4 反射弧武装与触发（v2.2）

1. ELU 评估威胁态势，下发 `arm_reflex_profile`（Discrete，含 `command_id`），FCS 校验 `reflex_capability` 后受理并回 Ack。
2. 威胁传感器（激光告警/导弹逼近告警）触发时，FCS 在已武装策略范围内**毫秒级自主执行**反射动作（快门/诱饵/规避），不上行请求、不等待 ELU。
3. FCS 在下一个 health 周期经 `FcsHealth.reflex.last_event` 回报触发详情；诱饵消耗同步反映到 `PlatformCapability.weapon_stores` / 诱饵库存。
4. ELU 下一 tick 消费回报，更新战术上下文（如规避后航线重规划）；策略超期或配额耗尽时 FCS 自动解除武装并上报。

## 6. 生产级最佳实践规范（必须遵循）

### 6.1 幂等与去重（MUST）

- Discrete 动作 MUST 携带 `command_id`（建议 `{correlation_id}:{unit_id}:{kind}:{shot_id?}`），FCS MUST 以此幂等去重，防止重发导致重复开火。
- `ActuationAck` MUST 回带对应 `command_id`，供 ELU 关联。

### 6.2 合并与限流（MUST）

- Continuous 动作 MAY last-wins 合并；ELU 的发布限流 **MUST NOT** 丢弃任何 Discrete 动作。

### 6.3 控制权安全（MUST）

- 同一 agent 同一时刻控制权唯一；切换必须握手确认；FCS 在冲突/超时下以安全侧（交回 autopilot）为默认。

### 6.4 能力门控（MUST）

- ELU 下发动作前 MUST 校验 `PlatformCapability`；不支持的动作不得下发。

### 6.5 fail-safe（MUST）

- 链路降级/失联：ELU 降级安全行为、不擅自升级打击；FCS 超时收回控制权。
- 武器类动作在 `weapon_status=HOLD`（来自 TCS 交战授权）下 MUST 被 FCS 二次拒绝（纵深防误击）。

### 6.6 时序与单位（MUST）

- 所有消息带毫秒时间戳；实装 MUST 有统一授时（PTP/GPS）。
- 单位约定：角度=度，距离=米，速度=m/s，经纬度=度（WGS84）；偏离 MUST 在 `schema_version` 注明。

### 6.7 消息认证与零信任（v2.2）

- 实装环境下，所有跨物理链路传输的 wire 消息 SHOULD 在 header 携带 `auth_tag`（HMAC 或等效消息认证码）；涉及火力/控制权/反射弧武装的消息 MUST 携带。
- 验签在**适配器层**完成：验签失败的消息 MUST 直接丢弃并产生审计事件，**不得**进入 ELU 决策核或 FCS 执行链。
- 密钥分发与轮换由平台安全基础设施承担，不属于本契约范围；本契约仅约定 `auth_tag` 字段位置与"验签失败即丢弃"的处置语义。
- 机内总线（同平台 ELU↔FCS）若物理隔离可信，`auth_tag` MAY 省略，但跨平台中继转发的消息不受此豁免。

### 6.8 反射弧治理（v2.2，MUST）

- 反射弧执行不产生新的决策真相源：触发判据（告警传感器信号）与执行结果（诱饵消耗、姿态扰动）全部经既有真值面/健康面/能力面回报，ELU 不得镜像维护"反射弧影子状态机"。
- FCS 反射动作范围被 `arm_reflex_profile` 严格限定：未武装类别 MUST NOT 触发；硬杀伤武器 MUST NOT 进入反射响应集合。
- 反射触发与 ELU 正常动作流冲突时（如反射规避机动 vs ELU 航路指令），FCS 以反射动作优先并在 Ack 中以 `SAFETY_LIMIT` 语义拒绝被抢占的常规指令，确保冲突可审计。

## 7. 附：示例 JSON（节选）

### 7.1 Action（连续控制 + 离散开火，含 command_id）

```json
{
  "schema_version": "2.1.0",
  "timestamp_ms": 1730000000500,
  "trace_id": "T-abc",
  "source": {"service": "elu", "unit_id": "S-02", "team_id": "0"},
  "target": {"agent_id": "S-02"},
  "x": {
    "mode": "AUTO",
    "commands": [
      {"kind": "go_to_location", "command_id": "C1:S-02:go_to_location",
       "args": {"latitude": 30.01, "longitude": 120.02, "altitude_m": 8000, "speed_ms": 250}},
      {"kind": "fire_at_target", "command_id": "C1:S-02:fire_at_target:SHOT-001",
       "args": {"track_id": "g01:2"}}
    ]
  }
}
```

### 7.2 ActuationAck（受理 + 1 条拒绝，含标准拒绝码与 command_id）

```json
{
  "correlation_id": "C1",
  "trace_id": "T-abc",
  "unit_id": "S-02",
  "accepted": true,
  "rejected_commands": [
    {
      "index": 1,
      "kind": "fire_at_target",
      "command_id": "C1:S-02:fire_at_target:SHOT-001",
      "reject_code": "ACTUATION_REJECT_REASON_ROE_VIOLATION",
      "detail_reason": "WEAPON_STATUS_HOLD_BY_TCS_POLICY"
    }
  ],
  "launch_request_id": "SHOT-001",
  "effective_ts_ms": 1730000000540
}
```

### 7.3 ControlAuthorityState（分层控制权状态）

```json
{
  "header": {
    "message_id": "auth-992",
    "timestamp_ms": 1730000000550,
    "source": "fcs",
    "schema_version": "2.1.0",
    "correlation_id": "C1",
    "trace_id": "T-abc"
  },
  "unit_id": "S-02",
  "flight_plane": "ELU",
  "weapon_plane": "PILOT",
  "sensor_plane": "ELU",
  "ew_plane": "ELU",
  "since_ts_ms": 1730000000500,
  "reason": "PILOT_OVERRIDE_WEAPON_PLANE"
}
```

### 7.4 PlatformState（物理真值）

```json
{
  "header": {"ts_ms": 1730000000400, "producer_role": "fcs"},
  "platform_id": "S-02",
  "latitude": 30.0, "longitude": 120.0, "altitude_msl": 8000,
  "heading_deg": 95.0, "velocity_n": 240, "velocity_e": 20, "velocity_down": 0,
  "ground_speed_ms": 241, "fuel_level_percent": 62.5
}
```

### 7.5 FcsHealth（含授时与 IFF）

```json
{
  "header": {
    "message_id": "h-001",
    "timestamp_ms": 1730000000400,
    "source": "fcs",
    "schema_version": "2.1.0"
  },
  "unit_id": "S-02",
  "link": {
    "rtt_ms": 180,
    "frame_loss_pct": 7.5,
    "last_ack_age_ms": 1200,
    "degraded": true
  },
  "bus_ok": true,
  "actuator_ok": true,
  "sensor_ok": true,
  "weapon_bus_ok": true,
  "iff_ok": true,
  "time_sync": {
    "time_synchronized": true,
    "skew_us": 12.5
  },
  "nav": {
    "nav_mode": "INS_ONLY",
    "estimated_cep_m": 85.0,
    "drift_rate_m_s": 0.4
  },
  "reflex": {
    "armed_profile_id": "RP-DE-01",
    "last_event": {
      "type": "LASER_DAZZLE",
      "triggered_ts_ms": 1730000000350,
      "response_taken": "APERTURE_SHUTTER",
      "expended": {"chaff": 0, "flare": 0}
    }
  }
}
```

### 7.6 PlatformCapability（结构化弹载与 EMCON）

```json
{
  "header": {
    "message_id": "cap-001",
    "timestamp_ms": 1730000000000,
    "source": "fcs",
    "schema_version": "2.1.0"
  },
  "asset_id": "S-02",
  "weapon_stores": [
    {"type": "AAM-1", "count": 4, "ready": true, "stations": ["ST-1", "S-2"], "cost_class": "HIGH"},
    {"type": "AAM-2", "count": 2, "ready": false, "stations": ["ST-3", "S-4"], "cost_class": "MEDIUM"}
  ],
  "sensors": ["radar-x", "irst-1"],
  "max_g": 9.0,
  "speed_envelope": {"min": 120, "max": 600},
  "alt_envelope": {"min": 100, "max": 18000},
  "supports_jamming": true,
  "current_emcon_level": "B",
  "active_iff_modes": ["3C", "5"],
  "reflex_capability": {
    "supported_profiles": [
      {"threat": "LASER_DAZZLE", "responses": ["APERTURE_SHUTTER"]},
      {"threat": "MISSILE_TERMINAL", "responses": ["AUTO_DECOY", "AUTO_BREAK_MANEUVER"]}
    ],
    "max_decoy_quota": {"CHAFF": 8, "FLARE": 8}
  }
}
```

---

## 8. 与 NGBM 内部 SSOT 的映射（落地对齐）


| NGBM 内部（Python ArkBM）                                      | 本契约                                             |
| ---------------------------------------------------------- | ----------------------------------------------- |
| `ActionBook` / `AMxxxAction`（`data_structures/actions.py`） | `ActionEnvelopeV2.x.commands[]`                 |
| `AMFireAtAction.launch_request_id`                         | `command_id` / `ActuationAck.launch_request_id` |
| `afsim_msg_translators.py`（当前唯一适配器）                        | **FcsHardwareAdapter**（实现态势入/动作出/能力声明）          |
| AFSIM `PlatformState`/`TrackState`/`WeaponState`           | `PlatformState` / `WeaponState`（物理真值面）          |
| `AgentContrl`（接管/释放）                                       | `agent_control` + `ControlAuthorityState`       |


> 实装落地：决策核仅依赖本契约抽象；AFSIM 适配器与 FCS 适配器实现同一组端口，二者可热替换（验收：同一决策核注入两适配器，`ActionEnvelopeV2` 输出字节级一致）。

---

**治理要求**：

- 本目录三份产物（文档/proto/schema）MUST 同步更新并保持一致。
- 每次新增 action kind / args 字段 / 开放字段 key，MUST 同步更新：文档（条款+示例）、proto（字段/枚举）、JSON Schema（required/约束/examples）。
- 任何跨 ELU↔FCS 边界的新字段，MUST 先入本契约，再在适配器实现；禁止在适配器内私造影子字段。

