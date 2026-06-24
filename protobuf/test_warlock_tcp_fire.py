#!/usr/bin/env python3
"""
Warlock ZMQ 18000 自动化 FireAtTarget / FireSalvoAtTarget + 机动观察测试

用法:
  python test_warlock_tcp_fire.py
  python test_warlock_tcp_fire.py --mode salvo --observe-steps 10
  python test_warlock_tcp_controls.py --profile safe   # 全控制指令
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Optional

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))

from arkcmd.proto.proto_utils import ProtoStringBuilder, normalize_track_id  # noqa: E402
from warlock_command_walkthrough import (  # noqa: E402
    DEFAULT_AGENT,
    DEFAULT_TCP_PORT,
    DEFAULT_TRACK,
    DEFAULT_WEAPON,
    EMPTY_ACTIONS,
    ZmqStepClient,
    log,
)
from warlock_tcp_harness import (  # noqa: E402
    connect_client,
    ensure_outside_control,
    final_sim_time,
    log_step_result,
    observe_sim,
    require_step,
    resolve_track,
    run_motion_phase,
    wait_for_port,
)

LOG_PATH = ROOT / "test_warlock_tcp_fire.log"


def build_weapon_command(args: argparse.Namespace, track: str) -> tuple[str, bytes]:
    b = ProtoStringBuilder()
    if args.mode == "salvo":
        b.fire_salvo_at_target(args.agent, args.weapon, track, args.salvo_size)
        salvo_msg = b.get_actions().a_fireslavoattarget[0]
        label = "E_FireSlavoAtTarget"
        log(
            f"  >> {label} ({len(b.serialize_actions())} bytes) "
            f"weapon={salvo_msg.agent.Component_id!r} track={salvo_msg.trck_id!r} "
            f"salvo_size={salvo_msg.slavo_size}"
        )
        return label, b.serialize_actions()

    b.fire_at_target(args.agent, args.weapon, track)
    fire_msg = b.get_actions().a_fireattarget[0]
    label = "E_FireAtTarget"
    log(
        f"  >> {label} ({len(b.serialize_actions())} bytes) "
        f"weapon={fire_msg.agent.Component_id!r} track={fire_msg.trck_id!r}"
    )
    return label, b.serialize_actions()


def run_tcp_fire(args: argparse.Namespace) -> int:
    LOG_PATH.write_text("", encoding="utf-8")
    args.track = normalize_track_id(args.track)
    est = 0
    if args.pre_motion:
        est += 2 * args.observe_steps * 3 + 2
    if args.post_motion:
        est += 2 * args.observe_steps * 3 + 2
    est += 6

    log("Warlock ZMQ 18000 自动化武器 + 机动观察测试")
    log(
        f"  mode={args.mode!r}  agent={args.agent!r}  weapon={args.weapon!r} "
        f"track={args.track!r}  salvo_size={args.salvo_size}"
    )
    log(
        f"  pre_motion={args.pre_motion}  post_motion={args.post_motion}  "
        f"observe_steps={args.observe_steps}  (预估仿真 ~{est}s+)"
    )

    if not wait_for_port(args.host, args.port, args.wait):
        log("FAIL: 18000 未监听")
        log("  备选: python test_warlock_tcp_controls.py --profile weapon")
        return 1

    client: Optional[ZmqStepClient] = None
    try:
        client = connect_client(
            args.host, args.port, args.agent,
            step_timeout=args.step_timeout,
        )

        if args.empty_sync:
            log("  >> 空步进同步态势")
            require_step(client, EMPTY_ACTIONS, label="empty_sync")

        ensure_outside_control(client, args.agent)

        if args.pre_motion:
            run_motion_phase(
                client, args.agent,
                phase="pre_fire",
                speed_ms=args.pre_speed,
                heading_deg=args.pre_heading_deg,
                heading_speed_ms=args.pre_heading_speed,
                observe_steps=args.observe_steps,
                step_delay=args.step_delay,
            )

        fire_track = resolve_track(
            client, args.agent, args.track, args.warmup_max, args.step_delay,
        )

        log("\n--- 开火 ---")
        fire_label, fire_data = build_weapon_command(args, fire_track)
        log_step_result(require_step(client, fire_data, label=fire_label), "fire")

        if args.fire_observe_steps > 0:
            log(f"  ... 开火后空步进 x{args.fire_observe_steps}")
            observe_sim(client, args.fire_observe_steps, "after_fire", args.step_delay)

        if args.post_motion:
            run_motion_phase(
                client, args.agent,
                phase="post_fire",
                speed_ms=args.post_speed,
                heading_deg=args.post_heading_deg,
                heading_speed_ms=args.post_heading_speed,
                observe_steps=args.observe_steps,
                step_delay=args.step_delay,
            )

        sim_t = final_sim_time(client, args.agent)
        if sim_t is not None:
            log(
                f"\nPASS: {args.mode} recv={client._recv_count}  "
                f"sim_time≈{sim_t:.1f}s — 请确认 Warlock 速度/航向/开火效果"
            )
        else:
            log(f"\nPASS: {args.mode} recv={client._recv_count}")
        return 0
    except KeyboardInterrupt:
        log("用户中断")
        return 130
    except Exception as exc:
        log(f"FAIL: {exc}")
        return 1
    finally:
        if client is not None:
            client.close()


def main() -> int:
    p = argparse.ArgumentParser(description="Warlock ZMQ 18000 开火/齐射 + 机动观察")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=DEFAULT_TCP_PORT)
    p.add_argument("--mode", choices=("fire", "salvo"), default="fire")
    p.add_argument("--agent", default=DEFAULT_AGENT)
    p.add_argument("--weapon", default=DEFAULT_WEAPON)
    p.add_argument("--track", default=DEFAULT_TRACK)
    p.add_argument("--salvo-size", type=int, default=2)
    p.add_argument("--wait", type=float, default=120.0)
    p.add_argument("--step-timeout", type=float, default=60.0)
    p.add_argument("--step-delay", type=float, default=1.0)
    p.add_argument("--warmup-max", type=int, default=6)
    p.add_argument("--observe-steps", type=int, default=8)
    p.add_argument("--fire-observe-steps", type=int, default=4)
    p.add_argument("--pre-speed", type=float, default=12.0)
    p.add_argument("--pre-heading-deg", type=float, default=90.0)
    p.add_argument("--pre-heading-speed", type=float, default=10.0)
    p.add_argument("--post-speed", type=float, default=8.0)
    p.add_argument("--post-heading-deg", type=float, default=180.0)
    p.add_argument("--post-heading-speed", type=float, default=10.0)
    p.add_argument("--pre-motion", action=argparse.BooleanOptionalAction, default=True)
    p.add_argument("--post-motion", action=argparse.BooleanOptionalAction, default=True)
    p.add_argument("--empty-sync", action="store_true")
    return run_tcp_fire(p.parse_args())


if __name__ == "__main__":
    sys.exit(main())
