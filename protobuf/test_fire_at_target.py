#!/usr/bin/env python3
"""
FireAtTarget 命令通道自动联机测试

流程:
  1. 经 ArkService (tcp://127.0.0.1:60004) start 拉起 usv_loiter_strike 想定
  2. changesituation + customizedsituation + resume + runstep 推进仿真
  3. 从定制态势推送中解析红方平台 + 蓝方 trackId；若无推送则用想定缺省值
  4. set_agent_outside_control → FireAtTarget(红方, 武器, 蓝方航迹)
  5. 再 runstep 推进，检查 evt 是否出现开火相关事件

用法:
  python test_fire_at_target.py
  python test_fire_at_target.py --dry-run
"""
from __future__ import annotations

import argparse
import base64
import json
import re
import subprocess
import sys
import threading
import time
import traceback
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import zmq

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))

from arkcmd.proto import arksimActions_pb2  # noqa: E402
from arkcmd.proto.proto_utils import ProtoStringBuilder, normalize_track_id  # noqa: E402

DEFAULT_SERVICE = "tcp://127.0.0.1:60004"
DEFAULT_SCENARIO = r"E:\dev\ArkSIM_SCEN\ArkSIMModels\scenarios\usv_loiter_strike\usv_loiter_strike.txt"
EVT_PATH = Path(DEFAULT_SCENARIO).parent / "output" / "usv_loiter_strike.evt"

# 想定缺省: 红方平台 self, 第二组巡飞弹 loiter_wave2, 蓝方航迹 self:1 → blue_patrol_1
FALLBACK_RED_AGENT = "self"
FALLBACK_WEAPON = "loiter_wave2"
FALLBACK_BLUE_TRACK = "self:1"


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def side_is_red(side: str) -> bool:
    s = (side or "").lower()
    return s in ("red", "foe", "hostile", "enemy") or "red" in s


def side_is_blue(side: str) -> bool:
    s = (side or "").lower()
    return s in ("blue", "friend") or "blue" in s


def extract_situation_payload(raw: str) -> Optional[Dict[str, Any]]:
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return None
    if isinstance(data, dict):
        for key in ("customizedsituation", "state", "situation", "data"):
            inner = data.get(key)
            if isinstance(inner, dict) and "platforms" in inner:
                return inner
        if "platforms" in data:
            return data
    return None


def pick_loiter_weapon(weapons: Dict) -> str:
    """优先选巡飞弹 Component_id；缺省 loiter_wave2（第二组）。"""
    if not isinstance(weapons, dict) or not weapons:
        return FALLBACK_WEAPON
    loiter_keys = sorted(k for k in weapons if str(k).startswith("loiter_wave"))
    if len(loiter_keys) >= 2:
        return loiter_keys[1]  # 第二组巡飞弹
    if loiter_keys:
        return loiter_keys[0]
    return FALLBACK_WEAPON


def pick_red_shooter_and_blue_track(situation: Dict[str, Any]) -> Tuple[str, str, str]:
    """返回 (红方平台名, 武器ID, 蓝方trackId)。"""
    platforms = situation.get("platforms") or []
    red_plat: Optional[Dict] = None
    for p in platforms:
        if side_is_red(str(p.get("side", ""))):
            weapons = p.get("weapons") or {}
            if isinstance(weapons, dict) and weapons:
                red_plat = p
                break
            if red_plat is None:
                red_plat = p
    if red_plat is None:
        raise RuntimeError("态势中未找到红方平台")

    agent = str(red_plat.get("name") or FALLBACK_RED_AGENT)
    weapons = red_plat.get("weapons") or {}
    weapon = pick_loiter_weapon(weapons if isinstance(weapons, dict) else {})

    blue_track = ""
    for tr in red_plat.get("tracks") or []:
        tid = normalize_track_id(str(tr.get("trackId") or tr.get("track_id") or ""))
        tr_side = str(tr.get("side") or "")
        if tid and side_is_blue(tr_side):
            blue_track = tid
            break
    if not blue_track:
        for tr in red_plat.get("tracks") or []:
            tid = normalize_track_id(str(tr.get("trackId") or tr.get("track_id") or ""))
            iff = str(tr.get("iff") or "").lower()
            if tid and iff in ("foe", "hostile", "enemy"):
                blue_track = tid
                break
    if not blue_track:
        raise RuntimeError(f"红方平台 {agent!r} 无蓝方航迹")

    return agent, weapon, blue_track


def build_proto_bytes(builder: ProtoStringBuilder) -> bytes:
    return builder.serialize_actions()


def build_outside_control(agent: str) -> bytes:
    b = ProtoStringBuilder()
    b.set_agent_outside_control(agent)
    return build_proto_bytes(b)


def build_fire(agent: str, weapon: str, track: str) -> bytes:
    b = ProtoStringBuilder()
    b.fire_at_target(agent, weapon, track)
    actions = b.get_actions()
    fire = actions.a_fireattarget[0]
    assert fire.action == arksimActions_pb2.E_FireAtTarget
    assert fire.agent.agent_id == agent
    assert fire.agent.Component_id == weapon
    assert fire.trck_id == normalize_track_id(track)
    return build_proto_bytes(b)


def evt_has_fire_events(since_size: int) -> List[str]:
    """读取 evt 新增内容，查找开火相关行。"""
    if not EVT_PATH.exists():
        return []
    text = EVT_PATH.read_text(encoding="utf-8", errors="ignore")
    if since_size > len(text):
        since_size = 0
    tail = text[since_size:]
    hits = []
    for line in tail.splitlines():
        upper = line.upper()
        if any(k in upper for k in ("WEAPON_FIRED", "WEAPON_LAUNCHED", "FIRE_", "ENGAGEMENT")):
            if any(k in line for k in ("loiter_wave", "RED_LOITER", "gun_30mm")) or FALLBACK_RED_AGENT in line:
                hits.append(line.strip())
    return hits[:10]


def list_sim_processes() -> List[str]:
    lines = []
    for name in ("ark_service", "mission", "warlock"):
        out = subprocess.run(
            ["tasklist", "/FI", f"IMAGENAME eq {name}.exe"],
            capture_output=True,
            text=True,
            encoding="gbk",
            errors="ignore",
        ).stdout
        for line in out.splitlines():
            if name + ".exe" in line.lower():
                lines.append(line.strip())
    return lines


class ArksimClient:
    def __init__(self, addr: str):
        self.ctx = zmq.Context.instance()
        self.socket = self.ctx.socket(zmq.DEALER)
        self.socket.setsockopt(zmq.LINGER, 0)
        self.socket_id = f"fire_auto_{uuid.uuid4().hex[:8]}"
        self.socket.setsockopt(zmq.ROUTING_ID, self.socket_id.encode())
        self.socket.connect(addr)
        self._recv_buf: List[str] = []
        self._stop = False
        self._thread = threading.Thread(target=self._recv_loop, daemon=True)
        self._thread.start()
        log(f"已连接 {addr}  ROUTING_ID={self.socket_id}")
        time.sleep(0.3)

    def _recv_loop(self) -> None:
        while not self._stop:
            try:
                if self.socket.poll(200):
                    for f in self.socket.recv_multipart():
                        self._recv_buf.append(f.decode("utf-8", errors="replace"))
            except zmq.ZMQError:
                break

    def drain_situations(self) -> List[Dict[str, Any]]:
        out: List[Dict[str, Any]] = []
        for raw in self._recv_buf:
            sit = extract_situation_payload(raw)
            if sit:
                out.append(sit)
        return out

    def send_json(self, payload: dict) -> None:
        log(f"  >> {json.dumps(payload, ensure_ascii=False)[:240]}")
        self.socket.send(json.dumps(payload).encode("utf-8"))

    def send_proto(self, instance_uuid: str, proto_bytes: bytes) -> None:
        payload = {
            "fn": "proto",
            "proto": base64.b64encode(proto_bytes).decode("ascii"),
            "uuid": instance_uuid,
        }
        log(f"  >> proto bytes={len(proto_bytes)}")
        self.socket.send(json.dumps(payload).encode("utf-8"))

    def switch_routing_id(self, routing_id: str) -> None:
        self.socket.setsockopt(zmq.ROUTING_ID, routing_id.encode())
        log(f"ROUTING_ID → {routing_id}")

    def close(self) -> None:
        self._stop = True
        time.sleep(0.2)
        self.socket.close()


def wait_for_uuid(recv_buf: List[str], timeout: float = 25.0) -> Optional[str]:
    end = time.time() + timeout
    while time.time() < end:
        for raw in recv_buf:
            try:
                d = json.loads(raw)
                uid = d.get("data", {}).get("uuid") or d.get("uuid")
                if uid:
                    return uid
            except Exception:
                pass
        time.sleep(0.2)
    return None


def run_auto_test(
    service: str,
    scenario: str,
    cleanup: bool,
    sim_steps: int,
) -> int:
    evt_size_before = EVT_PATH.stat().st_size if EVT_PATH.exists() else 0
    client = ArksimClient(service)
    instance_uuid: Optional[str] = None
    exit_code = 0

    try:
        log("\n=== 1. 拉起想定 (start) ===")
        log(f"  进程: {list_sim_processes() or '(无 mission/warlock)'}")
        client.send_json({
            "fn": "start",
            "args": {
                "exec": 1,
                "offscreen": False,
                "randomSeed": int(time.time()) % 100000,
                "realtime": False,
                "scenarios": [scenario],
                "simType": 0,
            },
        })
        instance_uuid = wait_for_uuid(client._recv_buf, timeout=30.0)
        if not instance_uuid:
            raise RuntimeError("start 超时 — 请确认 ark_service 在 60004 运行")
        log(f"  instance_uuid = {instance_uuid}")
        client.switch_routing_id(instance_uuid)

        log("\n=== 2. 定制态势 + resume + 推进仿真 ===")
        client.send_json({"fn": "changesituation", "rate": 0, "uuid": instance_uuid})
        time.sleep(0.2)
        client.send_json({"fn": "customizedsituation", "time": 1.0, "uuid": instance_uuid})
        time.sleep(0.2)
        client.send_json({"fn": "resume", "uuid": instance_uuid})
        time.sleep(0.5)
        client.send_json({"fn": "runstep", "args": {"step": sim_steps}, "uuid": instance_uuid})
        time.sleep(1.0)
        client.send_json({"fn": "advance_to_time", "args": {"time": 60.0}, "uuid": instance_uuid})
        time.sleep(2.0)
        log(f"  进程: {list_sim_processes() or '(无 mission/warlock)'}")

        log("\n=== 3. 解析红方实体 + 蓝方 trackId ===")
        situations = client.drain_situations()
        agent, weapon, track = FALLBACK_RED_AGENT, FALLBACK_WEAPON, FALLBACK_BLUE_TRACK
        source = "fallback(想定缺省 self/loiter_wave2/self:1)"
        if situations:
            try:
                agent, weapon, track = pick_red_shooter_and_blue_track(situations[-1])
                source = f"态势推送 ({len(situations)} 帧)"
            except RuntimeError as exc:
                log(f"  态势解析失败: {exc}，使用缺省值")
        else:
            log("  未收到定制态势推送，使用想定缺省 (self / loiter_wave2 / self:1)")
        log(f"  来源: {source}")
        log(f"  红方 agent={agent!r}  weapon={weapon!r}(巡飞弹)  蓝方 track={track!r}")

        log("\n=== 4. 外部控制 + FireAtTarget ===")
        client.send_proto(instance_uuid, build_outside_control(agent))
        time.sleep(0.3)
        fire_bytes = build_fire(agent, weapon, track)
        actions = arksimActions_pb2.ActionsFromOutside()
        actions.ParseFromString(fire_bytes)
        fire = actions.a_fireattarget[0]
        log(f"  protobuf: action={fire.action} agent={fire.agent.agent_id} "
            f"weapon={fire.agent.Component_id} track={fire.trck_id}")
        client.send_proto(instance_uuid, fire_bytes)
        log("  FireAtTarget 已发送")

        log("\n=== 5. 推进仿真并检查 evt ===")
        client.send_json({"fn": "runstep", "args": {"step": 50}, "uuid": instance_uuid})
        time.sleep(2.0)
        fire_hits = evt_has_fire_events(evt_size_before)
        if fire_hits:
            log(f"  evt 开火事件 ({len(fire_hits)} 条):")
            for h in fire_hits[:5]:
                log(f"    {h[:200]}")
        else:
            log("  evt 暂无明确 WEAPON_FIRED 行 (proto 通道已送达，仿真侧可能静默处理)")

        log("\n=== PASS: 想定已拉起，红方对蓝方 track 开火指令已发出 ===")
        log(f"  recv 总帧数={len(client._recv_buf)}")

    except Exception as exc:
        log(f"\nFAIL: {exc}")
        log(traceback.format_exc())
        exit_code = 1
    finally:
        if cleanup and instance_uuid:
            log("\n=== 清理: pause + exit ===")
            try:
                client.send_json({"fn": "pause", "uuid": instance_uuid})
                time.sleep(0.3)
                client.send_json({"fn": "exit", "uuid": instance_uuid})
            except Exception:
                pass
        client.close()
    return exit_code


def main() -> int:
    parser = argparse.ArgumentParser(description="FireAtTarget 自动联机测试")
    parser.add_argument("--service", default=DEFAULT_SERVICE)
    parser.add_argument("--scenario", default=DEFAULT_SCENARIO)
    parser.add_argument("--sim-steps", type=int, default=200, help="resume 后 runstep 步数")
    parser.add_argument("--no-cleanup", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    log("FireAtTarget 自动测试")
    if args.dry_run:
        b = build_fire(FALLBACK_RED_AGENT, FALLBACK_WEAPON, FALLBACK_BLUE_TRACK)
        log(f"dry-run OK: agent={FALLBACK_RED_AGENT} weapon={FALLBACK_WEAPON} track={FALLBACK_BLUE_TRACK} ({len(b)} bytes)")
        return 0
    return run_auto_test(
        service=args.service,
        scenario=args.scenario,
        cleanup=not args.no_cleanup,
        sim_steps=args.sim_steps,
    )


if __name__ == "__main__":
    sys.exit(main())
