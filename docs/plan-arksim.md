# OpenFang ArkSIM 仿真集成 — 实施方案（轨道 3，蓝军 Deferred）

> 版本: v1.0 | 日期: 2026-06-08 | 状态: Plan
> 依赖: Phase 0「活线接入」（已落地，含 `AdapterRegistry` 多 adapter 路由）

---

## 0. 现状基线
- ArkSIM adapter 已实装：`bridge.rs / sim_control.rs / command_mapper.rs /
  state_mapper.rs / proto_manual.rs`（手写 protobuf，无 prost-build 依赖）。
- Phase 0 已重写 `AdapterRegistry::route_commands()` / `poll_all()` 为**多 adapter
  路由 + 快照合并**，修复了原 secondary 死代码（P3C 已完成）。
- `platform_boot.rs` 已支持 `"arksim"` 类型按 config 构造 `ArkSimAdapter`。

## 1. 工作分解

### 3A Proto 对齐
- 依 ADR-029 选型对齐 ArkSIM 4.x 线格式，校准 `proto_manual.rs` 字段偏移。
- 用 `contract_equivalence.rs` 做编解码等价回归。

### 3B Mapper 补齐
- `command_mapper.rs`：未映射指令进入 `rejected`，而非无脑 `all_accepted`（诚实化）。
- `state_mapper.rs`：补齐 tracks / weapons / jammers / munitions 映射。
- `capabilities()` 按真实支持声明（UAV 收放 / 编队当前 = false）。

### 3C 多 adapter 路由（已完成于 Phase 0）
- `registry.route_commands()` 按 `platform_routing` 分组派发到 secondary；
  `poll_all()` 合并多 adapter 快照。已有 `registry::routing_tests` 覆盖。

### 3D 蓝军想定（Deferred）
- 红 / 蓝对抗想定、ArkSIM 场景脚本、对抗评估——本期不实现，待主线稳定后排期。

## 2. 质量门禁
```bash
cargo build --workspace --lib
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## 3. 顺序
3A → 3B（诚实化，安全相关优先）→ （3C 已完成）→ 3D Deferred。
