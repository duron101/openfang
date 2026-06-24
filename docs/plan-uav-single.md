# OpenFang 单机无人机自主大脑 — 实施方案（轨道 1）

> 版本: v1.0 | 日期: 2026-06-08 | 状态: Plan
> 对应 PRD: [`prd-uav-single.md`](prd-uav-single.md)
> 依赖: Phase 0「活线接入」（已落地）

---

## 0. 现状基线（GitNexus 校正）

安全 / 仲裁内核已成熟（composer / gate / weapon_engagement / cerebellum /
command_lane / TacticalPipeline），且 Phase 0 已将其接成实时控制环：

- 配置层：`crates/openfang-types/src/config.rs` 的 `PlatformConfig / AdapterConfig / PlatformMode`。
- 加载：`crates/openfang-kernel/src/platform_boot.rs::build_registry()`，已接入 `kernel.rs` 启动。
- 工具执行器：`crates/openfang-runtime/src/platform_tools.rs::map_tool_to_command()` +
  `kernel.rs` 的 `KernelHandle::dispatch_platform_command`（武器类拒绝直发）。
- 控制环：`crates/openfang-kernel/src/platform_control.rs::PlatformControlLoop`
  （poll → DCC → cerebellum → TacticalPipeline）。
- DCC：`direct_channel.rs` 已含 `auto_rtb_on_low_fuel / auto_abort_on_comm_loss / auto_chaff_on_radar_lock`。

## 1. 工作分解

### 1A 空域模型
- `platform.rs` 新增 `AirDomainConstraints`（min/max 高度、最大爬升率、空域 geofence id）。
- `nav_control.rs` CPA/TCPA 扩展为三维（含垂直相对速度）。
- `op_restrictions.rs` 增加空域 geofence 校验（高度上下限）。

### 1B DDS 诚实化
- adapter 已实装；仅按 feature 真实声明 `capabilities()`（LSUAV 武器 / 干扰 = false）。

### 1C MAVLink 适配器
- 新建 `crates/openfang-platform-mavlink`，实现 `PlatformAdapter`（双向编解码占位 + SITL 路径）。
- 武器 / 干扰能力位 = false（典型飞控不直接管武器）。
- 在 `platform_boot.rs` 的 `build_adapter` 增加 `"mavlink"` 分支，kernel 增加 `mavlink` 特性。

### 1D 单机 Agent Profile
- 在 `bundled_agents.rs` 注册 `agents/uav-cca/`、`agents/uav-lsuav/`（不加载 FMA）。

### 1E 单机 DCC 规则
- 复用既有三条规则；补充 LSUAV 变体（如中继丢链处理）。

### 1F CCA 角色状态机（ABMS）
- `platform.rs` 新增 `CcaRole` 枚举（recon … adaptive + ew_protection / ew_jamming）。
- `mission_config.rs` 实现角色驱动行为：切换 EMCON、传感器模式、武器安保、NA 规划倾向。
- `AssignMission` → 解析角色 → 设置当前角色状态 → 影响后续 tick。

### 1G E2E
- Mock 闭环 → DDS 回环 → MAVLink SITL Patrol。
- 门禁：DCC RTB < 100ms；角色切换次 tick 生效。

## 2. 质量门禁
```bash
cargo build --workspace --lib
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## 3. 顺序
1A → 1F（角色，价值最高）→ 1C（MAVLink）→ 1B/1D/1E → 1G。
