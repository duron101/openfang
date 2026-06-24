#!/usr/bin/env python3
"""
Warlock ZMQ 18000 — 全控制指令自动化观察测试

与 test_warlock_tcp_fire.py 同一思路:
  SetAgentOutsideControl → 发单条 E_Action → 空步进观察 → 日志快照

分组 profile (默认 safe，跳过 Warlock 高风险指令):
  motion  — 速度/高度/航向/航渡
  sensor  — 传感器开关/模式/查询
  comm    — 通信开关/平台消息
  weapon  — 开火/齐射/更新目标 (自动解析 track)
  aux     — AUX 透传 (需 StateMessage.index)
  safe    — motion + sensor + comm + weapon (推荐)
  all     — 含 risky，需 --include-risky

用法:
  python test_warlock_tcp_controls.py --list
  python test_warlock_tcp_controls.py --profile safe
  python test_warlock_tcp_controls.py --profile motion --observe-steps 8
  python test_warlock_tcp_controls.py --only E_SetDesiredVelocity,E_GoToLocation
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional, Set

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))

from arkcmd.proto.proto_utils import normalize_track_id  # noqa: E402
from usv_self_components import (  # noqa: E402
    BLUE_PATROL_1,
    DEFAULT_COMM,
    DEFAULT_SENSOR,
    DEFAULT_SENSOR_MODE,
    log_component_summary,
)
from warlock_command_walkthrough import (  # noqa: E402
    CLOSED_LOOP_DEFER,
    CommandStep,
    DEFAULT_AGENT,
    DEFAULT_TCP_PORT,
    DEFAULT_TRACK,
    DEFAULT_WEAPON,
    EMPTY_ACTIONS,
    WARLOCK_RISKY,
    ZmqStepClient,
    build_catalog_for_args,
    log,
    order_catalog_for_closed_loop,
    resolve_platform_index,
    serialize_command_step,
)
from warlock_tcp_harness import (  # noqa: E402
    connect_client,
    ensure_outside_control,
    log_step_result,
    observe_sim,
    require_step,
    resolve_track,
    wait_for_port,
)

LOG_PATH = ROOT / "test_warlock_tcp_controls.log"
REPORT_PATH = ROOT / "test_warlock_tcp_controls_report.json"

SKIP_IN_AUTO = frozenset({"E_SetAgentOutsideControl"})

NEEDS_TRACK = frozenset({
    "E_FireAtTarget",
    "E_FireSlavoAtTarget",
    "E_StartJamming",
})

PROFILE_COMMANDS: Dict[str, List[str]] = {
    "motion": [
        "E_SetDesiredVelocity",
        "E_SetDesiredAltitude",
        "E_SetDesiredHeading",
        "E_GoToLocation",
    ],
    "sensor": [
        "E_TurnOnSensor",
        "E_TurnOffSensor",
        "E_ChangeSensorMode",
        "E_GetSensorCurrentMode",
    ],
    "comm": [
        "E_TurnOnComm",
        "E_TurnOffComm",
        "E_SendMsgToPlatform",
    ],
    "weapon": [
        "E_FireAtTarget",
        "E_FireSlavoAtTarget",
        "E_UpdateTarget",
    ],
    "aux": ["E_AuxActions"],
    "defer": list(CLOSED_LOOP_DEFER),
}

OBSERVE_STEPS: Dict[str, int] = {
    "E_SetDesiredVelocity": 8,
    "E_SetDesiredAltitude": 6,
    "E_SetDesiredHeading": 8,
    "E_GoToLocation": 10,
    "E_FollowRoute": 8,
    "E_AuxActions": 4,
    "E_TurnOnSensor": 4,
    "E_TurnOffSensor": 3,
    "E_ChangeSensorMode": 4,
    "E_GetSensorCurrentMode": 2,
    "E_FireAtTarget": 5,
    "E_FireSlavoAtTarget": 6,
    "E_UpdateTarget": 3,
    "E_StartJamming": 4,
    "E_StopJamming": 3,
    "E_ChangeJammingMode": 4,
    "E_TurnOnComm": 3,
    "E_TurnOffComm": 3,
    "E_SendMsgToPlatform": 2,
    "E_SendMsgToCommandChain": 2,
    "E_ChangePlatformNumber_add": 6,
    "E_ChangePlatformNumber_del": 4,
    "E_ChangeCommander": 3,
    "E_ReleaseOutsideControl": 2,
}
DEFAULT_OBSERVE = 4


@dataclass
class StepReport:
    name: str
    description: str
    pass_: bool
    reason: str
    send_bytes: int = 0
    sim_time: Optional[float] = None
    summary: str = ""
    observe_steps: int = 0
    risky: bool = False


@dataclass
class ControlsReport:
    profile: str
    agent: str
    weapon: str
    track: str
    total: int = 0
    passed: int = 0
    failed: int = 0
    skipped: int = 0
    steps: List[StepReport] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "profile": self.profile,
            "agent": self.agent,
            "weapon": self.weapon,
            "track": self.track,
            "total": self.total,
            "passed": self.passed,
            "failed": self.failed,
            "skipped": self.skipped,
            "steps": [
                {
                    "name": s.name,
                    "description": s.description,
                    "pass": s.pass_,
                    "reason": s.reason,
                    "send_bytes": s.send_bytes,
                    "sim_time": s.sim_time,
                    "summary": s.summary,
                    "observe_steps": s.observe_steps,
                    "risky": s.risky,
                }
                for s in self.steps
            ],
        }


def safe_profile_commands() -> List[str]:
    names: List[str] = []
    for group in ("motion", "sensor", "comm", "weapon"):
        names.extend(PROFILE_COMMANDS[group])
    names.extend(PROFILE_COMMANDS["defer"])
    return names


def resolve_profile_names(args: argparse.Namespace) -> List[str]:
    if args.only:
        return [n.strip() for n in args.only.split(",") if n.strip()]
    if args.profile == "safe":
        return safe_profile_commands()
    if args.profile == "all":
        catalog = build_catalog_for_args(args)
        names = [c.name for c in catalog if c.name not in SKIP_IN_AUTO]
        if not args.include_risky:
            names = [n for n in names if n not in WARLOCK_RISKY]
        ordered = order_catalog_for_closed_loop([c for c in catalog if c.name in names])
        return [c.name for c in ordered]
    if args.profile not in PROFILE_COMMANDS:
        raise ValueError(f"未知 profile: {args.profile}")
    names = list(PROFILE_COMMANDS[args.profile])
    if args.profile != "defer" and args.include_release:
        for n in PROFILE_COMMANDS["defer"]:
            if n not in names:
                names.append(n)
    return names


def catalog_by_name(args: argparse.Namespace) -> Dict[str, CommandStep]:
    return {c.name: c for c in build_catalog_for_args(args)}


def observe_count(step_name: str, default: int) -> int:
    return OBSERVE_STEPS.get(step_name, default)


def print_profiles() -> None:
    log("可用 profile:")
    for key, cmds in PROFILE_COMMANDS.items():
        log(f"  {key:8s} ({len(cmds)}): {', '.join(cmds)}")
    log("  safe     — motion + sensor + comm + weapon + defer")
    log("  all      — 全指令 (默认跳过 risky，--include-risky 启用)")


def run_controls(args: argparse.Namespace) -> int:
    LOG_PATH.write_text("", encoding="utf-8")
    args.track = normalize_track_id(args.track)

    try:
        names = resolve_profile_names(args)
    except ValueError as exc:
        log(f"FAIL: {exc}")
        return 1

    if args.skip:
        skip = {n.strip() for n in args.skip.split(",") if n.strip()}
        names = [n for n in names if n not in skip]

    catalog = catalog_by_name(args)
    steps_to_run: List[CommandStep] = []
    for name in names:
        if name not in catalog:
            log(f"WARN: 未知指令 {name!r}，跳过")
            continue
        if name in SKIP_IN_AUTO:
            continue
        steps_to_run.append(catalog[name])

    if not steps_to_run:
        log("FAIL: 无指令可测")
        return 1

    report = ControlsReport(
        profile=args.profile,
        agent=args.agent,
        weapon=args.weapon,
        track=args.track,
    )
    report.total = len(steps_to_run)

    log("Warlock ZMQ 18000 控制指令自动化观察测试")
    log_component_summary(log)
    log(
        f"  profile={args.profile!r}  sensor={args.sensor!r}  comm={args.comm!r}  "
        f"weapon={args.weapon!r}  共 {len(steps_to_run)} 条"
    )

    if not wait_for_port(args.host, args.port, args.wait):
        log("FAIL: 18000 未监听")
        return 1

    client: Optional[ZmqStepClient] = None
    exit_code = 0
    resolved_track = args.track

    try:
        client = connect_client(
            args.host, args.port, args.agent,
            step_timeout=args.step_timeout,
        )

        if args.empty_sync:
            log("  >> 空步进同步")
            require_step(client, EMPTY_ACTIONS, label="empty_sync")

        ensure_outside_control(client, args.agent)

        weapon_block = any(s.name in NEEDS_TRACK for s in steps_to_run)
        if weapon_block:
            log("\n--- 武器组: 解析 track ---")
            resolved_track = resolve_track(
                client, args.agent, args.track, args.warmup_max, args.step_delay,
            )
            report.track = resolved_track

        for i, step in enumerate(steps_to_run, 1):
            log(f"\n{'=' * 60}")
            log(f"[{i}/{len(steps_to_run)}] {step.name} — {step.description}")
            if step.risky:
                log("  ⚠ RISKY 指令")

            n_observe = observe_count(step.name, args.observe_steps)
            plat_idx = None
            if step.uses_platform_index:
                plat_idx = resolve_platform_index(client, args.agent)
                if plat_idx is None:
                    msg = f"缺少 {args.agent!r} 的 platform index (Aux)"
                    log(f"  FAIL: {msg}")
                    report.failed += 1
                    exit_code = 1
                    report.steps.append(StepReport(
                        step.name, step.description, False, msg,
                        risky=step.risky, observe_steps=n_observe,
                    ))
                    continue

            proto_bytes, build_err = serialize_command_step(
                step,
                agent=args.agent,
                weapon=args.weapon,
                track=resolved_track,
                platform_index=plat_idx,
            )
            if build_err:
                log(f"  FAIL: {build_err}")
                report.failed += 1
                exit_code = 1
                report.steps.append(StepReport(
                    step.name, step.description, False, build_err,
                    risky=step.risky, observe_steps=n_observe,
                ))
                continue

            log(f"  >> send {step.name} ({len(proto_bytes)} bytes)")
            try:
                result = require_step(client, proto_bytes, label=step.name)
                log_step_result(result, step.name)
                if n_observe > 0:
                    log(f"  ... 空步进 x{n_observe} (~{n_observe * 3}s 仿真) 观察效果")
                    observe_sim(client, n_observe, step.name, args.step_delay)
                report.passed += 1
                report.steps.append(StepReport(
                    step.name, step.description, True, "OK",
                    send_bytes=len(proto_bytes),
                    sim_time=result.sim_time,
                    summary=result.summary or "",
                    observe_steps=n_observe,
                    risky=step.risky,
                ))
            except Exception as exc:
                log(f"  FAIL: {exc}")
                report.failed += 1
                exit_code = 1
                report.steps.append(StepReport(
                    step.name, step.description, False, str(exc),
                    send_bytes=len(proto_bytes),
                    observe_steps=n_observe,
                    risky=step.risky,
                ))

            if args.inter_command_delay > 0:
                time.sleep(args.inter_command_delay)

        log(f"\n{'=' * 60}")
        log(f"汇总: {report.passed}/{report.total} PASS, {report.failed} FAIL")
        if client._last_payload:
            from warlock_tcp_harness import final_sim_time
            sim_t = final_sim_time(client, args.agent)
            if sim_t is not None:
                log(f"  最终 sim_time≈{sim_t:.1f}s  recv={client._recv_count}")

        REPORT_PATH.write_text(
            json.dumps(report.to_dict(), ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        log(f"  报告: {REPORT_PATH}")
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


def main() -> int:
    p = argparse.ArgumentParser(description="Warlock 控制指令自动化观察测试")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=DEFAULT_TCP_PORT)
    p.add_argument("--agent", default=DEFAULT_AGENT)
    p.add_argument("--weapon", default=DEFAULT_WEAPON)
    p.add_argument("--sensor", default=DEFAULT_SENSOR)
    p.add_argument("--comm", default=DEFAULT_COMM)
    p.add_argument("--sensor-mode", default=DEFAULT_SENSOR_MODE)
    p.add_argument("--msg-target", default=BLUE_PATROL_1)
    p.add_argument("--track", default=DEFAULT_TRACK)
    p.add_argument("--profile", default="safe",
                   choices=("motion", "sensor", "comm", "weapon", "aux", "defer", "safe", "all"))
    p.add_argument("--only", default="", help="逗号分隔指令名 (覆盖 profile)")
    p.add_argument("--skip", default="", help="逗号分隔跳过")
    p.add_argument("--list", action="store_true", help="列出 profile 后退出")
    p.add_argument("--include-risky", action="store_true")
    p.add_argument("--include-release", action=argparse.BooleanOptionalAction, default=True,
                   help="profile 非 defer 时是否追加 Release/平台删除")
    p.add_argument("--wait", type=float, default=120.0)
    p.add_argument("--step-timeout", type=float, default=60.0)
    p.add_argument("--step-delay", type=float, default=1.0, help="空步进间隔")
    p.add_argument("--observe-steps", type=int, default=DEFAULT_OBSERVE,
                   help="未单独配置的指令默认观察步数")
    p.add_argument("--inter-command-delay", type=float, default=0.5,
                   help="两条指令之间的额外等待")
    p.add_argument("--warmup-max", type=int, default=6)
    p.add_argument("--empty-sync", action="store_true")
    args = p.parse_args()
    if args.list:
        print_profiles()
        catalog = build_catalog_for_args(args)
        log("\n全指令 observe 步数:")
        for c in catalog:
            if c.name in SKIP_IN_AUTO:
                continue
            tag = " [RISKY]" if c.risky else ""
            log(f"  {c.name:<28} observe={observe_count(c.name, args.observe_steps)}{tag}")
        return 0
    return run_controls(args)


if __name__ == "__main__":
    sys.exit(main())
