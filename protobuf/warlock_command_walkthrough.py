#!/usr/bin/env python3
"""
Warlock 手动观察 — 常规控制指令逐步执行脚本

推荐用法 (手动逐个测试，避免 Warlock 崩溃):
  1. Warlock 加载 usv_loiter_strike，Pause 后运行
  2. python warlock_command_walkthrough.py --list          # 查看编号
  3. python warlock_command_walkthrough.py                 # Enter 发送 / s 跳过 / q 退出
  4. python warlock_command_walkthrough.py --at 12         # 只测第 12 条

警告: --closed-loop --no-prompt 会连续轰炸 18000，曾导致 Warlock 4.1 空指针崩溃；
      批量自动化请用 mission.exe，Warlock 仅用手动步进。

真实 wire 协议 (WSF_ZMQ_PROCESSOR):
  ZMQ PAIR @ 18000 → send ActionsFromOutside (裸 protobuf) → recv StateMessage
"""
from __future__ import annotations

import argparse
import base64
import json
import socket
import struct
import sys
import threading
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Dict, List, Optional, Tuple, Union

import zmq

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(REPO / "scripts" / "gen"))

from arkcmd.proto.proto_utils import ProtoStringBuilder  # noqa: E402

try:
    import afsimproto_pb2 as state_pb2  # noqa: E402
except ImportError:
    state_pb2 = None  # type: ignore

from usv_self_components import (  # noqa: E402
    BLUE_PATROL_1,
    DEFAULT_COMM,
    DEFAULT_SENSOR,
    DEFAULT_SENSOR_MODE,
    log_component_summary,
)

# 想定缺省 (usv_loiter_strike 红方 USV) — 组件树见 usv_self_components.py
from usv_self_components import DEFAULT_AGENT, DEFAULT_TRACK, DEFAULT_WEAPON  # noqa: E402
DEFAULT_TCP_PORT = 18000
DEFAULT_ARK_SERVICE = "tcp://127.0.0.1:60004"
DEFAULT_SCENARIO = r"E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\usv_loiter_strike.txt"
LOG_PATH = ROOT / "warlock_command_walkthrough.log"
CLOSED_LOOP_REPORT_PATH = ROOT / "warlock_closed_loop_report.json"
HANDSHAKE_LEN = 10

# 空 ActionsFromOutside — PAIR 模式下允许纯步进（0 字节）
EMPTY_ACTIONS = b""

# 闭环测试时延后执行，避免提前释放外部控制
CLOSED_LOOP_DEFER = frozenset({
    "E_ReleaseOutsideControl",
    "E_ChangePlatformNumber_del",
})

# Warlock 4.1 上连续/不当调用可能导致 WsfPlugin 崩溃 (空指针)
WARLOCK_RISKY = frozenset({
    "E_ChangePlatformNumber_add",
    "E_ChangePlatformNumber_del",
    "E_FollowRoute",           # rt_test 想定中可能不存在
    "E_ChangeCommander",       # red_cmd 可能无效
    "E_SendMsgToCommandChain", # chain_red 可能无效
    "E_FireSlavoAtTarget",
    "E_StartJamming",
    "E_ChangeJammingMode",
    "E_StopJamming",
})


def port_is_listening(host: str, port: int, timeout: float = 1.0) -> bool:
    try:
        s = socket.create_connection((host, port), timeout=timeout)
        s.close()
        return True
    except OSError:
        return False


def print_tcp_unavailable_help(host: str, port: int) -> None:
    log("")
    log("诊断: tcp://{}:{} 无进程监听 (WinError 10061)".format(host, port))
    log("  常见原因:")
    log("  1) Warlock GUI 模式通常不对外暴露 18000（evt 有 zmq_controller 但端口仍拒绝连接）")
    log("  2) 未加载 usv_loiter_strike.txt，或仿真未 Play（ZMQ 在 T=0.3s 才创建）")
    log("  3) 需要 mission.exe 直跑才稳定监听 18000")
    log("")
    log("可行替代 (无需 18000，可在 Warlock 界面观察):")
    log("  python warlock_command_walkthrough.py --mode ark_service --auto-start \\")
    log("    --only E_SetAgentOutsideControl,E_FireAtTarget --no-prompt")
    log("")
    log("或一键自动测试:")
    log("  python test_fire_at_target.py")


def log(msg: str) -> None:
    line = f"[{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    with open(LOG_PATH, "a", encoding="utf-8") as f:
        f.write(line + "\n")


@dataclass
class CommandStep:
    name: str
    description: str
    build: Callable[[ProtoStringBuilder], None]
    allow_empty: bool = False
    risky: bool = False
    uses_platform_index: bool = False  # Aux: PlatformAuxData.index 须来自 StateMessage


@dataclass
class StepResult:
    """ZMQ/Framed 单步 send→recv 结果。"""

    label: str
    send_bytes: int
    recv_ok: bool
    payload_bytes: int = 0
    sim_time: Optional[float] = None
    platform_count: int = 0
    agent_found: bool = False
    error: str = ""
    summary: str = ""

    @property
    def closed_loop_ok(self) -> bool:
        if not self.recv_ok or self.payload_bytes <= 0:
            return False
        if state_pb2 is None:
            return True
        return self.sim_time is not None and self.error == ""


@dataclass
class ClosedLoopReport:
    mode: str
    wire: str
    agent: str
    total: int = 0
    passed: int = 0
    failed: int = 0
    steps: List[Dict[str, object]] = field(default_factory=list)

    def to_dict(self) -> Dict[str, object]:
        return {
            "mode": self.mode,
            "wire": self.wire,
            "agent": self.agent,
            "total": self.total,
            "passed": self.passed,
            "failed": self.failed,
            "pass_rate": f"{self.passed}/{self.total}",
            "steps": self.steps,
        }


def build_empty_actions() -> bytes:
    return EMPTY_ACTIONS


def order_catalog_for_closed_loop(catalog: List[CommandStep]) -> List[CommandStep]:
    head = [c for c in catalog if c.name not in CLOSED_LOOP_DEFER]
    tail = [c for c in catalog if c.name in CLOSED_LOOP_DEFER]
    return head + tail


def apply_catalog_filters(args: argparse.Namespace, catalog: List[CommandStep]) -> List[CommandStep]:
    if args.only:
        names = {n.strip() for n in args.only.split(",") if n.strip()}
        catalog = [c for c in catalog if c.name in names]
    if args.skip:
        skip = {n.strip() for n in args.skip.split(",") if n.strip()}
        catalog = [c for c in catalog if c.name not in skip]
    if getattr(args, "skip_risky", False) and not getattr(args, "include_risky", False):
        catalog = [c for c in catalog if not c.risky]
    if getattr(args, "at", 0) > 0:
        idx = args.at - 1
        if idx < 0 or idx >= len(catalog):
            raise ValueError(f"--at {args.at} 超出范围 1..{len(catalog)}")
        catalog = [catalog[idx]]
    return catalog


def print_catalog_list(catalog: List[CommandStep]) -> None:
    log("指令清单 (手动测试请用 --at N 或默认交互模式):")
    for i, step in enumerate(catalog, 1):
        data, _ = serialize_command_step(
            step, agent=DEFAULT_AGENT, weapon=DEFAULT_WEAPON, track=DEFAULT_TRACK,
            platform_index=1 if step.uses_platform_index else None,
        )
        tag = " [RISKY]" if step.risky else ""
        aux = " [needs index]" if step.uses_platform_index else ""
        log(f"  {i:2d}. {step.name:<28} {len(data):3d}B  {step.description}{tag}{aux}")


def parse_state_payload(payload: bytes, agent: str) -> Tuple[Optional[float], int, bool, str]:
    if state_pb2 is None:
        return None, 0, False, "未安装 afsimproto_pb2"
    msg = state_pb2.StateMessage()
    try:
        msg.ParseFromString(payload)
    except Exception as exc:
        return None, 0, False, str(exc)
    agent_found = any(p.name == agent or agent in p.name for p in msg.platforms)
    return msg.time, len(msg.platforms), agent_found, ""


def platform_indices_from_state(payload: bytes) -> Dict[str, int]:
    """从 StateMessage 提取 name → PlatformState.index（Aux 指令必填）。"""
    if state_pb2 is None or not payload:
        return {}
    msg = state_pb2.StateMessage()
    try:
        msg.ParseFromString(payload)
    except Exception:
        return {}
    return {p.name: int(p.index) for p in msg.platforms}


def lookup_platform_index(payload: bytes, platform_name: str) -> Optional[int]:
    return platform_indices_from_state(payload).get(platform_name)


def pick_track_for_agent(
    payload: bytes,
    agent: str,
    *,
    fallback: str = DEFAULT_TRACK,
) -> Tuple[str, str]:
    """从 StateMessage 为红方平台选取 FireAtTarget 用的 trackId (name:number)。"""
    from arkcmd.proto.proto_utils import normalize_track_id

    fb = normalize_track_id(fallback)
    if state_pb2 is None or not payload:
        return fb, "fallback(no state_pb2)"
    msg = state_pb2.StateMessage()
    try:
        msg.ParseFromString(payload)
    except Exception as exc:
        return fb, f"fallback(parse:{exc})"

    def side_is_foe(side: str, iff: str) -> bool:
        s = (side or "").lower()
        i = (iff or "").lower()
        return s in ("blue", "foe", "hostile", "enemy") or i in ("foe", "hostile", "enemy")

    for plat in msg.platforms:
        if plat.name != agent and agent not in plat.name:
            continue
        foe_tracks: List[str] = []
        any_tracks: List[str] = []
        for tr in plat.tracks:
            tid = normalize_track_id(tr.trackId or "")
            if not tid:
                continue
            any_tracks.append(tid)
            if side_is_foe(tr.side, tr.iff):
                foe_tracks.append(tid)
        if foe_tracks:
            return foe_tracks[0], f"state(foe track, {len(plat.tracks)} total)"
        if any_tracks:
            return any_tracks[0], f"state(first track, {len(plat.tracks)} total)"
        return fb, f"fallback(agent {agent!r} has 0 tracks @ t={msg.time:.1f}s)"
    return fb, f"fallback(agent {agent!r} not in state)"


AUX_DATA_SAMPLES: List[Dict[str, object]] = [
    {"key": "walkthrough", "type": 0, "value": "ping"},
    {"key": "speed_hint", "type": 1, "value": 3.14},
    {"key": "flag", "type": 2, "value": True},
]


def serialize_command_step(
    step: CommandStep,
    *,
    agent: str,
    weapon: str,
    track: str,
    platform_index: Optional[int] = None,
) -> Tuple[bytes, Optional[str]]:
    builder = ProtoStringBuilder()
    if step.uses_platform_index:
        if platform_index is None:
            return b"", (
                f"{step.name} 需要 StateMessage 里平台 {agent!r} 的 index "
                f"(非 0；见 mid_ark seenPlatformNames)"
            )
        builder.set_aux_data(agent, AUX_DATA_SAMPLES, index=platform_index)
    else:
        step.build(builder)
    return builder.serialize_actions(), None


def resolve_platform_index(
    client: Optional[Union[ZmqStepClient, FramedTcpStepClient]],
    agent: str,
    *,
    sync_if_missing: bool = True,
) -> Optional[int]:
    if not isinstance(client, ZmqStepClient):
        return None
    idx = client.platform_index(agent)
    if idx is not None or not sync_if_missing:
        return idx
    log(f"  同步态势以解析 {agent!r} 的 platform index ...")
    sync = client.step(EMPTY_ACTIONS, label="platform_index_sync")
    if not sync.recv_ok:
        return None
    return client.platform_index(agent)


def build_command_catalog(
    agent: str,
    weapon: str,
    track: str,
    *,
    sensor: str = DEFAULT_SENSOR,
    comm: str = DEFAULT_COMM,
    sensor_mode: str = DEFAULT_SENSOR_MODE,
    msg_target: str = BLUE_PATROL_1,
) -> List[CommandStep]:
    """25 条 E_Actions 实体控制指令 (与 arksimActions.proto / mid_ark ADAP 路径一致)。"""
    return [
        CommandStep(
            "E_SetAgentOutsideControl",
            "设置外部可控制实体",
            lambda b: b.set_agent_outside_control(agent),
        ),
        CommandStep(
            "E_ReleaseOutsideControl",
            "释放外部控制",
            lambda b: b.release_outside_control(agent),
        ),
        CommandStep(
            "E_SetDesiredVelocity",
            "设置期望速度 12 m/s",
            lambda b: b.set_desired_velocity(agent, 12.0, 0.5),
        ),
        CommandStep(
            "E_SetDesiredAltitude",
            "设置期望高度 50 m",
            lambda b: b.set_desired_altitude(agent, 50.0, 2.0),
        ),
        CommandStep(
            "E_SetDesiredHeading",
            "设置期望航向 π/2 rad (东)",
            lambda b: b.set_desired_heading(agent, 1.5708, 10.0, 1),
        ),
        CommandStep(
            "E_GoToLocation",
            "航渡至 20.5°N 122.5°E",
            lambda b: b.go_to_location(agent, [20.5, 122.5, 0.0], 1),
        ),
        CommandStep(
            "E_FollowRoute",
            "跟随测试航线 rt_test",
            lambda b: b.follow_route(agent, "rt_test", [
                {"id": "wp1", "speed": 8.0, "location": [20.4, 122.4, 0.0]},
            ]),
        ),
        CommandStep(
            "E_AuxActions",
            "AUX 透传 (string/double/bool)",
            lambda b: None,  # 发送时按 StateMessage.index 动态构建
            uses_platform_index=True,
        ),
        CommandStep(
            "E_TurnOnSensor",
            f"打开传感器 {sensor!r}",
            lambda b: b.turn_on_sensor(agent, sensor),
        ),
        CommandStep(
            "E_TurnOffSensor",
            f"关闭传感器 {sensor!r}",
            lambda b: b.turn_off_sensor(agent, sensor),
        ),
        CommandStep(
            "E_ChangeSensorMode",
            f"切换传感器 {sensor!r} 模式 {sensor_mode!r}",
            lambda b: b.change_sensor_mode(agent, sensor, sensor_mode),
        ),
        CommandStep(
            "E_GetSensorCurrentMode",
            f"查询传感器 {sensor!r} 当前模式",
            lambda b: b.get_sensor_current_mode(agent, sensor),
        ),
        CommandStep(
            "E_FireAtTarget",
            f"对目标开火 track={track!r} weapon={weapon!r}",
            lambda b: b.fire_at_target(agent, weapon, track),
        ),
        CommandStep(
            "E_FireSlavoAtTarget",
            f"齐射 track={track!r} salvo=2",
            lambda b: b.fire_salvo_at_target(agent, weapon, track, 2),
        ),
        CommandStep(
            "E_UpdateTarget",
            f"更新目标 (sensor={sensor!r})",
            lambda b: b.update_target(agent, sensor),
        ),
        CommandStep(
            "E_StartJamming",
            f"开启干扰 track={track!r} (self 无 jammer 组件，[RISKY])",
            lambda b: b.start_jamming(agent, "", track),
        ),
        CommandStep(
            "E_StopJamming",
            "停止干扰 (self 无 jammer)",
            lambda b: b.stop_jamming(agent, ""),
        ),
        CommandStep(
            "E_ChangeJammingMode",
            "切换干扰模式 (self 无 jammer)",
            lambda b: b.change_jamming_mode(agent, "", 1e9, 1e6, 1),
        ),
        CommandStep(
            "E_TurnOnComm",
            f"通信开机 {comm!r}",
            lambda b: b.turn_on_comm(agent, comm),
        ),
        CommandStep(
            "E_TurnOffComm",
            f"通信关机 {comm!r}",
            lambda b: b.turn_off_comm(agent, comm),
        ),
        CommandStep(
            "E_SendMsgToPlatform",
            f"经 {comm!r} 向 {msg_target!r} 发送消息",
            lambda b: b.send_msg_to_platform(agent, comm, msg_target, "walkthrough_ping"),
        ),
        CommandStep(
            "E_SendMsgToCommandChain",
            "向指挥链 chain_red 发送消息",
            lambda b: b.send_msg_to_command_chain(agent, "", "chain_red", 0, "walkthrough_cmd"),
        ),
        CommandStep(
            "E_ChangePlatformNumber_add",
            "动态增加平台 probe_new_1",
            lambda b: b.change_platform_number(
                "probe_new_1", True, "J7_UAV", "red", 121.0, 20.0, 1000.0, 90.0, 50.0,
            ),
        ),
        CommandStep(
            "E_ChangePlatformNumber_del",
            "删除动态平台 probe_new_1",
            lambda b: b.change_platform_number(
                "probe_new_1", False, "J7_UAV", "red", 0, 0, 0, 0, 0,
            ),
        ),
        CommandStep(
            "E_ChangeCommander",
            "变更指挥官 red_cmd",
            lambda b: b.change_commander(agent, "red_cmd"),
        ),
    ]
    for step in catalog:
        step.risky = step.name in WARLOCK_RISKY
    return catalog


def catalog_component_kwargs(args: argparse.Namespace) -> dict:
    return {
        "sensor": getattr(args, "sensor", DEFAULT_SENSOR),
        "comm": getattr(args, "comm", DEFAULT_COMM),
        "sensor_mode": getattr(args, "sensor_mode", DEFAULT_SENSOR_MODE),
        "msg_target": getattr(args, "msg_target", BLUE_PATROL_1),
    }


def build_catalog_for_args(args: argparse.Namespace) -> List[CommandStep]:
    return build_command_catalog(
        args.agent,
        args.weapon,
        args.track,
        **catalog_component_kwargs(args),
    )


def _read_exact(sock: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError(f"连接断开，还需 {n - len(buf)} 字节")
        buf += chunk
    return buf


def _recv_framed(sock: socket.socket) -> bytes:
    plen = struct.unpack("<I", _read_exact(sock, 4))[0]
    if plen <= 0 or plen > 32 * 1024 * 1024:
        raise ValueError(f"非法帧长度: {plen}")
    return _read_exact(sock, plen)


def _send_framed(sock: socket.socket, payload: bytes) -> None:
    sock.sendall(struct.pack("<I", len(payload)) + payload)


def summarize_state(payload: bytes, agent: str) -> str:
    if state_pb2 is None:
        return f"payload={len(payload)}B (未安装 afsimproto_pb2，跳过解析)"
    msg = state_pb2.StateMessage()
    try:
        msg.ParseFromString(payload)
    except Exception as exc:
        return f"StateMessage 解析失败: {exc} ({len(payload)}B)"
    parts = [f"t={msg.time:.1f}s platforms={len(msg.platforms)}"]
    for p in msg.platforms:
        if p.name == agent or agent in p.name:
            head = p.orientationNED[0] if len(p.orientationNED) >= 1 else float("nan")
            vel = p.velocityNED[0] if len(p.velocityNED) >= 1 else float("nan")
            parts.append(f"{p.name} side={p.side} hdg={head * 57.3:.1f}° vel={vel:.1f}m/s")
            break
    else:
        names = [p.name for p in msg.platforms[:6]]
        parts.append(f"未找到 {agent!r}，已有: {names}")
    return " | ".join(parts)


class ZmqStepClient:
    """mid_ark MsgHandler(use_zmq_socket=True) 同款: ZMQ PAIR + 裸 protobuf。"""

    def __init__(
        self,
        host: str,
        port: int,
        connect_timeout: float,
        agent: str,
        step_timeout: float = 30.0,
    ):
        self.agent = agent
        self._recv_count = 0
        self._platform_index_by_name: Dict[str, int] = {}
        self._last_payload: bytes = b""
        self.ctx = zmq.Context.instance()
        self.sock = self.ctx.socket(zmq.PAIR)
        self.sock.setsockopt(zmq.LINGER, 0)
        self.sock.setsockopt(zmq.RCVTIMEO, int(max(step_timeout, 5) * 1000))
        self.sock.setsockopt(zmq.SNDTIMEO, 10000)
        deadline = time.time() + connect_timeout
        last_err: Optional[Exception] = None
        while time.time() < deadline:
            try:
                self.sock.connect(f"tcp://{host}:{port}")
                last_err = None
                break
            except zmq.ZMQError as exc:
                last_err = exc
                time.sleep(1.0)
        if last_err is not None:
            print_tcp_unavailable_help(host, port)
            raise RuntimeError(
                f"无法连接 zmq://{host}:{port} — 请确认 mission 已加载想定: {last_err}"
            )
        log(f"ZMQ PAIR 已连接 {host}:{port} (裸 protobuf，首帧在首条指令后返回)")

    def step(self, proto_bytes: bytes, *, label: str = "step") -> StepResult:
        send_len = len(proto_bytes)
        try:
            self.sock.send(proto_bytes)
            payload = self.sock.recv()
        except zmq.Again:
            return StepResult(
                label=label,
                send_bytes=send_len,
                recv_ok=False,
                error="recv timeout (ZMQ RCVTIMEO)",
            )
        except zmq.ZMQError as exc:
            return StepResult(
                label=label,
                send_bytes=send_len,
                recv_ok=False,
                error=f"ZMQ error: {exc}",
            )
        self._recv_count += 1
        self._last_payload = payload
        self._platform_index_by_name = platform_indices_from_state(payload)
        sim_time, plat_count, agent_found, err = parse_state_payload(payload, self.agent)
        summary = summarize_state(payload, self.agent)
        if self._platform_index_by_name:
            idx_hint = self._platform_index_by_name.get(self.agent)
            if idx_hint is not None:
                summary += f" | {self.agent}.index={idx_hint}"
        log(f"  << [{label}] #{self._recv_count} send={send_len}B: {summary}")
        return StepResult(
            label=label,
            send_bytes=send_len,
            recv_ok=True,
            payload_bytes=len(payload),
            sim_time=sim_time,
            platform_count=plat_count,
            agent_found=agent_found,
            error=err,
            summary=summary,
        )

    def platform_index(self, platform_name: str) -> Optional[int]:
        return self._platform_index_by_name.get(platform_name)

    def close(self) -> None:
        try:
            self.sock.close()
        except zmq.ZMQError:
            pass


class FramedTcpStepClient:
    """openfang bridge.rs 同款: TCP + 10B 握手 + LE u32 帧 (非 WSF_ZMQ 默认路径)。"""

    def __init__(self, host: str, port: int, connect_timeout: float, agent: str):
        self.agent = agent
        self._recv_count = 0
        deadline = time.time() + connect_timeout
        last_err: Optional[Exception] = None
        self.sock: Optional[socket.socket] = None
        while time.time() < deadline:
            try:
                s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                s.settimeout(10.0)
                s.connect((host, port))
                self.sock = s
                break
            except OSError as exc:
                last_err = exc
                time.sleep(1.0)
        if self.sock is None:
            print_tcp_unavailable_help(host, port)
            raise RuntimeError(
                f"无法连接 tcp://{host}:{port} — 请确认 mission.exe 直跑或改用 --mode ark_service --auto-start: {last_err}"
            )

        log(f"TCP framed 已连接 {host}:{port}")
        hs = _read_exact(self.sock, HANDSHAKE_LEN)
        log(f"  handshake: {hs.hex()} ({len(hs)}B)")
        self.sock.settimeout(3.0)
        try:
            initial = _recv_framed(self.sock)
            self._recv_count += 1
            log(f"  << 初始态势 #{self._recv_count}: {summarize_state(initial, agent)}")
        except (socket.timeout, TimeoutError, OSError):
            log("  (无初始态势推送，将在首条指令后收 StateMessage)")
        self.sock.settimeout(60.0)

    def step(self, proto_bytes: bytes, *, label: str = "step") -> StepResult:
        assert self.sock is not None
        send_len = len(proto_bytes)
        try:
            _send_framed(self.sock, proto_bytes)
            payload = _recv_framed(self.sock)
        except (socket.timeout, TimeoutError, OSError, ConnectionError, ValueError) as exc:
            return StepResult(
                label=label,
                send_bytes=send_len,
                recv_ok=False,
                error=str(exc),
            )
        self._recv_count += 1
        sim_time, plat_count, agent_found, err = parse_state_payload(payload, self.agent)
        summary = summarize_state(payload, self.agent)
        log(f"  << [{label}] #{self._recv_count} send={send_len}B: {summary}")
        return StepResult(
            label=label,
            send_bytes=send_len,
            recv_ok=True,
            payload_bytes=len(payload),
            sim_time=sim_time,
            platform_count=plat_count,
            agent_found=agent_found,
            error=err,
            summary=summary,
        )

    def close(self) -> None:
        if self.sock is not None:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None


class ArkServiceAutoStarter:
    """经 60004 start/resume 后 proto 发指令（Warlock GUI 无需 18000）。"""

    def __init__(self, service: str, scenario: str, sim_steps: int = 50):
        self.scenario = scenario
        self.sim_steps = sim_steps
        self.uuid = ""
        self.ctx = zmq.Context.instance()
        self.sock = self.ctx.socket(zmq.DEALER)
        self.sock.setsockopt(zmq.LINGER, 0)
        sid = f"walk_{uuid.uuid4().hex[:8]}"
        self.sock.setsockopt(zmq.ROUTING_ID, sid.encode())
        self.sock.connect(service)
        self.sock.setsockopt(zmq.RCVTIMEO, 500)
        self._recv_buf: List[str] = []
        self._recv_count = 0
        self._stop = False
        self._thread = threading.Thread(target=self._recv_loop, daemon=True)
        log(f"ArkService 已连接 {service}")
        self._thread.start()
        time.sleep(0.3)
        self._boot_instance()

    def _recv_loop(self) -> None:
        while not self._stop:
            try:
                frames = self.sock.recv_multipart()
            except zmq.Again:
                continue
            except zmq.ZMQError:
                break
            for f in frames:
                self._recv_count += 1
                text = f.decode("utf-8", errors="replace")
                self._recv_buf.append(text)
                log(f"  << recv #{self._recv_count} {text[:200]}")

    def _send_json(self, payload: dict) -> None:
        log(f"  >> {json.dumps(payload, ensure_ascii=False)[:240]}")
        self.sock.send(json.dumps(payload).encode("utf-8"))

    def _wait_uuid(self, timeout: float = 30.0) -> Optional[str]:
        end = time.time() + timeout
        while time.time() < end:
            for raw in self._recv_buf:
                try:
                    d = json.loads(raw)
                    uid = d.get("data", {}).get("uuid") or d.get("uuid")
                    if uid:
                        return uid
                except Exception:
                    pass
            time.sleep(0.2)
        return None

    def _boot_instance(self) -> None:
        log("ArkService auto-start: 拉起想定...")
        self._send_json({
            "fn": "start",
            "args": {
                "exec": 1,
                "offscreen": False,
                "randomSeed": int(time.time()) % 100000,
                "realtime": False,
                "scenarios": [self.scenario],
                "simType": 0,
            },
        })
        uid = self._wait_uuid()
        if not uid:
            raise RuntimeError("start 超时 — 请确认 ark_service.exe 在 60004 运行")
        self.uuid = uid
        self.sock.setsockopt(zmq.ROUTING_ID, uid.encode())
        log(f"  instance_uuid = {uid}")
        self._send_json({"fn": "resume", "uuid": uid})
        time.sleep(0.3)
        self._send_json({"fn": "runstep", "args": {"step": self.sim_steps}, "uuid": uid})
        time.sleep(0.5)

    def send_proto(self, proto_bytes: bytes) -> None:
        payload = {
            "fn": "proto",
            "proto": base64.b64encode(proto_bytes).decode("ascii"),
            "uuid": self.uuid,
        }
        self.sock.send(json.dumps(payload).encode("utf-8"))

    def close(self) -> None:
        self._stop = True
        time.sleep(0.2)
        self.sock.close()


class ArkServiceSender:
    """经 ArkService 60004 的 proto 通道 (需已有仿真实例 uuid)。"""

    def __init__(self, service: str, instance_uuid: str):
        self.uuid = instance_uuid
        self.ctx = zmq.Context.instance()
        self.sock = self.ctx.socket(zmq.DEALER)
        self.sock.setsockopt(zmq.LINGER, 0)
        sid = f"walk_{uuid.uuid4().hex[:8]}"
        self.sock.setsockopt(zmq.ROUTING_ID, sid.encode())
        self.sock.connect(service)
        self.sock.setsockopt(zmq.RCVTIMEO, 500)
        self._recv_count = 0
        self._stop = False
        self._thread = threading.Thread(target=self._recv_loop, daemon=True)
        self.sock.setsockopt(zmq.ROUTING_ID, instance_uuid.encode())
        log(f"ArkService 已连接 {service} uuid={instance_uuid}")
        self._thread.start()

    def _recv_loop(self) -> None:
        while not self._stop:
            try:
                frames = self.sock.recv_multipart()
            except zmq.Again:
                continue
            except zmq.ZMQError:
                break
            for f in frames:
                self._recv_count += 1
                text = f.decode("utf-8", errors="replace")
                log(f"  << recv #{self._recv_count} {text[:200]}")

    def send_proto(self, proto_bytes: bytes) -> None:
        payload = {
            "fn": "proto",
            "proto": base64.b64encode(proto_bytes).decode("ascii"),
            "uuid": self.uuid,
        }
        self.sock.send(json.dumps(payload).encode("utf-8"))

    def close(self) -> None:
        self._stop = True
        time.sleep(0.2)
        self.sock.close()


def prompt_user(step_idx: int, total: int, step: CommandStep, interactive: bool, delay: float) -> str:
    """返回 send | skip | quit"""
    log(f"\n{'=' * 60}")
    log(f"[{step_idx}/{total}] {step.name}")
    log(f"  {step.description}")
    if step.risky:
        log("  ⚠ 高风险: Warlock 上可能触发插件崩溃，建议 s 跳过或先备份想定")
    if step.uses_platform_index:
        log("  ℹ E_AuxActions: name 用平台名，index 须来自上一条 StateMessage (非固定 0)")
    if interactive:
        try:
            line = input("  → Enter=发送  s=跳过  q=退出: ").strip().lower()
        except EOFError:
            log("  (非交互终端，自动继续)")
            return "send"
        if line in ("q", "quit", "exit"):
            return "quit"
        if line in ("s", "skip", "n", "no"):
            return "skip"
        return "send"
    if delay > 0:
        log(f"  → {delay:.1f}s 后自动发送...")
        time.sleep(delay)
    return "send"


def _connect_step_client(args: argparse.Namespace) -> Union[ZmqStepClient, FramedTcpStepClient]:
    step_timeout = getattr(args, "step_timeout", 30.0)
    if args.wire == "zmq":
        return ZmqStepClient(
            args.host, args.port, args.connect_timeout, args.agent,
            step_timeout=step_timeout,
        )
    return FramedTcpStepClient(args.host, args.port, args.connect_timeout, args.agent)


def _evaluate_loop_step(
    result: StepResult,
    prev_time: Optional[float],
    *,
    require_time_advance: bool,
    require_agent: bool,
) -> Tuple[bool, str]:
    if not result.closed_loop_ok:
        if not result.recv_ok:
            return False, result.error or "recv 失败"
        if result.payload_bytes <= 0:
            return False, "回包为空"
        if result.error:
            return False, result.error
        return False, "StateMessage 解析失败"

    reasons: List[str] = []
    if require_agent and not result.agent_found:
        reasons.append(f"态势中未找到 agent={result.label}")
    if require_time_advance and prev_time is not None and result.sim_time is not None:
        if result.sim_time + 1e-6 < prev_time:
            reasons.append(f"仿真时间回退 {prev_time:.3f}→{result.sim_time:.3f}s")
    if reasons:
        return False, "; ".join(reasons)
    return True, "OK"


def run_closed_loop(args: argparse.Namespace) -> int:
    """全指令闭环测试: 每条 E_Actions send→recv StateMessage。"""
    LOG_PATH.write_text("", encoding="utf-8")
    full = build_catalog_for_args(args)
    try:
        catalog = apply_catalog_filters(args, order_catalog_for_closed_loop(full))
    except ValueError as exc:
        log(f"FAIL: {exc}")
        return 1
    if not catalog:
        log("FAIL: 过滤后无指令可测")
        return 1

    if not getattr(args, "include_risky", False):
        skipped = [c.name for c in full if c.risky and c not in catalog]
        if skipped and not args.only and not args.at:
            log(f"  闭环默认跳过 {len(skipped)} 条高风险指令 (用 --include-risky 启用)")
    if not args.no_prompt:
        log("  闭环手动模式: 每条 Enter 发送 / s 跳过 / q 退出")
    else:
        log("  ⚠ 闭环自动模式: 连续发送可能使 Warlock 崩溃，建议 mission.exe 或去掉 --no-prompt")

    if args.mode != "tcp":
        log("FAIL: --closed-loop 需要 mode=tcp (ZMQ PAIR send→recv)")
        log("  60004 ark_service 无 StateMessage 回包，不能验证 wire 闭环")
        return 1
    if not port_is_listening(args.host, args.port):
        log(f"FAIL: {args.host}:{args.port} 未监听")
        if args.fallback_ark_service:
            log("  ark_service 无法做 recv 闭环；请先 mission 直跑并开启 18000")
        return 1

    log("Warlock 全指令闭环测试 (PAIR send→recv)")
    log(f"  wire={args.wire}  agent={args.agent!r}  weapon={args.weapon!r}  track={args.track!r}")
    log(f"  共 {len(catalog)} 条  empty_pulse={args.empty_pulse}  report={CLOSED_LOOP_REPORT_PATH}")

    if args.dry_run:
        for i, cmd in enumerate(catalog, 1):
            data, err = serialize_command_step(
                cmd, agent=args.agent, weapon=args.weapon, track=args.track,
                platform_index=1 if cmd.uses_platform_index else None,
            )
            note = f" ({err})" if err else ""
            log(f"  [{i}/{len(catalog)}] {cmd.name} → {len(data)} bytes{note}")
        log(f"  空命令步进: {len(EMPTY_ACTIONS)} bytes (允许)")
        return 0

    client: Optional[Union[ZmqStepClient, FramedTcpStepClient]] = None
    report = ClosedLoopReport(mode=args.mode, wire=args.wire, agent=args.agent)
    prev_time: Optional[float] = None
    exit_code = 0

    try:
        client = _connect_step_client(args)

        if args.empty_sync:
            sync = client.step(EMPTY_ACTIONS, label="E_EmptySync")
            ok, reason = _evaluate_loop_step(
                sync, prev_time,
                require_time_advance=False,
                require_agent=False,
            )
            log(f"  空命令同步: {'PASS' if ok else 'FAIL'} — {reason}")
            if sync.sim_time is not None:
                prev_time = sync.sim_time

        for i, cmd in enumerate(catalog, 1):
            action = prompt_user(
                i, len(catalog), cmd,
                interactive=not args.no_prompt,
                delay=args.delay if args.no_prompt else 0.0,
            )
            if action == "quit":
                log("用户退出")
                break
            if action == "skip":
                log(f"  >> 跳过 {cmd.name}")
                report.steps.append({
                    "index": i, "name": cmd.name, "pass": None,
                    "reason": "skipped", "send_bytes": 0, "payload_bytes": 0,
                    "sim_time": None, "platform_count": 0, "agent_found": False,
                    "summary": "skipped",
                })
                continue

            if args.empty_pulse:
                pulse = client.step(EMPTY_ACTIONS, label=f"{cmd.name}__pulse")
                _evaluate_loop_step(
                    pulse, prev_time,
                    require_time_advance=False,
                    require_agent=False,
                )

            plat_idx = args.platform_index if args.platform_index >= 0 else None
            if cmd.uses_platform_index and plat_idx is None:
                plat_idx = resolve_platform_index(client, args.agent)
            proto_bytes, build_err = serialize_command_step(
                cmd,
                agent=args.agent,
                weapon=args.weapon,
                track=args.track,
                platform_index=plat_idx,
            )
            if build_err:
                log(f"  [FAIL] {cmd.name}: {build_err}")
                report.failed += 1
                exit_code = 1
                report.steps.append({
                    "index": i, "name": cmd.name, "pass": False,
                    "reason": build_err, "send_bytes": 0, "payload_bytes": 0,
                    "sim_time": None, "platform_count": 0, "agent_found": False,
                    "summary": build_err,
                })
                continue

            if len(proto_bytes) == 0:
                if cmd.allow_empty or args.allow_empty:
                    log(f"  >> {cmd.name} 空命令步进 (0 bytes)")
                else:
                    log(f"  >> {cmd.name} 序列化为 0 字节，按 --allow-empty 发空步进")
                    if not args.allow_empty:
                        report.total += 1
                        report.failed += 1
                        report.steps.append({
                            "index": i,
                            "name": cmd.name,
                            "pass": False,
                            "reason": "序列化 0 字节且未允许空命令",
                            "send_bytes": 0,
                        })
                        exit_code = 1
                        continue
            else:
                log(f"  >> send {cmd.name} ({len(proto_bytes)} bytes)")

            result = client.step(proto_bytes, label=cmd.name)
            ok, reason = _evaluate_loop_step(
                result,
                prev_time,
                require_time_advance=args.require_time_advance,
                require_agent=args.require_agent_in_state,
            )
            if result.sim_time is not None:
                prev_time = result.sim_time

            report.total += 1
            if ok:
                report.passed += 1
                status = "PASS"
            else:
                report.failed += 1
                status = "FAIL"
                exit_code = 1

            log(f"  [{status}] {cmd.name}: {reason}")
            report.steps.append({
                "index": i,
                "name": cmd.name,
                "pass": ok,
                "reason": reason,
                "send_bytes": result.send_bytes,
                "payload_bytes": result.payload_bytes,
                "sim_time": result.sim_time,
                "platform_count": result.platform_count,
                "agent_found": result.agent_found,
                "summary": result.summary,
            })

            if args.post_delay > 0:
                time.sleep(args.post_delay)

        log(f"\n{'=' * 60}")
        log(f"闭环汇总: {report.passed}/{report.total} PASS, {report.failed} FAIL")
        log(f"  recv 累计={client._recv_count}")
        CLOSED_LOOP_REPORT_PATH.write_text(
            json.dumps(report.to_dict(), ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        log(f"  报告已写入 {CLOSED_LOOP_REPORT_PATH}")
        return exit_code

    except KeyboardInterrupt:
        log("\n用户中断")
        return 130
    except Exception as exc:
        log(f"FAIL: {exc}")
        return 1
    finally:
        if client is not None:
            client.close()


def run_walkthrough(args: argparse.Namespace) -> int:
    LOG_PATH.write_text("", encoding="utf-8")
    full = build_catalog_for_args(args)
    try:
        catalog = apply_catalog_filters(args, full)
    except ValueError as exc:
        log(f"FAIL: {exc}")
        return 1
    if not catalog:
        log("FAIL: 过滤后无指令可执行")
        return 1

    log("Warlock 常规控制指令 — 手动逐步测试")
    log(f"  mode={args.mode}  wire={getattr(args, 'wire', 'zmq')}  agent={args.agent!r}  weapon={args.weapon!r}  track={args.track!r}")
    log(f"  共 {len(catalog)} 条  Enter=发送 / s=跳过 / q=退出  log={LOG_PATH}")
    if args.at and catalog[0].name != "E_SetAgentOutsideControl":
        log("  提示: 未选 E_SetAgentOutsideControl，请先手动执行 --at 1 或确保已外部控制")

    if args.dry_run:
        for i, step in enumerate(catalog, 1):
            data, err = serialize_command_step(
                step, agent=args.agent, weapon=args.weapon, track=args.track,
                platform_index=1 if step.uses_platform_index else None,
            )
            note = f" ({err})" if err else ""
            log(f"  [{i}/{len(catalog)}] {step.name} → {len(data)} bytes{note}")
        log("dry-run 完成 (未连接 Warlock)")
        return 0

    sender: Optional[object] = None
    try:
        if args.mode == "tcp":
            if not port_is_listening(args.host, args.port):
                log(f"预检: {args.host}:{args.port} 当前未监听")
                if args.fallback_ark_service:
                    log("  → 自动切换 --mode ark_service --auto-start")
                    args.mode = "ark_service"
                    args.auto_start = True
            if args.mode == "tcp":
                if args.wire == "zmq":
                    sender = ZmqStepClient(
                        args.host, args.port, args.connect_timeout, args.agent,
                        step_timeout=getattr(args, "step_timeout", 30.0),
                    )
                else:
                    sender = FramedTcpStepClient(args.host, args.port, args.connect_timeout, args.agent)
        if args.mode == "ark_service":
            if args.auto_start:
                sender = ArkServiceAutoStarter(args.service, args.scenario, args.sim_steps)
            else:
                if not args.uuid:
                    log("FAIL: --mode ark_service 需要 --uuid 或 --auto-start")
                    return 1
                sender = ArkServiceSender(args.service, args.uuid)

        interactive = args.delay <= 0 and not args.no_prompt
        total = len(catalog)
        sent = 0
        for i, step in enumerate(catalog, 1):
            action = prompt_user(i, total, step, interactive, args.delay)
            if action == "quit":
                log(f"\n已退出 (已发送 {sent}/{total})")
                return 0
            if action == "skip":
                log(f"  >> 跳过 {step.name}")
                continue
            plat_idx = args.platform_index if args.platform_index >= 0 else None
            if step.uses_platform_index and plat_idx is None:
                plat_idx = resolve_platform_index(
                    sender if isinstance(sender, (ZmqStepClient, FramedTcpStepClient)) else None,
                    args.agent,
                )
            proto_bytes, build_err = serialize_command_step(
                step,
                agent=args.agent,
                weapon=args.weapon,
                track=args.track,
                platform_index=plat_idx,
            )
            if build_err:
                log(f"  FAIL: {build_err}")
                continue
            if step.uses_platform_index and plat_idx is not None:
                log(f"  >> {step.name} platform index={plat_idx}")
            log(f"  >> send {step.name} ({len(proto_bytes)} bytes)")
            if isinstance(sender, (ZmqStepClient, FramedTcpStepClient)):
                result = sender.step(proto_bytes, label=step.name)
                if result.recv_ok and result.summary:
                    log(f"  << {result.summary}")
            else:
                sender.send_proto(proto_bytes)
            sent += 1
            if args.post_delay > 0:
                time.sleep(args.post_delay)

        log(f"\n已发送 {sent}/{total} 条。recv 累计={getattr(sender, '_recv_count', '?')}")
        log("请在 Warlock 中确认各指令效果。")
        return 0

    except KeyboardInterrupt:
        log("\n用户中断")
        return 130
    except Exception as exc:
        log(f"FAIL: {exc}")
        return 1
    finally:
        if sender is not None:
            sender.close()


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Warlock 手动观察 — 逐步发送常规 E_Actions 控制指令",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
示例 (只测开火 — 推荐):
  python warlock_command_walkthrough.py --fire-only

  # Warlock 18000 手动步进 (Enter 发送):
  python warlock_command_walkthrough.py --fire-only

  # 18000 不可用 → ArkService 自动拉起:
  python warlock_command_walkthrough.py --fire-only --fallback-ark-service --no-prompt

  # 全自动开火联测 (60004 + evt 检查):
  python protobuf/test_fire_at_target.py
        """,
    )
    parser.add_argument("--mode", choices=("tcp", "ark_service"), default="tcp",
                        help="tcp=18000 直连仿真; ark_service=60004 proto")
    parser.add_argument("--wire", choices=("zmq", "framed"), default="zmq",
                        help="18000 线协议: zmq=PAIR裸protobuf(默认); framed=LE帧+握手")
    parser.add_argument("--host", default="127.0.0.1", help="Warlock TCP 主机 (mode=tcp)")
    parser.add_argument("--port", type=int, default=DEFAULT_TCP_PORT, help="Warlock TCP 端口 (mode=tcp)")
    parser.add_argument("--service", default=DEFAULT_ARK_SERVICE, help="ArkService 地址")
    parser.add_argument("--scenario", default=DEFAULT_SCENARIO, help="--auto-start 使用的想定路径")
    parser.add_argument("--uuid", default="", help="ArkService 仿真实例 uuid")
    parser.add_argument("--auto-start", action="store_true",
                        help="ark_service 模式: 自动 start/resume/runstep 后发 proto")
    parser.add_argument("--fallback-ark-service", action="store_true",
                        help="tcp 预检失败时自动改用 ark_service --auto-start")
    parser.add_argument("--sim-steps", type=int, default=50, help="auto-start 后 runstep 步数")
    parser.add_argument("--agent", default=DEFAULT_AGENT, help="控制平台名")
    parser.add_argument("--weapon", default=DEFAULT_WEAPON,
                        help="武器 Component_id (loiter_wave1/2/3, gun_30mm)")
    parser.add_argument("--sensor", default=DEFAULT_SENSOR,
                        help="传感器 Component_id (surf_radar, eoir)")
    parser.add_argument("--comm", default=DEFAULT_COMM,
                        help="通信 Component_id (usv_cmd_radio, usv_loiter_radio)")
    parser.add_argument("--sensor-mode", default=DEFAULT_SENSOR_MODE,
                        help="传感器模式 (想定 surf_radar 为 SEARCH)")
    parser.add_argument("--msg-target", default=BLUE_PATROL_1,
                        help="SendMsgToPlatform 目标平台名")
    parser.add_argument("--track", default=DEFAULT_TRACK,
                        help="目标 trackId (API 格式 self:1，非 evt 的 self.1)")
    parser.add_argument("--platform-index", type=int, default=-1,
                        help="E_AuxActions 的 PlatformAuxData.index (-1=从上一条 StateMessage 解析)")
    parser.add_argument("--only", default="", help="逗号分隔，仅执行这些指令名")
    parser.add_argument(
        "--fire-only", action="store_true",
        help="仅测开火 (等价 --only E_SetAgentOutsideControl,E_FireAtTarget)",
    )
    parser.add_argument("--skip", default="", help="逗号分隔，跳过这些指令名")
    parser.add_argument("--list", action="store_true", help="列出全部指令编号后退出")
    parser.add_argument("--at", type=int, default=0, metavar="N",
                        help="仅执行第 N 条指令 (1-based，见 --list)")
    parser.add_argument("--include-risky", action="store_true",
                        help="包含高风险指令 (平台增删/齐射/干扰/无效航线等)")
    parser.add_argument("--skip-risky", action="store_true",
                        help="手动模式: 排除高风险指令 (闭环默认等同开启)")
    parser.add_argument("--delay", type=float, default=0.0,
                        help="每条指令发送前等待秒数 (>0 则非交互自动模式)")
    parser.add_argument("--post-delay", type=float, default=0.5,
                        help="每条指令发送后等待秒数")
    parser.add_argument("--no-prompt", action="store_true", help="不等待 Enter，立即连续发送")
    parser.add_argument("--connect-timeout", type=float, default=120.0,
                        help="等待 Warlock TCP 端口就绪的最长秒数")
    parser.add_argument("--step-timeout", type=float, default=20.0,
                        help="闭环/步进模式每条指令 recv StateMessage 超时秒数")
    parser.add_argument("--dry-run", action="store_true", help="仅列出指令与字节长度")
    parser.add_argument("--closed-loop", action="store_true",
                        help="闭环测试 send→recv (默认手动步进；勿对 Warlock 用 --no-prompt)")
    parser.add_argument("--allow-empty", action=argparse.BooleanOptionalAction, default=True,
                        help="闭环模式: 允许 0 字节 ActionsFromOutside 步进 (默认开启)")
    parser.add_argument("--empty-pulse", action="store_true",
                        help="闭环模式: 每条指令前先发送空命令步进 (PAIR 同步)")
    parser.add_argument("--empty-sync", action="store_true",
                        help="闭环模式: 开始前先发一次空命令")
    parser.add_argument("--require-time-advance", action="store_true",
                        help="闭环模式: 要求每条指令后 sim_time 不回退")
    parser.add_argument("--require-agent-in-state", action="store_true",
                        help="闭环模式: 要求回包态势含 agent 平台")
    args = parser.parse_args()
    if args.fire_only:
        if args.only:
            parser.error("--fire-only 与 --only 不能同时使用")
        args.only = "E_SetAgentOutsideControl,E_FireAtTarget"
    if args.list:
        log_component_summary(log)
        catalog = build_catalog_for_args(args)
        print_catalog_list(catalog)
        return 0
    if args.closed_loop:
        if not args.include_risky and not args.skip_risky:
            args.skip_risky = True
        return run_closed_loop(args)
    return run_walkthrough(args)


if __name__ == "__main__":
    sys.exit(main())
