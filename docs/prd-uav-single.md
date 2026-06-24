# OpenFang 单机无人机自主大脑 — 产品需求文档 (PRD)

> 版本: v1.0 | 日期: 2026-06-08 | 状态: Draft
> 基础框架: OpenFang v0.3.24 (Rust)
> 适用平台: 协同作战飞机 CCA（高速、携武）+ 低速长航时 LSUAV（侦察 / 中继）

---

## 1. 范围与定位

本 PRD 描述**单架无人机**的机载自主大脑，与「无人机母机 / 集群」逻辑解耦
（母机见 [`prd-uav-mothership.md`](prd-uav-mothership.md)）。核心区别：

| 维度 | 单机 (本文) | 母机 (集群) |
|------|------------|-------------|
| 决策对象 | 本机本体（self） | 多架子机 + 收放 |
| 关键回路 | 飞控 / 规避 / 任务角色 | 任务分配 / 重指派 |
| Agent 画像 | 不加载 FMA | 加载 FMA |
| 适配器 | MAVLink / DDS | DDS |

## 2. 目标平台画像

- **CCA**：高速、可携带武器与干扰吊舱；需电子战、武器安保、角色化战术行为。
- **LSUAV**：低速长航时；以侦察 / 通信中继为主，武器与干扰能力位 = false。

## 3. 功能需求

### 3.1 飞行控制（机载，硬实时）
- FR-1 航向 / 速度 / 高度 / 航点 / 航线（`platform_set_*`、`platform_goto_location`、`platform_follow_route`）。
- FR-2 三维空域约束（最低 / 最高高度、爬升率、空域 geofence）。
- FR-3 三维 CPA/TCPA 防撞（含垂直分量）。

### 3.2 直接命令通道（DCC，旁路 LLM）
- FR-4 低油量自动返航 `auto_rtb_on_low_fuel`。
- FR-5 通信中断自动返航 `auto_abort_on_comm_loss`。
- FR-6 被雷达锁定自动投放干扰 `auto_chaff_on_radar_lock`。
- 门禁：DCC RTB 反射延迟 < 100ms。

### 3.3 任务角色驱动（ABMS）
- FR-7 接收上级分配的角色，驱动自身 EMCON / 传感器 / 武器安保 / 导航规划。
- 角色槽位（互斥主角色）：`recon / designator / relay / striker / decoy / intercept / patrol / escort / surveil / leader / adaptive`。
- 电子战叠加角色（可与主角色并存）：`ew_protection / ew_jamming`。

### 3.4 武器与电子战
- FR-8 武器释放必须经武器交战流水线（配额签署 + ROE 互锁），禁止工具直发。
- FR-9 干扰启停 / 模式（`platform_jam_*`）。

## 4. 非功能需求
- NFR-1 安全不变量（Iron Laws）：生产者只产 `CandidateIntent`；唯一仲裁者是 CommandGate；武器不可旁路。
- NFR-2 精简：通过 `tactical-uav` / `mavlink` 特性裁剪二进制与攻击面。
- NFR-3 在环可测：Mock 闭环 → DDS 回环 → MAVLink SITL。

## 5. 角色行为矩阵（ABMS）

| 角色 | EMCON | 传感器 | 武器 | NA 规划倾向 |
|------|-------|--------|------|-------------|
| recon | 静默优先 | 被动为主 | safe | 渗透 / 抵近观察 |
| designator | 受控辐射 | 激光 / EOIR 照射 | safe | 保持照射几何 |
| relay | 受控辐射 | 中继链路 | safe | 占据中继高点 |
| striker | 任务期辐射 | 火控雷达 | armed(经流水线) | 攻击航路 |
| decoy | 主动辐射 | 高可见 | safe | 诱导吸引 |
| intercept | 火控辐射 | 跟踪 | armed(经流水线) | 拦截几何 |
| patrol | 周期辐射 | 搜索 | hold | 巡逻航线 |
| escort | 随护辐射 | 警戒 | tight | 编队随护 |
| surveil | 静默 | 长时凝视 | safe | 站位凝视 |
| leader | 协调辐射 | 综合 | tight | 编队引领 |
| adaptive | 动态 | 动态 | 动态 | 依态势自适应 |
| ew_protection | — | ESM | safe | 自卫干扰几何 |
| ew_jamming | 主动 | 干扰 | safe | 压制几何 |

## 6. 验收
- 角色切换在下一个慢回路 tick 生效并改变 EMCON / 武器安保。
- DCC 三条单机规则可触发且满足延迟门禁。
- 武器意图在任何路径都无法绕过 gate。
