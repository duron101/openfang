# OpenFang 无人机母机自主大脑 — 实施方案（轨道 2）

> 版本: v1.0 | 日期: 2026-06-08 | 状态: Plan
> 对应 PRD: [`prd-uav-mothership.md`](prd-uav-mothership.md)
> 依赖: Phase 0「活线接入」（已落地）+ 轨道 1 命令映射

---

## 0. 现状基线
- `PlatformCommand` 已含 `LaunchUav / RecoverUav / ReturnToBase / AssignMission /
  AbortMission / HandoffTarget / RelayEnable / RelayDisable / DeckReconfigure /
  CoordinatedStrike / WeaponGuidanceHandoff / FormUp / BreakFormation / FormationManeuver`。
- 工具映射器（Phase 0）已覆盖上述全部母机工具 → 命令。
- 缺：舰队领域模型（`FleetSnapshot` 等）、`FleetManager` 服务、FMA 接线、舰队工作流。

## 1. 工作分解

### 2A 舰队领域模型
- `platform.rs` 新增：
  - `UavState`（id、就绪、油量 pct、链路质量、当前任务、最后心跳）。
  - `UavMission`（类型 + 参数 + 状态）。
  - `FleetSnapshot`（`Vec<UavState>` + 聚合指标）。
- `WorldSnapshot` 增 `fleet: Option<FleetSnapshot>`（向后兼容，默认 None）。

### 2B 命令映射补齐
- adapter 侧（DDS `publisher.rs`）补 `LaunchUav / RecoverUav / AssignMission /
  HandoffTarget` 的真实编码（当前可能落在 passthrough）。

### 2C FleetManager 服务
- 新建 `crates/openfang-runtime/src/fleet_manager.rs`：
  - 输入 `FleetSnapshot`；
  - 检测失联（心跳超时）、油尽（pct < 阈值）、战损；
  - 产出重指派 `CandidateIntent`（不直发 adapter，经流水线）。

### 2D FMA 接线 + 工作流
- `bundled_agents.rs` 注册 `agents/fma/`（已存在 agent toml）。
- 5 条舰队工作流：FleetLaunch / FleetRecovery / ReconToStrike /
  AutoTaskReallocate / CommRelayHandoff（`agents/workflows/`）。

### 2E E2E
- Mock / DDS 回环：发射→任务→失联→重指派 闭环测试。

## 2. 质量门禁
```bash
cargo build --workspace --lib
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## 3. 顺序
2A → 2C（FleetManager，核心价值）→ 2B → 2D → 2E。
