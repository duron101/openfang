# ArkSIM Rust 开发指南

> 基于 `usv_loiter_strike` 想定 **FireAtTarget 联调实测** 整理。  
> 目标读者：在 OpenFang 中扩展 `openfang-platform-arksim`、接入武器打击闭环的 Rust 工程师。  
> 配套参考：`protobuf/arksim_client_rust_port_reference.md`（60004 JSON 协议细节）、Python 联调脚本 `protobuf/warlock_command_walkthrough.py`。

---

## 1. 先读结论

| 通道 | 地址 | 线协议 | 适用场景 | OpenFang 现状 |
|------|------|--------|----------|---------------|
| **ArkService 控制面** | `tcp://127.0.0.1:60004` | ZMQ DEALER + JSON | 生产集成、Warlock GUI 观察 + 后台发令 | ✅ `ArkServiceClient` / `ArkSimAdapter` |
| **仿真直连（ZMQ）** | `tcp://127.0.0.1:18000` | **ZMQ PAIR + 裸 protobuf** | mission 直跑、逐步调试、单步收 StateMessage | ⚠️ 需新增客户端（见 §5） |
| **仿真直连（Framed TCP）** | `18000` | 10B 握手 + LE u32 帧 | 少数旧部署 / `bridge.rs` 假设 | ❌ 与 WSF_ZMQ_PROCESSOR **不兼容** |

**已验证打击闭环（2026-06）**：

```
外部控制 (E_SetAgentOutsideControl)
  → FireAtTarget(agent=self, weapon=loiter_wave2, track=self:1)
  → 仿真推进，平台数增加，Warlock/mission 可见巡飞弹释放
```

**三个硬性规则**（踩坑即失败）：

1. **trackId 必须是 `平台名:编号`**（冒号），evt 里的 `self.1` 只是日志展示，FireAtTarget 会报 `TrackID is error:self.1`。
2. **18000 上是 ZMQ，不是裸 TCP**。`ff00000000000000017f` 是 ZMQ 协议头，读完后**不会**自动推 StateMessage；必须 **send 指令 → recv 回包**。
3. **Warlock GUI 常不监听 18000**；同一想定经 **60004 proto** 仍可发令并在界面观察。

---

## 2. 想定与标识符（usv_loiter_strike）

| 角色 | 字段 | 值 |
|------|------|-----|
| 想定入口 | `scenario_path` | `E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\usv_loiter_strike.txt` |
| 红方平台 | `agent_id` / `platform_id` | `self` |
| 第二组巡飞弹 | `weapon_id` / `Component_id` | `loiter_wave2` |
| 蓝方巡逻艇 #1 | `track_id` | **`self:1`**（不是 `self.1`） |
| 其它航迹 | | `self:2`→blue_patrol_2，`self:3`→blue_patrol_3，`self:4`→指挥所，`self:5`→SAM |

航迹来源：`StateMessage.platforms[].tracks[].trackId`，proto 注释为 `<name>:<number>`（见 `protobuf/arksimproto.proto`）。

---

## 3. 端到端打击流程

### 3.1 推荐路径：ArkService 60004（与 `ArkSimAdapter` 一致）

```text
ArkSimAdapter::connect()
  └─ ArkServiceClient::connect()
       ├─ ResponseHandler (ZMQ DEALER → 60004)
       ├─ JSON start → 拿到 session uuid
       ├─ changesituation + customizedsituation + resume
       └─ 后台线程 recv_snapshot → latest_snapshot 缓存

ArkSimAdapter::send_commands(&[PlatformCommand::...])
  └─ command_mapper::to_proto_bytes()
  └─ ArkServiceClient::send_actions()
       └─ JSON {"fn":"proto","proto":"<base64>","uuid":"<session>"}
```

**Rust 侧最小命令序列**：

```rust
use openfang_types::platform::PlatformCommand;

let cmds = vec![
    PlatformCommand::SetOutsideControl {
        platform_id: "self".into(),
    },
    PlatformCommand::FireAtTarget {
        platform_id: "self".into(),
        weapon_id: "loiter_wave2".into(),
        track_id: "self:1".into(),  // 必须是冒号格式
    },
];

adapter.send_commands(&cmds).await?;
// 可选：runstep 由 kernel/platform_boot 或单独 sim_control 触发
```

对应 crate 入口：

- 适配器：`crates/openfang-platform-arksim/src/lib.rs` → `ArkSimAdapter`
- 命令编码：`command_mapper.rs` + `proto_manual.rs`
- 60004 会话：`arkservice.rs` + `response_handler.rs`

### 3.2 调试路径：ZMQ PAIR 18000（mission 直跑）

与 Python `ZmqStepClient` 相同语义：

```text
connect tcp://127.0.0.1:18000 (ZMQ PAIR)
loop:
  send ActionsFromOutside (裸 bytes)
  recv StateMessage (裸 bytes)
```

**不要**在 18000 上：

- 把 ZMQ 头当成自定义 handshake 后再读 LE 长度帧（会 `timed out`）
- 使用 `bridge.rs` 的 `FramedTcpStepClient` 路径（除非确认对端是 LE 帧而非 WSF_ZMQ）

Python 参考：`protobuf/warlock_command_walkthrough.py` 中 `ZmqStepClient`。

---

## 4. Protobuf 与编码

### 4.1 消息类型

| 方向 | Proto | Rust 实现 |
|------|-------|-----------|
| 控制 → 仿真 | `arksimActions.proto` → `ActionsFromOutside` | `proto_manual.rs` 手写编码（无 prost-build） |
| 仿真 → 控制 | `arksimproto.proto` → `StateMessage` | `proto_manual::parse_state_message` |

FireAtTarget 字段（`E_FireAtTarget = 12`）：

```text
FireAtTarget {
  action     = 12
  agent      = AgentName { agent_id, Component_id }
  trck_id    = "self:1"
}
```

编码函数：`proto_manual::encode_fire_at_target(action, agent_id, weapon_id, track_id)`  
聚合入口：`command_mapper::to_proto_bytes(&[PlatformCommand])`

### 4.2 trackId 规范化（必须在 Rust 复刻）

Python 已在 `proto_utils.normalize_track_id()` 实现：`self.1` → `self:1`。

**Rust 建议在 `command_mapper` 或独立 `track_id.rs` 中实现**：

```rust
/// FireAtTarget 要求 `<platform>:<number>`；evt 日志常写成 `<platform>.<number>`。
pub fn normalize_track_id(track_id: &str) -> String {
    let tid = track_id.trim();
    if tid.contains(':') {
        return tid.to_string();
    }
    if let Some((name, num)) = tid.rsplit_once('.') {
        if !name.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
            return format!("{name}:{num}");
        }
    }
    tid.to_string()
}
```

在 `FireAtTarget` / `FireSalvo` / `JamStart` / `UpdateTarget` 映射处调用，避免上游传入 evt 风格 ID。

### 4.3 ArkService proto 封装

60004 不直接收裸 protobuf，而是 JSON：

```json
{
  "fn": "proto",
  "proto": "<base64(ActionsFromOutside)>",
  "uuid": "<session_uuid>"
}
```

Rust 实现见 `arkservice.rs::send_actions` → `proto_bytes_to_json_string`。

---

## 5. 待补齐：ZMQ PAIR 18000 客户端

当前 `bridge.rs` 描述的是 **LE 长度前缀 + 10 字节握手**，与实测 WSF_ZMQ_PROCESSOR **不符**。建议新增模块（命名示例）：

```text
crates/openfang-platform-arksim/src/
  zmq_sim_bridge.rs    # 新增
  bridge.rs            # 保留或标注 legacy framed TCP
```

**接口 sketch**：

```rust
pub struct ZmqSimBridge {
    socket: zmq::Socket,
}

impl ZmqSimBridge {
    pub fn connect(host: &str, port: u16) -> Result<Self, String> {
        let ctx = zmq::Context::new();
        let socket = ctx.socket(zmq::PAIR).map_err(|e| e.to_string())?;
        socket.set_rcvtimeo(30_000).ok();
        socket.connect(&format!("tcp://{host}:{port}")).map_err(|e| e.to_string())?;
        Ok(Self { socket })
    }

    /// 发 ActionsFromOutside，收 StateMessage（推进一步）
    pub fn step(&self, actions: &[u8]) -> Result<Vec<u8>, String> {
        self.socket.send(actions, 0).map_err(|e| e.to_string())?;
        self.socket.recv(0).map_err(|e| e.to_string())
    }
}
```

用途：

- 集成测试探针（不依赖 60004）
- 与 Python walkthrough 对拍字节级行为
- 未来 `PlatformAdapter` 双传输：`Transport::ArkService | Transport::ZmqDirect`

依赖：crate 已含 `zmq = "0.10"`（`Cargo.toml`）。

---

## 6. OpenFang 配置示例

`~/.openfang/config.toml`（字段名以 kernel 实际为准）：

```toml
[platform.arksim]
host = "127.0.0.1"
service_port = 60004
# port = 18000  # 仅 legacy / 未来 ZmqDirect；当前适配器走 service_port
scenario_path = "E:\\dev\\ArkSIM_SCEN\\ArkSIMModels\\scenarios\\usv_loiter_strike\\usv_loiter_strike.txt"
situation_interval_secs = 3.0
```

连接后 `session_uuid` 由 `start` 响应写入，**不要**从配置静态填写。

---

## 7. 开发任务清单

### 7.1 新增 / 修改 PlatformCommand 映射

1. 在 `openfang-types` 增加或扩展 `PlatformCommand` 变体（若尚未存在）。
2. `command_mapper::is_supported` 加入匹配分支。
3. `command_mapper::to_proto_bytes` 调用 `proto_manual` 编码。
4. `proto_manual.rs` 补充 encoder（对照 `protobuf/arkcmd/proto/arksimActions.proto` 字段号）。
5. 单元测试：对比 Python `ProtoStringBuilder` 序列化 hex（可脚本导出 golden bytes）。

### 7.2 武器打击特性

| 步骤 | 说明 |
|------|------|
| 外部控制 | 先发 `SetOutsideControl`，否则部分指令被仿真忽略 |
| 选武器 | 从 `PlatformState.weapons` 或想定缺省取 `loiter_wave2` |
| 选航迹 | 从 `platform.tracks` 取蓝方 `trackId`，**normalize 冒号格式** |
| 推进仿真 | 60004：`runstep` / `advance_to_time`；18000：每次 `step()` 推进一步 |
| 验证 | evt `WEAPON_FIRED` 不一定出现；以 StateMessage 平台数/ActiveWeapon 为准 |

### 7.3 态势

- **不要**在 Rust 主路径阻塞等待 `customizedsituation` 推送（本环境常 0 帧）。
- 使用 `poll_state()` 读缓存；无缓存时读 `output/usv_loiter_strike.evt` 或 ZMQ step 回包解析。
- JSON 态势映射：`situation.rs`；protobuf 态势：`state_mapper.rs`。

---

## 8. 测试与联调

### 8.1 单元 / 契约测试

```bash
cargo test -p openfang-platform-arksim
cargo test -p openfang-platform-arksim --test contract_equivalence
```

### 8.2 与 Python 对拍（推荐）

| Python | 用途 |
|--------|------|
| `python protobuf/warlock_command_walkthrough.py --only E_SetAgentOutsideControl,E_FireAtTarget --no-prompt` | 18000 ZMQ 逐步打击 |
| `python protobuf/warlock_command_walkthrough.py --fallback-ark-service ...` | 60004 回退 |
| `python protobuf/test_fire_at_target.py` | 全自动 start → fire |

Rust 集成测试可：

1. 启动 mission 或依赖已运行实例；
2. `ZmqSimBridge::step` 发送与 Python 相同 bytes；
3. 断言 `parse_state_message` 平台数 / 时间递增。

### 8.3 环境检查

```powershell
# 60004 ArkService
netstat -ano | findstr "60004"

# 18000 mission ZMQ（需 mission 直跑且想定含 ZMQ_CONTROL）
netstat -ano | findstr "18000"
```

---

## 9. 常见错误

| 现象 | 原因 | 处理 |
|------|------|------|
| `WinError 10061` @ 18000 | 无监听（Warlock GUI 或未 Play） | 用 `--fallback-ark-service` 或 mission 直跑 |
| handshake 后 `timed out` | 把 ZMQ 头当 TCP 帧，等首帧态势 | 改用 ZMQ PAIR；首帧在首次 `step` 后 |
| `TrackID is error:self.1` | trackId 用了点号 | 改为 `self:1` 或 `normalize_track_id` |
| proto 已发但无开火 | 未 SetOutsideControl / 航迹无效 / 武器不可用 | 查 evt 与 StateMessage.tracks |
| `poll_state` 报错 | 态势缓存为空 | 读 evt；或 ZMQ step 本地解析 |

---

## 10. 模块地图（openfang-platform-arksim）

```text
lib.rs                 PlatformAdapter 实现，send_commands / poll_state
arkservice.rs          60004 会话：start、send_actions、recv_snapshot
response_handler.rs    ZMQ DEALER I/O 线程
arksim_controller.rs   JSON 命令 builder（start/pause/proto/...）
command_mapper.rs      PlatformCommand → ActionsFromOutside bytes
proto_manual.rs        手写 protobuf 编解码（含 FireAtTarget）
situation.rs           JSON customizedsituation → WorldSnapshot
state_mapper.rs        protobuf StateMessage → WorldSnapshot
bridge.rs              Legacy LE framed TCP（非 WSF_ZMQ 默认路径）
sim_control.rs         仿真控制抽象（测试 / 实验）
```

上层调用链：

```text
openfang-kernel (platform_boot / tactical_pipeline)
  → openfang-platform-arksim::ArkSimAdapter
  → openfang-runtime (weapon_engagement / intent → PlatformCommand)
```

---

## 11. 最小可运行示例（独立 binary / test）

```rust
// 伪代码 — 60004 路径，需在 spawn_blocking 中调用
use openfang_platform_arksim::command_mapper;
use openfang_types::platform::PlatformCommand;

fn fire_loiter_wave2(client: &ArkServiceClient) -> Result<(), String> {
    let cmds = [
        PlatformCommand::SetOutsideControl { platform_id: "self".into() },
        PlatformCommand::FireAtTarget {
            platform_id: "self".into(),
            weapon_id: "loiter_wave2".into(),
            track_id: "self:1".into(),
        },
    ];
    let bytes = command_mapper::to_proto_bytes(&cmds);
    client.send_actions(&bytes)?;
    Ok(())
}
```

ZMQ 18000 路径见 §5 `ZmqSimBridge::step`。

---

## 12. 延伸阅读

| 文档 / 代码 | 内容 |
|-------------|------|
| `protobuf/arksim_client_rust_port_reference.md` | 60004 JSON 全命令、E_Actions 表 |
| `protobuf/ArkService接口文档.html` | ArkService 官方接口说明 |
| `protobuf/warlock_command_walkthrough.py` | 已验证 ZMQ + 60004 双模式 walkthrough |
| `protobuf/arkcmd/proto/proto_utils.py` | Python ProtoStringBuilder + normalize_track_id |
| `mid_ark/afsim_py/msg_handler.py` | ZMQ PAIR 原始参考实现 |

---

**文档版本**: 2026-06-10  
**验证想定**: `usv_loiter_strike` / ArkSIM 4.1 / WSF_ZMQ_PROCESSOR port 18000  
**验证状态**: Python 打击闭环 ✅；Rust `ArkSimAdapter` 60004 发令 ✅；Rust ZMQ 18000 客户端待实现（§5）
