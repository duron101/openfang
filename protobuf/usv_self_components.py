"""
usv_loiter_strike 想定中平台 `self` 的 Warlock 组件树 (Component_id)。

通信: usv_cmd_radio, usv_loiter_radio
机动: mover (mover 部件，速度/航向/航渡指令走 agent_id=self)
传感器: surf_radar, eoir
武器: gun_30mm, loiter_wave1/2/3, scout_uav_slot
"""
from __future__ import annotations

DEFAULT_AGENT = "self"

# 通信
USV_COMM_CMD = "usv_cmd_radio"
USV_COMM_LOITER = "usv_loiter_radio"
DEFAULT_COMM = USV_COMM_CMD

# 机动 (Component_id 用于 part 级指令时；SetDesired* 仍用 agent=self)
USV_MOVER = "mover"

# 传感器
USV_SENSOR_RADAR = "surf_radar"
USV_SENSOR_EOIR = "eoir"
DEFAULT_SENSOR = USV_SENSOR_RADAR
DEFAULT_SENSOR_MODE = "SEARCH"  # 想定 initial_mode SEARCH

# 武器
USV_WEAPON_GUN = "gun_30mm"
USV_WEAPON_LOITER1 = "loiter_wave1"
USV_WEAPON_LOITER2 = "loiter_wave2"
USV_WEAPON_LOITER3 = "loiter_wave3"
USV_WEAPON_SCOUT = "scout_uav_slot"
DEFAULT_WEAPON = USV_WEAPON_LOITER2

# 航迹 (FireAtTarget API 格式 name:number)
DEFAULT_TRACK = "self:1"

# 蓝方平台名 (SendMsgToPlatform 测试目标)
BLUE_PATROL_1 = "blue_patrol_1"

COMPONENT_TREE: dict[str, str] = {
    "usv_cmd_radio": "通信-指挥链",
    "usv_loiter_radio": "通信-巡飞弹链",
    "mover": "机动",
    "usv_mission_mgr": "任务管理",
    "usv_track_processor": "航迹处理",
    "eoir": "传感器-光电",
    "surf_radar": "传感器-水面雷达",
    "gun_30mm": "武器-30mm炮",
    "loiter_wave1": "武器-巡飞弹波次1",
    "loiter_wave2": "武器-巡飞弹波次2",
    "loiter_wave3": "武器-巡飞弹波次3",
    "scout_uav_slot": "武器-侦察无人机槽",
    "fuel": "燃油",
}


def log_component_summary(log_fn) -> None:
    log_fn("self 平台组件 (usv_loiter_strike):")
    for name, desc in COMPONENT_TREE.items():
        log_fn(f"  {name:<18} {desc}")
