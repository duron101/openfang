"""
最终方案：双进程并行
A) ark_service 60004 通道：用 arkcmd 客户端发全部 25+9 个接口（验证接口契约）
B) mission.exe 60s 短场景：直跑看真实状态机/事件输出
"""
import sys
import os
import time
import json
import base64
import subprocess
import threading
import types
import zmq
import logging

# mock arkcore
fake = types.ModuleType('arkcore')
fake_logging = types.ModuleType('arkcore.logging')
fake_logging.setup_logger = lambda *a, **k: logging.getLogger('mock')
fake.logging = fake_logging
sys.modules['arkcore'] = fake
sys.modules['arkcore.logging'] = fake_logging

sys.path.insert(0, r'E:\dev\openfang\protobuf')
from arkcmd import ArkSIMController, ProtoStringBuilder, SimulationConfig, SituationType

# ============ 配置 ============
ARK_SVC = "tcp://127.0.0.1:60004"
SCEN_FULL = r"E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\usv_loiter_strike.txt"
SCEN_SHORT = r"E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\usv_loiter_strike_short.txt"
SCEN_DIR = r"E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike"
LOG = r"E:\dev\openfang\protobuf\warlock_final.log"

with open(LOG, 'w', encoding='utf-8') as f:
    f.write("")

def log(msg):
    line = f"[{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    with open(LOG, 'a', encoding='utf-8') as f:
        f.write(line + "\n")

# ============ A. 启 mission 直跑 60s 短场景 ============
log("=" * 70)
log("A. 启 mission 直跑 usv_loiter_strike_short.txt (60s)")
log("=" * 70)

# 清旧 log
for fn in ["warlock.log", "mission_stdout.log", "mission.log"]:
    p = os.path.join(SCEN_DIR, fn)
    if os.path.exists(p):
        try: os.remove(p)
        except: pass
# 清旧 .aer
for fn in os.listdir(os.path.join(SCEN_DIR, "output")):
    if fn.endswith(('.aer', '.evt', '.log')):
        try: os.remove(os.path.join(SCEN_DIR, "output", fn))
        except: pass

mission = subprocess.Popen(
    [r"D:\program files\Ark\ArkSIM-4.1\bin\mission.exe", SCEN_SHORT],
    cwd=SCEN_DIR,
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    text=True, encoding='gbk', errors='ignore', bufsize=1,
)
log(f"  mission pid={mission.pid}")

def mission_reader():
    for line in iter(mission.stdout.readline, ''):
        if 'aSimTime' in line: continue
        if line.strip():
            log(f"  [mission] {line.rstrip()[:200]}")
    mission.stdout.close()
threading.Thread(target=mission_reader, daemon=True).start()

# 等 5s 让 mission 加载
time.sleep(5)

# ============ B. 用 arkcmd 客户端走 60004 发指令 ============
log("\n" + "=" * 70)
log("B. 用 arkcmd 客户端 + arkcomm 走 ark_service 60004 发指令")
log("=" * 70)

controller = ArkSIMController(service_address=ARK_SVC)
rh = controller.response_handler
log(f"  controller + ResponseHandler 就绪, socket_id={rh.socket_id}")

# 后台收 command 响应
def cmd_recv():
    while True:
        try:
            msg = rh.get_command(block=True, timeout=0.5)
            if msg is None: continue
            log(f"  [command] {json.dumps(msg, ensure_ascii=False)[:200]}")
        except: break
threading.Thread(target=cmd_recv, daemon=True).start()

# start instance
log("\n--- B.1 start instance ---")
start_cmd = controller.start_instance(scenarios=[SCEN_FULL], offscreen=False, random_seed=12345, realtime=False, sim_type=0)
rh.send_command(start_cmd)
log(f"  start payload: {json.dumps(start_cmd)[:150]}")

uuid_val = rh.get_uuid(timeout=30)
log(f"  uuid = {uuid_val}")
if not uuid_val:
    log("FATAL: no uuid")
    sys.exit(1)

# apply_default_situation
log("\n--- B.2 apply_default_situation ---")
for cmd in controller.apply_default_situation(uuid_val, interval=3.0):
    log(f"  >> {json.dumps(cmd)[:150]}")
    rh.send_command(cmd)
    time.sleep(0.3)

# resume
log("\n--- B.3 resume ---")
rh.send_command(controller.resume_simulation(uuid_val))
time.sleep(1)

# ============ C. 发 25 个 E_Actions 实体动作指令 ============
log("\n" + "=" * 70)
log("C. 用 arkcmd.ProtoStringBuilder 发 25 个 E_Actions 动作指令")
log("=" * 70)
builder = ProtoStringBuilder()
AGENT = "usv_mothership_1"

actions = [
    ('E_SetAgentOutsideControl', lambda: builder.set_agent_outside_control(AGENT)),
    ('E_ReleaseOutsideControl', lambda: builder.release_outside_control(AGENT)),
    ('E_SetDesiredVelocity', lambda: builder.set_desired_velocity(AGENT, 12.0, 0.5)),
    ('E_SetDesiredAltitude', lambda: builder.set_desired_altitude(AGENT, 50.0, 2.0)),
    ('E_SetDesiredHeading', lambda: builder.set_desired_heading(AGENT, 1.5708, 10.0, 1)),
    ('E_GoToLocation', lambda: builder.go_to_location(AGENT, [20.5, 122.5, 0.0], 1)),
    ('E_FollowRoute', lambda: builder.follow_route(AGENT, "rt_test", [
        {"id": "wp1", "speed": 8.0, "location": [20.4, 122.4, 0.0]},
    ])),
    ('E_AuxActions', lambda: builder.set_aux_data(AGENT, [
        {"key": "k1", "type": 0, "value": "v"},
        {"key": "k2", "type": 1, "value": 3.14},
        {"key": "k3", "type": 2, "value": True},
    ])),
    ('E_TurnOnSensor', lambda: builder.turn_on_sensor(AGENT, "")),
    ('E_TurnOffSensor', lambda: builder.turn_off_sensor(AGENT, "")),
    ('E_ChangeSensorMode', lambda: builder.change_sensor_mode(AGENT, "", "search")),
    ('E_GetSensorCurrentMode', lambda: builder.get_sensor_current_mode(AGENT, "")),
    ('E_FireAtTarget', lambda: builder.fire_at_target(AGENT, "", "")),
    ('E_FireSlavoAtTarget', lambda: builder.fire_salvo_at_target(AGENT, "", "", 2)),
    ('E_UpdateTarget', lambda: builder.update_target(AGENT, "")),
    ('E_StartJamming', lambda: builder.start_jamming(AGENT, "", "")),
    ('E_StopJamming', lambda: builder.stop_jamming(AGENT, "")),
    ('E_ChangeJammingMode', lambda: builder.change_jamming_mode(AGENT, "", 1e9, 1e6, 1)),
    ('E_TurnOnComm', lambda: builder.turn_on_comm(AGENT, "")),
    ('E_TurnOffComm', lambda: builder.turn_off_comm(AGENT, "")),
    ('E_SendMsgToPlatform', lambda: builder.send_msg_to_platform(AGENT, "", "blue_patrol_1", "ping")),
    ('E_SendMsgToCommandChain', lambda: builder.send_msg_to_command_chain(AGENT, "", "chain_red", 0, "cmd")),
    ('E_ChangePlatformNumber_add', lambda: builder.change_platform_number(
        "probe_new_1", True, "J7_UAV", "red", 121.0, 20.0, 1000.0, 90.0, 50.0)),
    ('E_ChangePlatformNumber_del', lambda: builder.change_platform_number(
        "probe_new_1", False, "J7_UAV", "red", 0, 0, 0, 0, 0)),
    ('E_ChangeCommander', lambda: builder.change_commander("usv_mothership_1", "red_cmd")),
]

for name, fn in actions:
    log(f"  -- {name} --")
    fn()
    proto_bytes = builder.serialize_actions()
    b64 = base64.b64encode(proto_bytes).decode('ascii')
    rh.send_command(controller.send_entity_command(uuid_val, b64))
    builder.clear_actions()
    time.sleep(0.2)

# 时序控制
log("\n--- C.2 时序控制命令 ---")
for name, cmd in [
    ('pause', controller.pause_simulation(uuid_val)),
    ('runstep', controller.run_step(uuid_val, 1)),
    ('advance_to_time', controller.advance_to_time(uuid_val, 100.0)),
    ('set_clock_rate', controller.set_clock_rate(uuid_val, 1.0)),
    ('simulationtimeswitch', controller.toggle_simulation_time_output(uuid_val, True)),
    ('restart', controller.restart_simulation(uuid_val)),
]:
    log(f"  -- {name} --")
    log(f"    >> {json.dumps(cmd)[:150]}")
    rh.send_command(cmd)
    time.sleep(0.2)

# stop
log("\n--- C.3 stop ---")
rh.send_command(controller.stop_simulation(uuid_val))
time.sleep(0.5)

# ============ D. 等 mission 跑完 60s, 收 mission 输出文件 ============
log("\n" + "=" * 70)
log("D. 等 mission 跑完, 收 output/ 文件")
log("=" * 70)
# mission 60s 短场景需 ~10-15s 跑完 (simtime 600s, 默认 100x 速)
end = time.time() + 30
while time.time() < end and mission.poll() is None:
    time.sleep(1)

if mission.poll() is not None:
    log(f"  mission exited rc={mission.returncode}")
else:
    log("  mission 仍在跑, kill")
    mission.terminate()
    try: mission.wait(timeout=5)
    except: mission.kill()

time.sleep(2)

# ============ E. 读取 mission 输出 ============
for fn in ["mission.log", "mission_stdout.log", "warlock.log"]:
    p = os.path.join(SCEN_DIR, fn)
    if os.path.exists(p):
        size = os.path.getsize(p)
        log(f"  {fn}: {size} bytes")

# 关键状态转移
ms_path = os.path.join(SCEN_DIR, "mission_stdout.log")
if os.path.exists(ms_path):
    with open(ms_path, 'r', encoding='gbk', errors='ignore') as f:
        content = f.read()
    log(f"\n=== mission_stdout.log 关键状态机日志 ===")
    for line in content.splitlines():
        if '[USV]' in line and 'state=' in line:
            log(f"  STATE: {line.strip()[:200]}")
        elif 'RED_LOITER_MUN' in line and ('LAUNCH' in line or 'TERMINAL' in line or 'HIT' in line):
            log(f"  STRIKE: {line.strip()[:200]}")
        elif 'J-7 UAV' in line and 'launched' in line:
            log(f"  UAV: {line.strip()[:200]}")
        elif 'Wave-' in line and 'fired' in line:
            log(f"  WAVE: {line.strip()[:200]}")
        elif 'state=COMMAND' in line:
            log(f"  >>> FINAL: {line.strip()[:200]}")

# output/ 内容
out_dir = os.path.join(SCEN_DIR, "output")
if os.path.exists(out_dir):
    log(f"\n=== output/ 目录 ===")
    for fn in sorted(os.listdir(out_dir)):
        p = os.path.join(out_dir, fn)
        log(f"  {fn}: {os.path.getsize(p)} bytes")

# aer 文件
aer_path = os.path.join(out_dir, "usv_loiter_strike.aer")
if os.path.exists(aer_path):
    with open(aer_path, 'rb') as f:
        content = f.read()
    log(f"  .aer 总长 {len(content)} bytes")
    # 看前 500 字节
    log(f"  .aer 前 200 chars: {content[:200]}")

# evt 文件
evt_path = os.path.join(out_dir, "usv_loiter_strike.evt")
if os.path.exists(evt_path):
    with open(evt_path, 'r', encoding='utf-8', errors='ignore') as f:
        content = f.read()
    log(f"\n=== .evt 关键事件 (前 50 行) ===")
    for line in content.splitlines()[:50]:
        log(f"  EVT: {line[:200]}")

log("\n=== END ===")
