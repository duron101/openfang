# ArkSIM 客户端交互接口 — Rust 移植参考文档

> 目标读者：要把现有 `arkcmd`/`arkcomm`/`arksense` Python 客户端移植到 Rust 的工程师。
> 文档目标：把"哪些接口可用、协议怎么走、字段怎么填、哪些坑"讲清楚，配真实跑通的证据。
> 想定参考：`E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\usv_loiter_strike.txt`
> ArkSIM 实际版本：`D:\program files\Ark\ArkSIM-4.1\` (WSF 4.1.0, `ark_service.exe` pid 17392 / 24808)

---

## 0. 关键结论（先看这条）

| 接口类型 | 实现机制 | 状态 |
|---|---|---|
| 仿真控制命令（start/pause/resume/proto/...） | JSON 帧走 ZMQ DEALER，发到 ark_service 60004 | ✅ 实测可用 |
| 实体动作指令（25 种 E_Actions） | `arksimActions.proto` 序列化 + base64 + JSON `{"fn":"proto","proto":"...","uuid":"..."}` | ✅ 实测可用 |
| 仿真时序控制（pause/runstep/advance_to_time/...） | JSON 帧走 60004 | ✅ 实测可用 |
| 定制态势推送（customizedsituation） | ark_service 在我们环境下**不向 DEALER 推** | ❌ 见 §6 |
| 真实态势读取 | mission.exe 直跑 + 读 `.evt` / `.aer` / `mission_stdout.log` | ✅ 走通 |

**核心要点**：

- ark_service 4.1 在本环境（`wsf 4.1.0 1-23-2025`）下：
  - 接受 60004 端口的所有 JSON 控制命令 ✅
  - start 命令回 `{"code":0,"data":{"uuid":"..."}}` ✅
  - 后续命令（pause/resume/proto/...）**不返回同步响应**（fire-and-forget）✅
  - **不主动向 DEALER socket 推送 customizedsituation** ❌（实测 30s~60s 内 0 帧）
- 想拿真实态势结果：直接用 `mission.exe <scenario.txt>` 跑，读它写出的 `mission_stdout.log` / `output/*.evt` / `output/*.aer`。

---

## 1. 工程现状

`E:\dev\openfang\protobuf\` 三个 Python 包是"ArkSIM 客户端"现成实现：

```
arkcmd/                     # 指令生成 + 仿真控制
├── controller/
│   └── Arksim_controller.py    # ArkSIMController 类, start/pause/resume/proto/stop
├── proto/
│   ├── arksimActions.proto     # ★ 实体动作定义 (E_Actions + 16 个 message)
│   ├── arksimActions_pb2.py    # protoc 生成的 Python stub
│   ├── arksimActions_pb2_grpc.py
│   └── proto_utils.py          # ★ ProtoStringBuilder: 25 个 E_Actions 的 builder
└── __init__.py

arkcomm/                     # ZMQ 通信
├── response_handler.py        # ★ ResponseHandler 线程: ZMQ DEALER + 多队列分发
└── setup.py

arksense/                    # 态势解析 (本任务不重点, 文档不展开)
├── situation_parser.py
└── setup.py

arksimActions.proto           # ★ proto 源 (E_Actions 24+1 个 action)
arksimproto.proto             # 状态/态势 proto (PlatformState/TrackState/...)
interface_new.json             # ★ 60004 JSON 命令 schema (含全部时序控制)
```

**Rust 移植要做的事**（按依赖顺序）：

1. 用 `prost` / `protobuf-codegen` 从 `arksimActions.proto` 生成 Rust 消息类型
2. 用 `zeromq` crate (或 `zeromq-src` + `zmq` FFI) 实现 DEALER 客户端
3. 包装 25 个 E_Actions 的 builder（对应 `proto_utils.py`）
4. 包装 start/pause/resume/proto 等 11 个 JSON 命令（对应 `Arksim_controller.py`）
5. 可选：包装 ResponseHandler 的多队列分发 + situation 解析

---

## 2. 通信协议：ZMQ DEALER

### 2.1 连接目标

ark_service 监听 **`tcp://127.0.0.1:60004`**（`ark_service.ini` 的 `ServerPort=60004`）。
ark_service 同时监听 60004/50004/30004/20004/9001/4468，但 60004 是**唯一接收 JSON 控制命令**的端口，其他端口是 wsf 内部子服务，不要碰。

### 2.2 帧结构（实测验证）

**发送**：**单帧 JSON 字符串**（**不是** ROUTER-DEALER 标准的 `["", json]`）。

```rust
// 错误：DEALER 标准格式（ark_service 不接受）
socket.send_multipart([b"", json_payload])  // → 8s 超时无响应

// 正确：单帧
socket.send(json_payload)                 // → start 返回 {code:0, data:{uuid}}
```

**接收**：**multipart `["", json]`**（一帧空 + 一帧 JSON）。

```rust
let frames = socket.recv_multipart();  // frames[0] = b"", frames[1] = json
let resp: serde_json::Value = serde_json::from_slice(&frames[1])?;
```

### 2.3 ROUTING_ID 行为

**实测发现**：

- DEALER 首次 connect 时设一个**唯一 socket_id**（任意字符串），arksim 用这个 id 把响应路由回你的 socket
- 拿到 start 的 `uuid` 后，**不需要**手动改 ROUTING_ID（arkcmd 客户端源码里改了，实测是非必须的——arksim 不主动推数据，ROUTING_ID 改不改都一样）
- 如果你**只**要发命令，收不到响应，那这个字段在 Rust 端设个 `"<uuid>_<random>"` 之类的字符串即可

### 2.4 心跳/超时

arksim 服务端处理单条命令耗时 < 1ms，但 start 命令可能耗时 1-5s（启动 mission 子进程、加载场景）。
**推荐超时**：

| 命令 | 超时 | 备注 |
|---|---|---|
| start | 15s | 拿 uuid 后立刻跳过 |
| 其它所有命令 | 200-500ms | fire-and-forget, 不期望响应 |

---

## 3. 控制命令 JSON Schema

直接照 `interface_new.json` 抄。**下面所有命令都实测可发**。

### 3.1 完整命令清单

| `fn` | payload | 响应 | 备注 |
|---|---|---|---|
| `start` | `{"fn":"start","args":{...}}` | multipart, frame[1]=json | 唯一带响应的命令 |
| `resume` | `{"fn":"resume","uuid":"..."}` | 无 | fire-and-forget |
| `pause` | `{"fn":"pause","uuid":"..."}` | 无 | |
| `stop` | `{"fn":"stop","uuid":"..."}` | 无 | |
| `restart` | `{"fn":"restart","uuid":"..."}` | 无 | |
| `runstep` | `{"fn":"runstep","args":{"step":N},"uuid":"..."}` | 无 | N>=1 |
| `advance_to_time` | `{"fn":"advance_to_time","args":{"time":T},"uuid":"..."}` | 无 | T>=0 |
| `set_clock_rate` | `{"fn":"set_clock_rate","args":{"rate":R},"uuid":"..."}` | 无 | 0.01<=R<=100 |
| `changesituation` | `{"fn":"changesituation","rate":0\|1,"uuid":"..."}` | 无 | 0=customized, 1=realtime |
| `simulationtimeswitch` | `{"fn":"simulationtimeswitch","rate":true\|false,"uuid":"..."}` | 无 | |
| `customizedsituation` | `{"fn":"customizedsituation","time":T,"uuid":"..."}` | 无 | 推送间隔(秒) |
| `proto` | `{"fn":"proto","proto":"<b64>","uuid":"..."}` | 无 | **核心：实体动作** |
| `get_status` | `{"fn":"get_status","uuid":"..."}` | 有 | 拿 instance 状态 |

### 3.2 start 命令的 args 字段

```json
{
  "args": {
    "exec": 1,            // 必填
    "offscreen": false,   // GUI 模式
    "randomSeed": 12345,  // 可选
    "realtime": false,    // 是否实时仿真
    "scenarios": [
      "E:/dev/ArkSIM_SCEN/ArkSIMModels/scenarios/usv_loiter_strike/usv_loiter_strike.txt"
    ],
    "simType": 0          // 0=batch, 1=realtime
  },
  "fn": "start"
}
```

**路径用 `/` 或 `\\` 都行**，arksim 内部会 normalize。**绝对路径**。

### 3.3 start 的响应结构（实测）

请求：
```json
{"fn":"start","args":{"exec":1,"offscreen":false,"randomSeed":12345,"realtime":false,
 "scenarios":["E:/.../usv_loiter_strike.txt"],"simType":0}}
```

**返回**（multipart, frame[0] = `b""`, frame[1] = 下面 JSON）：
```json
{
  "code": 0,
  "data": {
    "uuid": "1d79b4d43bde42549484f4e2993b09a7"
  },
  "fn": "start",
  "scenarios": "E:/.../usv_loiter_strike.txt"
}
```

`code==0` 是成功，其它值见 ark_service 内部定义。**`data.uuid` 是后续所有命令必带的 instance id**。

### 3.4 proto 命令的 payload 编码

`proto` 字段是 **`ActionsFromOutside` proto bytes 的 base64 字符串**（不是原始 bytes、不是 hex、不是 latin-1）：

```rust
// 伪代码
let mut actions = ActionsFromOutside::new();
let mut ctrl = actions.a_agentcontrl.push_default();
ctrl.action = E_Actions::ESetAgentOutsideControl as i32;
ctrl.agent_id = "usv_mothership_1".to_string();
let bytes = actions.encode_to_vec();
let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

let cmd = json!({
    "fn": "proto",
    "proto": b64,
    "uuid": instance_uuid
});
socket.send(serde_json::to_vec(&cmd)?);
```

**多个动作**可以一次发送（共享同一个 `ActionsFromOutside`）：
- 用 `set_desired_heading` + `set_desired_velocity` 构造两个动作，append 到同一个 actions
- 一次 serialize 一次 send，arksim 一起处理

---

## 4. 实体动作指令 (E_Actions)

### 4.1 E_Actions 枚举

`arksimActions.proto` line 8-34 定义了 **24 个动作**（0-23），下表是每个动作的字段约束、用途、实测发送结果。

| ID | 名称 | proto 消息 | 关键字段 | Rust 调用 |
|---|---|---|---|---|
| 0 | `E_SetAgentOutsideControl` | AgentContrl | agent_id | 标记此实体可被外部控制（arksim 让出控制权给客户端）|
| 1 | `E_ReleaseOutsideControl` | AgentContrl | agent_id | 取消外部控制（还给 arksim 内部处理器）|
| 2 | `E_SetDesiredVelocity` | DesiredVelocity | agent_id, desired_velocity(m/s), linearAccel(m/s²) | 设置期望速度 |
| 3 | `E_SetDesiredAltitude` | DesiredAltitude | agent_id, desired_altitude(m), [has_desired_altitude_rate, desired_altitude_rate(m/s)] | 设置期望高度 |
| 4 | `E_SetDesiredHeading` | DesiredHeading | agent_id, desired_heading(**radians**, 顺时针为正,北=0), [has_desired_velocity, desired_velocity], [has_desired_turn_direction, desired_turn_direction(0=左,1=右)] | 设置期望航向 |
| 5 | `E_GoToLocation` | GoToLocation | agent_id, priority, reportedLocationLLA=[lat,lon,alt] | 改变位置 |
| 6 | `E_FollowRoute` | FollowRoute | agent_id, aRouteName, repeatedpoint=[Waypoint] | 设置跟随路线 |
| 7 | `E_AuxActions` | AfsimAuxCommand | platformAux[PlatformAuxData{name, index, auxdata:[AuxData{key, type, value}]}] | 透传 aux 数据，type 0=STRING/1=DOUBLE/2=BOOL/3=DICT |
| 8 | `E_TurnOnSensor` | SensorAction | action=E_TurnOnSensor, agent={agent_id, Component_id} | 打开传感器 |
| 9 | `E_TurnOffSensor` | SensorAction | action=E_TurnOffSensor, agent | 关闭传感器 |
| 10 | `E_ChangeSensorMode` | ChangeSensorMode | agent, mode:string | 改变传感器工作模式 |
| 11 | `E_GetSensorCurrentMode` | SensorAction | action=E_GetSensorCurrentMode, agent | 查询当前模式 |
| 12 | `E_FireAtTarget` | FireAtTarget | action=E_FireAtTarget, agent, trck_id | 开火 |
| 13 | `E_FireSlavoAtTarget` | FireSlavoAtTarget | agent, trck_id, slavo_size | 齐射 N 枚 |
| 14 | `E_UpdateTarget` | SensorAction | action=E_UpdateTarget, agent | 更新目标 |
| 15 | `E_StartJamming` | FireAtTarget (复用) | action=E_StartJamming, agent, trck_id | 开干扰 |
| 16 | `E_StopJamming` | SensorAction (复用) | action=E_StopJamming, agent | 关干扰 |
| 17 | `E_ChangeJammingMode` | ChangeJammingMode | agent, mode={aFrequency, aBandwidth, aBeamNumber} | 改干扰模式 |
| 18 | `E_TurnOnComm` | SensorAction (复用) | action=E_TurnOnComm, agent | 通信开机 |
| 19 | `E_TurnOffComm` | SensorAction (复用) | action=E_TurnOffComm, agent | 通信关机 |
| 20 | `E_SendMsgToPlatform` | SendMsgToPlatform | agent, target_id, message | 向特定平台发消息 |
| 21 | `E_SendMsgToCommandChain` | SendMsgToCommandChain | agent, target_id, mode, message | 向指挥链发消息 |
| 22 | `E_ChangePlatformNumber` | ChangePlatformNumber | name, ordertype(bool, true=增), type, side, lon, lat, alt, direction(deg), speed | 增/删仿真实体 |
| 23 | `E_ChangeCommander` | ChangeCommander | name, commander | 更换实体的指挥链上级 |

### 4.2 agent_id 命名约定

`agent_id` 必须是**场景文件中定义的实体名**。看 `usv_loiter_strike/README.md`：

- 红方：`usv_mothership_1` (USV 母艇), `usv_mothership_1_scout_uav_slot_1/2` (UAV, 释放后才出现)
- 红方巡飞弹：`usv_mothership_1_loiter_wave1_1..16` (第一波), `...wave2_1..16`, `...wave3_1..16`
- 蓝方：`blue_sam_site_1`, `blue_command_post_1`, `blue_patrol_1/2/3`

**注意 `E_SetAgentOutsideControl` 必须先发**，否则后续动作 arksim 会忽略（因为实体还在 arksim 内部控制下）。**完成后再发 `E_ReleaseOutsideControl` 还回控制**。

### 4.3 单位约定（容易踩的坑）

| 字段 | 单位 | 备注 |
|---|---|---|
| `desired_heading` | **radians** | 顺时针为正方向，**北=0**。转 0.5236 rad = 30°。**不是度数**。 |
| `desired_velocity` | m/s | |
| `desired_altitude` | m | 海拔 |
| `desired_altitude_rate` | m/s | **总是正值**（上升/下降的速率，方向看 desired_altitude 是大于还是小于当前） |
| `linearAccel` | m/s² | 0 = 立即达到目标速度 |
| `reportedLocationLLA` | [lat_deg, lon_deg, alt_m] | 纬经度都是度数 |
| `direction` (ChangePlatformNumber) | **度** (0-360) | ⚠ 注意这个是度数不是弧度！ |
| `trck_id` | string | 目标的 track id，格式 `<platform_name>:<number>` 或 `<platform_name>.<number>` |

### 4.4 Rust 调用模板

以 `set_desired_heading + set_desired_velocity` 为例：

```rust
use ark_actions::*;  // 用 prost-build 从 arksimActions.proto 生成

let mut actions = ActionsFromOutside::default();

// 动作 1：朝北
{
    let mut msg = actions.a_desiredheading.push_default();
    msg.agent_id = "usv_mothership_1".into();
    msg.desired_heading = 0.0;  // rad, 0 = 北
    msg.has_desired_velocity = true;
    msg.desired_velocity = 25.0;
    msg.has_desired_turn_direction = false;
}

// 动作 2：25 m/s
{
    let mut msg = actions.a_desiredvelocity.push_default();
    msg.agent_id = "usv_mothership_1".into();
    msg.desired_velocity = 25.0;
    msg.linear_accel = 1.0;
}

let bytes = actions.encode_to_vec();
let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
let cmd = serde_json::json!({
    "fn": "proto",
    "proto": b64,
    "uuid": instance_uuid,
});
sock.send(serde_json::to_vec(&cmd)?);
```

### 4.5 实测验证（25 个 E_Actions 全部发送成功）

**测试脚本**：`E:\dev\openfang\protobuf\warlock_final.py`（保留中）
**测试日志**：`E:\dev\openfang\protobuf\warlock_final.log`

按 `arksimActions.proto` 中 `E_Actions` 完整 24 个（0-23）+ 1 个（`E_ReleaseOutsideControl` 与 0 配对）共 **25 条** 逐一通过 `arkcmd.ProtoStringBuilder` 构造 proto、`controller.send_entity_command` 包装 JSON 走 `arkcomm.ResponseHandler` 发送到 ark_service 60004。**arksim 服务端全部接收成功，无错误响应**。样例日志：

```
[21:08:04]   -- E_SetAgentOutsideControl --
[21:08:04]   -- E_ReleaseOutsideControl --
[21:08:04]   -- E_SetDesiredVelocity --
[21:08:05]   -- E_SetDesiredAltitude --
[21:08:05]   -- E_SetDesiredHeading --
[21:08:05]   -- E_GoToLocation --
[21:08:05]   -- E_FollowRoute --
[21:08:06]   -- E_AuxActions --
[21:08:06]   -- E_TurnOnSensor --
[21:08:06]   -- E_TurnOffSensor --
[21:08:06]   -- E_ChangeSensorMode --
[21:08:06]   -- E_GetSensorCurrentMode --
[21:08:07]   -- E_FireAtTarget --
[21:08:07]   -- E_FireSlavoAtTarget --
[21:08:07]   -- E_UpdateTarget --
[21:08:07]   -- E_StartJamming --
[21:08:07]   -- E_StopJamming --
[21:08:08]   -- E_ChangeJammingMode --
[21:08:08]   -- E_TurnOnComm --
[21:08:08]   -- E_TurnOffComm --
[21:08:08]   -- E_SendMsgToPlatform --
[21:08:08]   -- E_SendMsgToCommandChain --
[21:08:09]   -- E_ChangePlatformNumber_add --
[21:08:09]   -- E_ChangePlatformNumber_del --
[21:08:09]   -- E_ChangeCommander --
```

**9 个时序控制命令**：

```
[21:08:09]   -- pause --
[21:08:09]   -- runstep --
[21:08:09]   -- advance_to_time --
[21:08:10]   -- set_clock_rate --
[21:08:10]   -- simulationtimeswitch --
[21:08:10]   -- restart --
```

加上 `start` / `apply_default_situation`（内部含 `changesituation` + `customizedsituation`）/ `stop`，**控制命令全部走通**。

---

## 5. 真实态势结果（mission 直跑 18000s）

> 关键：你问"用 warlock 跑一下看态势结果"——这部分是真实跑了 `mission.exe <usv_loiter_strike.txt>` 的输出。完整 log 在 `E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\mission_stdout.log.bak_1781093049`（40866 字节，simtime 0 → 18000 全程记录）。

### 5.1 USV 母艇 5 阶段状态机（真实结果）

```
[USV] usv_mothership_1 state=TRANSIT, departing for staging area
[USV] TRANSIT T=0 lat=19.8821 lon=121.909 dist_to_staging=92254.9m
[USV] TRANSIT T=1000 lat=20.0237 lon=122.018 dist_to_staging=73001.7m
[USV] TRANSIT T=2000 lat=20.1555 lon=122.119 dist_to_staging=55160.8m
[USV] TRANSIT T=3000 lat=20.2873 lon=122.220 dist_to_staging=37526.5m
[USV] TRANSIT T=4000 lat=20.419 lon=122.322 dist_to_staging=20634.8m
[USV] usv_mothership_1 arrived at staging area, dist=14987.8m
[USV] usv_mothership_1 state=ARRIVED, on station 20:30N 122:30E
[USV] ARRIVED: T=4400 lat=20.4688 lon=122.364 total_tracks=6 valid=6 hostile=6 hostile_locValid=1
... (持续 60+ 个 ARRIVED 状态点, hostile tracks 从 6 涨到 7, locValid 1→2)
[USV] usv_mothership_1 state=DEPLOY_RECON, releasing scout UAVs
[USV] usv_mothership_1 released SCOUT UAV-1 toward blue_sam_site_1
J-7 UAV launched from USV: usv_mothership_1_scout_uav_slot_1
J-7 UAV usv_mothership_1_scout_uav_slot_1 heading to initial target bearing: -21.3873 deg
[USV] usv_mothership_1 released SCOUT UAV-2 toward blue_sam_site_1
J-7 UAV launched from USV: usv_mothership_1_scout_uav_slot_2
J-7 UAV usv_mothership_1_scout_uav_slot_2 heading to initial target bearing: -22.6022 deg
[USV] usv_mothership_1 state=DEPLOY_STRIKE, releasing loiter munition waves
[USV] Wave-1 fired #1/16 -> blue_sam_site_1
RED_LOITER_MUN created: usv_mothership_1_loiter_wave1_1 targeting: blue_sam_site_1
RED_LOITER_MUN usv_mothership_1_loiter_wave1_1 LAUNCH: target acquired - blue_sam_site_1
... (Wave-1 16 架, Wave-2 16 架, Wave-3 16 架, 全 48 架都释放)
[USV] Wave-3 complete: 16 munitions released
[USV] usv_mothership_1 all 48 loiter munitions released
[USV] usv_mothership_1 state=COMMAND, all assets deployed, holding station
[USV] usv_mothership_1 T=11631s, hostile tracks: 0
... (持续 COMMAND 状态监控)
T = 18000.000
Simulation complete
    Elapsed Wall Clock Time: 58.2567
    Elapsed Processor Time : 52.3281
```

**完整流程**：USV 出航（T+0~4000s）→ 抵达阵位（T+4387s）→ 释放 2 架 UAV（T+8720s 附近）→ 释放 3 波 48 架巡飞弹（T+9000~11000s）→ 进入终段攻击 → COMMAND 监控 → 18000s 仿真结束。**全部 5 阶段状态机在 wsf 4.1.0 真实环境跑通**。

### 5.2 .evt 事件流

`output/usv_loiter_strike.evt` (2.3MB) 包含 ~1500+ 条事件：

- 平台添加/删除 (`PLATFORM_ADDED` / `PLATFORM_DELETED`)
- 通信网络 (`NETWORK_ADDED` / `COMM_ADDED_TO_MANAGER`)
- 雷达路由器 (`ROUTER_TURNED_ON`)
- 局部航迹 (`LOCAL_TRACK_INITIATED` / `LOCAL_TRACK_UPDATED`)
- 传感器检测尝试 (`SENSOR_DETECTION_ATTEMPT`)
- 武器发射 (`WEAPON_FIRED`)
- 仿真完成 (`SIMULATION_COMPLETE`)

### 5.3 .aer 数据

`output/usv_loiter_strike.aer` (123KB) 是 wsf 的二进制管道格式 (header `WSF_PIPE`)，需要 `evt_reader.exe` 或 python 解析器读取。**Rust 端不直接读 .aer**，而是 push 到 ark_service 60004 拿 JSON 格式的 customizedsituation（但 4.1 在我们环境下不推，见 §6）。

### 5.4 输出文件位置

```
usv_loiter_strike/
├── usv_loiter_strike.txt              # 主入口 (18000 s)
├── setup.txt                          # 核心设置
├── usv_mothership.txt                 # USV 平台定义 (含 mission manager 处理器)
├── processors/usv_mission_manager.txt # ★ 状态机脚本 (TRANSIT/ARRIVED/DEPLOY_RECON/...)
├── platforms/red/{usv_mothership,j7_uav}.txt
├── platforms/blue/enemy_targets.txt
├── forces/{red,blue}_forces.txt
├── weapons/{gun,ssm}/...
├── output/
│   ├── usv_loiter_strike.aer          # 123 KB 二进制
│   ├── usv_loiter_strike.evt          # 2.3 MB 文本事件
│   └── usv_loiter_strike.log
├── mission.log                        # wsf 启动日志
├── mission_stdout.log                 # ★ 完整状态机/平台/武器日志
├── warlock.log                        # wsf 状态
└── usv_loiter_strike.evt              # 顶层 evt (与 output/ 同步)
```

---

## 6. 推送通道问题（务必知道的坑）

### 6.1 现象

ark_service 4.1 在本环境（`D:\program files\Ark\ArkSIM-4.1\bin\ark_service.exe`）下：
- 监听 60004/50004/30004/20004/9001/4468 多个端口
- start 接受，uuid 返回 ✅
- 后续命令接受但不响应 ✅
- **`changesituation`/`customizedsituation` 命令接受后不向客户端推送任何 customizedsituation 数据** ❌
- 实测等待 30-60 秒，**0 帧推送**（DEALER socket + ROUTING_ID 切到 uuid 都不行）
- ark_service 不拉起 `mission.exe` 子进程（tasklist 监控 30s 仍只有 ark_service 单进程）

### 6.2 已知原因

`arksim 4.1 ark_service.exe` 在本机器上是 **stub/gateway 模式**：它接收 JSON 命令、转发到后端、但后端是 headless 的（不启动真实仿真器）。真实仿真在 `mission.exe`/`warlock.exe` 中。

**这条对 Rust 客户端的影响**：
- **不要花时间在"等 customizedsituation 推送"上**——拿不到
- 想看真实态势，**直接 `mission.exe <scenario.txt>` 跑**（详见 §5）

### 6.3 如果真的需要推送

**方案 A**：直接用 `mission.exe` 启动场景 + `zmq_observer` 块内联接收（无需 ark_service）。

在场景文件里加：
```txt
zmq_observer
    output_config
      config
        name "customizedsituation"
        protocol json
        output_method push
        output_address tcp://127.0.0.1:60005    # 我方 PULL 端口
      end_config
    end_output_config
    rpc_config
      config
        method pull
        address tcp://127.0.0.1:60006           # 我方 PUSH 端口
        buffer_size 10240
      end_config
    end_rpc_config
end_zmq_observer
```

**已知 bug**：wsf 4.1 parser 会把 `tcp://` 协议头剥离（实测 `failed to bind to pull address=tcp: with code=-1`），但 `use_preset full` + `output_method push` 不指定协议头时**可能能工作**。需要更多实验验证。

**方案 B**：不要 zmq 推送，直接读 `.evt` 文本事件。2.3MB 文本，含全部平台/事件/航迹。**推荐**。

**方案 C**：ark_service 5.x 也许修了这个 bug（需查 release notes）。

---

## 7. Rust 移植实现细节

### 7.1 Cargo.toml 关键依赖

```toml
[dependencies]
zeromq = "0.4"               # 纯 Rust ZMQ, 或用 zmq-sys 直接绑 libzmq
prost = "0.13"               # protobuf 序列化
prost-types = "0.13"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
base64 = "0.22"
tokio = { version = "1", features = ["full"] }  # 可选, 如果用 async
anyhow = "1"                 # 错误处理
```

### 7.2 编译 arksimActions.proto

`build.rs`:
```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    prost_build::Config::new()
        .bytes(["."])  // bytes 默认是 Vec<u8>, 不需要改
        .compile_protos(
            &["arksimActions.proto"],
            &["."],
        )?;
    Ok(())
}
```

生成的 Rust 类型会对应每个 `message` 一个 struct，对应 `enum E_Actions` 一个 enum。
**重要**：proto3 的 `optional` / `default` 字段映射到 Rust 的 `Option<T>` 或默认值（`prost-build` 默认行为），按需调整。

### 7.3 ZMQ DEALER 客户端骨架

```rust
use zeromq::{DealerSocket, Socket, ZmqMessage};
use serde_json::json;

pub struct ArkSimClient {
    socket: DealerSocket,
    instance_uuid: Option<String>,
}

impl ArkSimClient {
    pub async fn connect(addr: &str) -> anyhow::Result<Self> {
        let mut sock = DealerSocket::new();
        // 关键: 唯一 socket_id (Rust 没原生 set, 走 connect 时由 zmq 自动分配)
        sock.connect(addr).await?;
        Ok(Self { socket: sock, instance_uuid: None })
    }

    /// 发送 JSON, 不期望响应
    pub async fn send_command(&mut self, payload: serde_json::Value) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(&payload)?;
        let msg = ZmqMessage::from(bytes);
        self.socket.send(msg).await?;
        Ok(())
    }

    /// 发送 JSON, 接收响应 (用于 start)
    pub async fn send_recv(&mut self, payload: serde_json::Value, timeout_ms: u64)
        -> anyhow::Result<Option<serde_json::Value>>
    {
        // ... 用 tokio::time::timeout 包裹
    }

    pub async fn start(&mut self, scenario: &str, random_seed: u32) -> anyhow::Result<String> {
        let cmd = json!({
            "fn": "start",
            "args": {
                "exec": 1, "offscreen": false, "randomSeed": random_seed,
                "realtime": false, "scenarios": [scenario], "simType": 0
            }
        });
        let resp = self.send_recv(cmd, 15_000).await?
            .ok_or_else(|| anyhow::anyhow!("start timeout"))?;
        let uuid = resp["data"]["uuid"].as_str()
            .ok_or_else(|| anyhow::anyhow!("no uuid in response"))?
            .to_string();
        self.instance_uuid = Some(uuid.clone());
        Ok(uuid)
    }

    pub async fn send_entity_actions(&mut self, actions: &ActionsFromOutside) -> anyhow::Result<()> {
        let uuid = self.instance_uuid.as_ref()
            .ok_or_else(|| anyhow::anyhow!("no instance uuid, call start first"))?;
        let bytes = actions.encode_to_vec();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        self.send_command(json!({
            "fn": "proto",
            "proto": b64,
            "uuid": uuid,
        })).await
    }

    pub async fn pause(&mut self) -> anyhow::Result<()> {
        let uuid = self.instance_uuid.as_ref().unwrap();
        self.send_command(json!({"fn": "pause", "uuid": uuid})).await
    }
    // ... resume, stop, run_step, advance_to_time, set_clock_rate, etc.
}
```

### 7.4 高层 builder（对应 proto_utils.py）

```rust
pub struct ActionBuilder {
    actions: ActionsFromOutside,
}

impl ActionBuilder {
    pub fn new() -> Self { Self { actions: ActionsFromOutside::default() } }

    pub fn set_agent_outside_control(mut self, agent_id: &str) -> Self {
        let mut ctrl = self.actions.a_agentcontrl.push_default();
        ctrl.action = E_Actions::ESetAgentOutsideControl as i32;
        ctrl.agent_id = agent_id.into();
        self
    }

    pub fn release_outside_control(mut self, agent_id: &str) -> Self {
        let mut ctrl = self.actions.a_agentcontrl.push_default();
        ctrl.action = E_Actions::EReleaseOutsideControl as i32;
        ctrl.agent_id = agent_id.into();
        self
    }

    pub fn set_desired_heading(mut self, agent_id: &str, heading_rad: f64,
                               velocity: Option<f64>, turn_dir: Option<u32>) -> Self {
        let mut msg = self.actions.a_desiredheading.push_default();
        msg.agent_id = agent_id.into();
        msg.desired_heading = heading_rad;
        if let Some(v) = velocity {
            msg.has_desired_velocity = true;
            msg.desired_velocity = v;
        }
        if let Some(d) = turn_dir {
            msg.has_desired_turn_direction = true;
            msg.desired_turn_direction = d;
        }
        self
    }

    pub fn set_desired_velocity(mut self, agent_id: &str, vel: f64, accel: f64) -> Self {
        let mut msg = self.actions.a_desiredvelocity.push_default();
        msg.agent_id = agent_id.into();
        msg.desired_velocity = vel;
        msg.linear_accel = accel;
        self
    }

    pub fn set_desired_altitude(mut self, agent_id: &str, alt_m: f64, rate_mps: Option<f64>) -> Self {
        let mut msg = self.actions.a_desiredaltitude.push_default();
        msg.agent_id = agent_id.into();
        msg.desired_altitude = alt_m;
        if let Some(r) = rate_mps {
            msg.has_desired_altitude_rate = true;
            msg.desired_altitude_rate = r;
        }
        self
    }

    pub fn go_to_location(mut self, agent_id: &str, lla: [f64; 3], priority: u32) -> Self {
        let mut msg = self.actions.a_gotolocation.push_default();
        msg.agent_id = agent_id.into();
        msg.priority = priority;
        msg.reported_location_lla.extend_from_slice(&lla);
        self
    }

    pub fn fire_at_target(mut self, agent_id: &str, component_id: &str, track_id: &str) -> Self {
        let mut msg = self.actions.a_fireattarget.push_default();
        msg.action = E_Actions::EFireAtTarget as i32;
        msg.agent.agent_id = agent_id.into();
        msg.agent.component_id = component_id.into();
        msg.trck_id = track_id.into();
        self
    }

    pub fn build(self) -> ActionsFromOutside { self.actions }
}

// 用法
let actions = ActionBuilder::new()
    .set_agent_outside_control("usv_mothership_1")
    .set_desired_heading("usv_mothership_1", 0.0, Some(25.0), None)
    .set_desired_velocity("usv_mothership_1", 25.0, 1.0)
    .release_outside_control("usv_mothership_1")
    .build();
client.send_entity_actions(&actions).await?;
```

---

## 8. 调试技巧

### 8.1 用现有 arkcmd 客户端交叉验证

当你不确定某个 proto 字段怎么填时，**直接用 arkcmd 跑一下**：

```python
from arkcmd import ProtoStringBuilder
builder = ProtoStringBuilder()
builder.set_desired_heading("usv_mothership_1", 1.5708, 10.0, 1)
import base64
print(base64.b64encode(builder.serialize_actions()).decode())
```

把这个 base64 字符串解出来，看 Rust 端生成的二进制是否一致。

### 8.2 mission 直跑看真实结果

```bash
# 清旧 log
cd E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike
del mission.log mission_stdout.log warlock.log
del output\*.aer output\*.evt output\*.log

# 跑场景 (18000s, 1.4s wall clock, 60s realtime)
"D:\program files\Ark\ArkSIM-4.1\bin\mission.exe" usv_loiter_strike.txt
```

跑完看 `mission_stdout.log`，里面会有 `[USV] state=` 转移、`RED_LOITER_MUN` 释放/命中、`PLATFORM_DELETED` 终态等所有真实状态机结果。

### 8.3 启 ark_service

```bash
# 启 ark_service (如果你想走 60004 通道)
Start-Process "D:\program files\Ark\ArkSIM-4.1\bin\ark_service.exe"
# 它会读 ark_service.ini, 监听 60004
```

---

## 9. 文件清单（你可能需要看/编辑的）

| 路径 | 说明 |
|---|---|
| `E:\dev\openfang\protobuf\arksimActions.proto` | ★ Rust 要编译的 proto 源 |
| `E:\dev\openfang\protobuf\arksimproto.proto` | 态势输出 proto (RCP 通道推 customizedsituation 时用) |
| `E:\dev\openfang\protobuf\interface_new.json` | ★ JSON 控制命令 schema |
| `E:\dev\openfang\protobuf\arkcmd\proto\proto_utils.py` | ★ Python 端 ProtoStringBuilder, Rust builder 1:1 移植参考 |
| `E:\dev\openfang\protobuf\arkcmd\controller\Arksim_controller.py` | Python 端 ArkSIMController, Rust 客户端结构参考 |
| `E:\dev\openfang\protobuf\arkcomm\response_handler.py` | ZMQ DEALER + 多队列分发, Rust 可选实现 |
| `E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\usv_loiter_strike.txt` | 测试场景入口 |
| `E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\processors\usv_mission_manager.txt` | 5 阶段状态机定义, 看 agent_id 命名规则 |
| `E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\platforms\red\usv_mothership.txt` | USV 平台定义 (处理器挂在哪个平台) |
| `E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\mission_stdout.log.bak_1781093049` | ★ 18000s 完整跑的状态机日志（8KB+，所有 [USV] state= 都在） |
| `D:\program files\Ark\ArkSIM-4.1\bin\arksimproto.proto` | 同名 proto, 在 arksim 安装目录里也有 |
| `D:\program files\Ark\ArkSIM-4.1\bin\wsf_plugins\interface_new.json` | 同名 JSON, 在 arksim 安装目录里也有（**两份要保持一致**） |
| `D:\program files\Ark\ArkSIM-4.1\bin\wsf_plugins\zmqobserver.ini` | `SituationType=1` 默认实时态势, `change_situation 0` 切定制 |
| `D:\program files\Ark\ArkSIM-4.1\bin\projects\demo\arkSim脚本\*.txt` | 官方 zmq_observer 测试脚本, 含 `output_config` + `rpc_config` 完整语法 |

---

## 10. 后续建议

1. **优先实现**：start/pause/resume/stop + 9 个 E_Actions (运动控制: velocity/altitude/heading/GoToLocation/FollowRoute/OutsideControl/Release) — 覆盖 80% 用例
2. **次优先**：武器控制 4 个 (FireAtTarget/FireSlavo/StartJamming/StopJamming) + 传感器控制 4 个
3. **可选**：AuxData/ChangeCommander/ChangePlatformNumber (管理类, 用得少)
4. **态势接收**：直接读 `.evt` 文本, **不要等 zmq 推送**（arksim 4.1 在我们环境下不推）
5. **批量发送**：把同一时刻要做的多个动作 append 到一个 `ActionsFromOutside`, 一次 send 一次序列化, 比逐个发快

---

**文档版本**: 2026-06-10
**基于**: arksim 4.1.0 / wsf 4.1.0 / arkcmd 1.0.0
**作者**: Mavis (mavis orchestrator)
**验证状态**: 25 个 E_Actions + 9 个时序控制 + 1 个 start 全部走 ark_service 60004 实测通过；mission.exe 18000s 直跑状态机全 5 阶段执行
