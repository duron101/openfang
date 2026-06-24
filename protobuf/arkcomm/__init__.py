"""
arkcomm - ARK Matrix 2.0 通信模块

基于 ZeroMQ 的通信环境，负责与外部仿真环境交互。
提供消息队列、响应处理器等核心通信组件。

使用示例:
    from arkcomm import ResponseHandler, MessageQueue

    handler = ResponseHandler(
        service_address="tcp://127.0.0.1:60004",
        socket_id="agent_001",
        operation_name="train",
        result_queue=result_queue
    )
    handler.start()
"""

from .response_handler import (
    MessageQueue,
    ResponseHandler,
    SerializableLock,
    MESSAGE_KEYS,
    MESSAGE_STATUS,
)

__all__ = [
    "MessageQueue",
    "ResponseHandler",
    "SerializableLock",
    "MESSAGE_KEYS",
    "MESSAGE_STATUS",
]

__version__ = "1.0.0"
