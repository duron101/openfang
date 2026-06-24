"""
arksense - ARK Matrix 2.0 态势解析模块

负责解析 ARKSim 仿真引擎发送的战场态势数据。
提供结构化的态势信息访问接口，包括平台信息、航迹信息、
阵营统计、距离计算、威胁评估等功能。

使用示例:
    from arksense import SituationParser

    # 从 JSON 数据解析
    parser = SituationParser(situation_data)

    # 从 JSON 文件解析
    parser = SituationParser.from_json_file("situation.json")

    # 获取平台信息
    blue_platforms = parser.get_blue_platforms()
    red_count = parser.get_side_statistics().get("red", 0)

    # 获取航迹
    tracks = parser.get_all_tracks()

    # 威胁评估
    threats = parser.get_threats_for_platform(platform_id, max_distance=50000)
"""

from .situation_parser import SituationParser

__all__ = [
    "SituationParser",
]

__version__ = "1.0.0"
