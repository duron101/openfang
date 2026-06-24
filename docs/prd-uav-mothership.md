# OpenFang 无人机母机自主大脑 — 产品需求文档 (PRD)

> 版本: v1.0 | 日期: 2026-06-08 | 状态: Draft
> 基础框架: OpenFang v0.3.24 (Rust)
> 适用平台: 无人机母机 / 母舰（异构舰队的发射、回收、任务分配）

---

## 1. 范围与定位

本 PRD 描述**母机（集群指挥）**自主大脑，与单机机载逻辑解耦
（单机见 [`prd-uav-single.md`](prd-uav-single.md)）。母机关注**舰队级**编排，
不替代子机的机载飞控 / 规避。

## 2. 功能需求

### 2.1 舰队态势
- FR-1 维护 `FleetSnapshot`：各子机的发射就绪、油量、链路质量、当前任务。
- FR-2 在 `WorldSnapshot` 中以 `fleet: Option<FleetSnapshot>` 暴露给 Agent。

### 2.2 发射 / 回收
- FR-3 `platform_launch_uav` / `platform_recover_uav` / `platform_rtb_uav`。
- FR-4 甲板资源编排 `platform_deck_reconfigure`（reload / refuel / swap / maintenance）。

### 2.3 任务分配与重指派
- FR-5 `platform_assign_mission`（area_search / track_target / strike / bda / comm_relay）。
- FR-6 失联 / 油尽 / 战损 → 自动重指派（`FleetManager`）。
- FR-7 目标交接 `platform_handoff_target`；中继使能 `platform_relay_enable/disable`。

### 2.4 协同
- FR-8 编队 `platform_form_up / break_formation / formation_maneuver`。
- FR-9 时敏协同打击 `platform_coordinated_strike`、制导交接 `platform_weapon_guidance_handoff`（武器类经流水线）。

## 3. 非功能需求
- NFR-1 与单机共享 Phase 0 内核与 Iron Laws；母机仅增舰队领域模型与服务。
- NFR-2 通过 `tactical-mothership` 特性裁剪。
- NFR-3 母机工作流可在 Mock / DDS 回环下端到端验证。

## 4. Agent 画像
- 加载 FMA（Fleet Management Agent）+ TCA/NA/SMA/CA/HMA/ORA；驱动 FleetManager。

## 5. 验收
- 子机失联后，下一个评估周期内由 FleetManager 触发重指派意图。
- 发射 / 回收 / 中继 / 交接命令经 gate 后到达 adapter。
