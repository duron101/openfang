import json
import uuid
import time
from typing import Dict, List, Optional, Any
from dataclasses import dataclass
from enum import Enum
from queue import Queue



class SituationType(Enum):
    """态势类型枚举"""
    CUSTOMIZED = 0  # 定制态势，对应arksim_customized_situation.proto
    REALTIME = 1    # 实时战场态势，对应zmq_observer_pb3.proto

@dataclass
class SimulationConfig:
    """仿真配置参数"""
    exec: int = 1
    offscreen: bool = False
    random_seed: int = 0
    realtime: bool = False
    scenarios: List[str] = None
    sim_type: int = 0
    
    def __post_init__(self):
        if self.scenarios is None:
            self.scenarios = ["D:/ArkSIM3.1.6/demo/floridistan/floridistan.txt"]

class ArkSIMControllerError(Exception):
    """ArkSIM控制器异常基类"""
    def __init__(self, message: str, error_code: str = None):
        self.message = message
        self.error_code = error_code
        super().__init__(self.message)

class ParameterRangeError(ArkSIMControllerError):
    """参数范围错误异常"""
    def __init__(self, param_name: str, value: Any, valid_range: str):
        super().__init__(f"参数 {param_name} 值 {value} 超出有效范围: {valid_range}", "PARAMETER_RANGE_ERROR")

class ArkSIMController:
    """ArkSIM 仿真控制与数据接口"""
    
    def __init__(self, service_address="tcp://127.0.0.1:60004", operation_name="controller", response_handler=None):
        from arkcore.logging import setup_logger
        self.logger = setup_logger('ArkSIMController', None, True)
        self.service_address = service_address
        self.operation_name = operation_name
        self.default_config = SimulationConfig()
        
        from arkcomm import SerializableLock
        self.socket_lock = SerializableLock()
        self.result_queue = Queue()
        
        if response_handler:
            # 复用外部传入的 ResponseHandler
            self.response_handler = response_handler
            self.socket_id = response_handler.socket_id
        else:
            # 生成唯一Socket ID
            self.socket_id = f'ark_ctrl_{uuid.uuid4().hex[:8]}'
            
            # 初始化响应处理器
            from arkcomm import ResponseHandler
            self.response_handler = ResponseHandler(
                service_address=self.service_address,
                socket_id=self.socket_id,
                operation_name=self.operation_name,
                result_queue=self.result_queue
            )
            self.response_handler.start()
        
        self.uuid = None
        self._is_connected = False
        self.active_instances = []
    
    def _validate_rate(self, rate: float) -> None:
        """验证速率参数
        
        Parameters
        ----------
            rate
                速率值
            
        Raises:
            ParameterRangeError: 速率超出有效范围
        """
        if not (0.01 <= rate <= 100.0):
            raise ParameterRangeError("rate", rate, "0.01-100.0")
    
    def _validate_uuid(self, instance_uuid: str) -> None:
        """验证UUID参数
        
        Parameters
        ----------
            instance_uuid
                实例UUID
            
        Raises:
            ParameterRangeError: UUID格式无效或为空
        """
        if not instance_uuid or not isinstance(instance_uuid, str):
            raise ParameterRangeError("instance_uuid", instance_uuid, "非空字符串")
        
        if len(instance_uuid.strip()) == 0:
            raise ParameterRangeError("instance_uuid", instance_uuid, "非空字符串")
    
    def start_instance(self, 
                      scenarios: List[str] = None,
                      offscreen: bool = None,
                      random_seed: int = None,
                      realtime: bool = None,
                      sim_type: int = None) -> Dict[str, Any]:
        """启动模拟器实例"""
        config = {
            "exec": self.default_config.exec,
            "offscreen": offscreen if offscreen is not None else self.default_config.offscreen,
            "randomSeed": random_seed if random_seed is not None else self.default_config.random_seed,
            "realtime": realtime if realtime is not None else self.default_config.realtime,
            "scenarios": scenarios or self.default_config.scenarios,
            "simType": sim_type if sim_type is not None else self.default_config.sim_type
        }
        
        # 返回与interface_new.json完全一致的结构
        result = {
            "args": config,
            "fn": "start"
        }
        
        return result
    
    def pause_simulation(self, instance_uuid: str) -> Dict[str, Any]:
        """暂停指定UUID的模拟实例"""
        
        result = {
            "fn": "pause",
            "uuid": instance_uuid
        }

        return result
    
    def resume_simulation(self, instance_uuid: str) -> Dict[str, Any]:
        """恢复被暂停的模拟实例"""
        
        result = {
            "fn": "resume",
            "uuid": instance_uuid
        }
        
        return result
    
    def stop_simulation(self, instance_uuid: str) -> Dict[str, Any]:
        """发送exit命令关闭模拟实例"""
        
        result = {
            "fn": "exit",
            "uuid": instance_uuid
        }
        
        return result
    
    def restart_simulation(self, instance_uuid: str) -> Dict[str, Any]:
        """重启指定的模拟实例"""
        
        result = {
            "fn": "restart",
            "uuid": instance_uuid
        }
        
        return result
    
    def run_step(self, instance_uuid: str, step: int = 1) -> Dict[str, Any]:
        """按指定步数执行步进模拟"""
        
        if step <= 0:
            raise ParameterRangeError("step", step, ">0")
        
        result = {
            "fn": "runstep",
            "args": {"step": step},
            "uuid": instance_uuid
        }
        
        return result
    
    def advance_to_time(self, instance_uuid: str, target_time: float) -> Dict[str, Any]:
        """快进到指定的模拟时间点"""
        self._validate_uuid(instance_uuid)
        
        if target_time < 0:
            raise ParameterRangeError("target_time", target_time, ">=0")
        
        result = {
            "fn": "advance_to_time",
            "args": {"time": target_time},
            "uuid": instance_uuid
        }
        
        return result
    
    def set_clock_rate(self, instance_uuid: str, rate: float = 1.0) -> Dict[str, Any]:
        """调整模拟速率"""
        self._validate_rate(rate)
        
        result = {
            "fn": "set_clock_rate",
            "args": {"rate": rate},
            "uuid": instance_uuid
        }
        
        return result
    
    def send_entity_command(self, instance_uuid: str, proto_str: str) -> Dict[str, Any]:
        """发送实体控制命令"""
        
        if not proto_str or not isinstance(proto_str, str):
            raise ParameterRangeError("proto_str", proto_str, "非空字符串")
        
        result = {
            "fn": "proto",
            "proto": proto_str,
            "uuid": instance_uuid
        }
        
        return result
    
    def switch_situation_type(self, instance_uuid: str, situation_type: SituationType) -> Dict[str, Any]:
        """切换态势输出类型"""
        
        result = {
            "fn": "changesituation",
            "rate": situation_type.value,
            "uuid": instance_uuid
        }
        
        return result
    
    def toggle_simulation_time_output(self, instance_uuid: str, enable: bool) -> Dict[str, Any]:
        """开关仿真时间输出"""
        
        result = {
            "fn": "simulationtimeswitch",
            "rate": enable,
            "uuid": instance_uuid
        }
        
        return result
    
    def get_instance_status(self, instance_uuid: str) -> Dict[str, Any]:
        """获取实例状态
        
        Parameters
        ----------
            instance_uuid
                实例UUID
            
        Returns
        -------
            包含实例状态信息的字典
        """
        self._validate_uuid(instance_uuid)
        
        result = {
            "fn": "get_status",
            "uuid": instance_uuid
        }
        
        return result
    
    def cleanup_stopped_instances(self) -> Dict[str, Any]:
        """清理已停止的实例
        
        Returns
        -------
            清理操作的结果信息
        """
        result = {
            "fn": "cleanup_instances",
            "cleaned_count": len(self.active_instances)
        }
        
        # 清空活跃实例记录
        self.active_instances.clear()
        
        return result
    
    def set_custom_situation_interval(self, instance_uuid: str, interval: float = 3.0) -> Dict[str, Any]:
        """设置定制态势输出时间间隔"""
        
        if interval <= 0:
            raise ParameterRangeError("interval", interval, ">0")
        
        result = {
            "fn": "customizedsituation",
            "time": interval,
            "uuid": instance_uuid
        }
        
        return result

    def apply_default_situation(
        self,
        instance_uuid: str,
        interval: float = 3.0,
    ) -> list:
        """应用默认定制态势配置（rate=0 + 推送间隔）。

        OpenFang 与 arksense 默认只消费定制态势 JSON（``customizedsituation``）。
        启动仿真实例后应调用本方法，再 ``resume``。
        """
        self._validate_uuid(instance_uuid)
        if interval <= 0:
            raise ParameterRangeError("interval", interval, ">0")
        return [
            self.switch_situation_type(instance_uuid, SituationType.CUSTOMIZED),
            self.set_custom_situation_interval(instance_uuid, interval),
        ]

