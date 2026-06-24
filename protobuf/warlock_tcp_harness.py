"""Warlock ZMQ 18000 自动化测试公共 harness (PAIR send→recv + 空步进观察)。"""
from __future__ import annotations

import math
import socket
import time
from pathlib import Path
from typing import Optional

from arkcmd.proto.proto_utils import ProtoStringBuilder, normalize_track_id

from warlock_command_walkthrough import (
    EMPTY_ACTIONS,
    StepResult,
    ZmqStepClient,
    log,
    parse_state_payload,
    pick_track_for_agent,
    resolve_platform_index,
    serialize_command_step,
)

ROOT = Path(__file__).resolve().parent


def port_open(host: str, port: int, timeout: float = 1.0) -> bool:
    try:
        s = socket.create_connection((host, port), timeout=timeout)
        s.close()
        return True
    except OSError:
        return False


def wait_for_port(host: str, port: int, wait_sec: float) -> bool:
    log(f"等待 tcp://{host}:{port} 就绪 (最长 {wait_sec:.0f}s)...")
    log("  → Warlock: 加载 usv_loiter_strike.txt 并 Play (建议 Pause 后跑脚本)")
    deadline = time.time() + wait_sec
    attempt = 0
    while time.time() < deadline:
        attempt += 1
        if port_open(host, port):
            log(f"  端口已就绪 (第 {attempt} 次探测)")
            return True
        if attempt == 1 or attempt % 10 == 0:
            log(f"  ... 尚未监听 ({attempt})")
        time.sleep(1.0)
    return False


def require_step(client: ZmqStepClient, data: bytes, label: str) -> StepResult:
    result = client.step(data, label=label)
    if not result.recv_ok:
        raise RuntimeError(f"{label}: recv 超时 — 检查 18000 是否被其它 PAIR 客户端占用")
    return result


def log_step_result(result: StepResult, note: str = "") -> None:
    prefix = f"  << {note}: " if note else "  << "
    log(f"{prefix}{result.summary or result.error or 'no summary'}")


def observe_sim(
    client: ZmqStepClient,
    n: int,
    prefix: str,
    step_delay: float,
) -> None:
    if n <= 0:
        return
    for i in range(n):
        require_step(client, EMPTY_ACTIONS, label=f"{prefix}_observe_{i + 1}/{n}")
        if step_delay > 0:
            time.sleep(step_delay)


def connect_client(
    host: str,
    port: int,
    agent: str,
    *,
    connect_timeout: float = 10.0,
    step_timeout: float = 60.0,
) -> ZmqStepClient:
    return ZmqStepClient(
        host, port,
        connect_timeout=connect_timeout,
        agent=agent,
        step_timeout=step_timeout,
    )


def ensure_outside_control(client: ZmqStepClient, agent: str) -> StepResult:
    b = ProtoStringBuilder()
    b.set_agent_outside_control(agent)
    data = b.serialize_actions()
    log(f"  >> E_SetAgentOutsideControl ({len(data)} bytes)")
    result = require_step(client, data, label="E_SetAgentOutsideControl")
    log_step_result(result, "outside control")
    return result


def build_velocity(agent: str, speed_ms: float) -> bytes:
    b = ProtoStringBuilder()
    b.set_desired_velocity(agent, speed_ms, 0.5)
    return b.serialize_actions()


def build_heading(agent: str, heading_deg: float, speed_ms: float) -> bytes:
    b = ProtoStringBuilder()
    b.set_desired_heading(agent, math.radians(heading_deg), speed_ms, 1)
    return b.serialize_actions()


def resolve_track(
    client: ZmqStepClient,
    agent: str,
    fallback: str,
    warmup_max: int,
    step_delay: float,
) -> str:
    track, source = pick_track_for_agent(client._last_payload, agent, fallback=fallback)
    if "fallback(agent" not in source or track != normalize_track_id(fallback):
        log(f"  目标 track={track!r}  ← {source}")
        return track

    log(f"  态势暂无有效航迹，最多 {warmup_max} 次步进等待 sensor track ...")
    for i in range(warmup_max):
        require_step(client, build_velocity(agent, 12.0), label=f"track_warmup_{i + 1}")
        track, source = pick_track_for_agent(client._last_payload, agent, fallback=fallback)
        log(f"  track_warmup #{i + 1}: track={track!r} ({source})")
        if "fallback(agent" not in source:
            return track
        if step_delay > 0:
            time.sleep(step_delay)

    track, source = pick_track_for_agent(client._last_payload, agent, fallback=fallback)
    log(f"  仍用缺省 track={track!r} ({source})")
    return track


def run_motion_phase(
    client: ZmqStepClient,
    agent: str,
    *,
    phase: str,
    speed_ms: float,
    heading_deg: float,
    heading_speed_ms: float,
    observe_steps: int,
    step_delay: float,
) -> None:
    log(f"\n--- {phase}: 机动 (速度 {speed_ms} m/s → 航向 {heading_deg}°) ---")
    vel = build_velocity(agent, speed_ms)
    log(f"  >> E_SetDesiredVelocity ({len(vel)}B) speed={speed_ms} m/s")
    log_step_result(require_step(client, vel, label=f"{phase}_SetDesiredVelocity"), "velocity")
    if observe_steps > 0:
        log(f"  ... 空步进 x{observe_steps} 观察加速")
        observe_sim(client, observe_steps, f"{phase}_after_vel", step_delay)

    hdg = build_heading(agent, heading_deg, heading_speed_ms)
    log(f"  >> E_SetDesiredHeading ({len(hdg)}B) heading={heading_deg}°")
    log_step_result(require_step(client, hdg, label=f"{phase}_SetDesiredHeading"), "heading")
    if observe_steps > 0:
        log(f"  ... 空步进 x{observe_steps} 观察转向")
        observe_sim(client, observe_steps, f"{phase}_after_hdg", step_delay)


def final_sim_time(client: ZmqStepClient, agent: str) -> Optional[float]:
    if not client._last_payload:
        return None
    sim_t, _, _, _ = parse_state_payload(client._last_payload, agent)
    return sim_t
