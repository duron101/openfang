"""
arkcmd - ARK Matrix 2.0 指令生成模块

提供仿真引擎控制指令和仿真实体控制指令的生成功能。
基于 ZeroMQ (通过 arkcomm) 发送指令，基于 Protobuf 序列化实体动作。

使用示例:
    # 仿真控制
    from arkcmd import ArkSIMController, SimulationConfig

    controller = ArkSIMController(
        service_address="tcp://127.0.0.1:60004"
    )
    cmd = controller.start_instance(
        scenarios=["D:/scenario.txt"],
        offscreen=True
    )

    # 实体控制
    from arkcmd import ProtoStringBuilder

    builder = ProtoStringBuilder()
    builder.set_desired_velocity("agent_001", 250.0)
    builder.set_desired_heading("agent_001", 1.57)
    proto_bytes = builder.serialize_actions()
"""

from .controller import (
    ArkSIMController,
    ArkSIMControllerError,
    ParameterRangeError,
    SimulationConfig,
    SituationType,
)

from .proto import ProtoStringBuilder

from .proto import (
    ActionsFromOutside,
    E_Actions,
)

__all__ = [
    # 仿真控制器
    "ArkSIMController",
    "ArkSIMControllerError",
    "ParameterRangeError",
    "SimulationConfig",
    "SituationType",
    # Protobuf 指令构建器
    "ProtoStringBuilder",
    # Protobuf 消息类
    "ActionsFromOutside",
    "E_Actions",
]

__version__ = "1.0.0"
