from typing import List, Dict, Any, Optional
import sys
import os

# 统一从 SimEnv.proto 加载生成的 protobuf 类，避免本地旧版本冲突
try:
    from .arksimActions_pb2 import (
        ActionsFromOutside, AgentContrl, DesiredHeading, DesiredAltitude,
        DesiredVelocity, GoToLocation, FollowRoute, Waypoint, SensorAction,
        ChangeSensorMode, FireAtTarget, FireSlavoAtTarget, ChangeJammingMode,
        SendMsgToPlatform, SendMsgToCommandChain, AfsimAuxCommand,
        PlatformAuxData, AuxData, AgentName, JammingModeStruct, E_Actions,
        ChangeCommander, ChangePlatformNumber,
    )
except ImportError:
    try:
        from SimEnv.proto.arksimActions_pb2 import (
            ActionsFromOutside, AgentContrl, DesiredHeading, DesiredAltitude,
            DesiredVelocity, GoToLocation, FollowRoute, Waypoint, SensorAction,
            ChangeSensorMode, FireAtTarget, FireSlavoAtTarget, ChangeJammingMode,
            SendMsgToPlatform, SendMsgToCommandChain, AfsimAuxCommand,
            PlatformAuxData, AuxData, AgentName, JammingModeStruct, E_Actions,
            ChangeCommander, ChangePlatformNumber,
        )
    except ImportError:
        print("Warning: arksimActions_pb2 not found. Please regenerate via grpc_tools.protoc.")
        raise

import re

_TRACK_DOT_RE = re.compile(r"^(.+)\.(\d+)$")


def normalize_track_id(track_id: str) -> str:
    """FireAtTarget 需要 `<platform>:<number>`；evt 日志常写成 `self.1`。"""
    tid = (track_id or "").strip()
    if not tid or ":" in tid:
        return tid
    m = _TRACK_DOT_RE.match(tid)
    if m:
        return f"{m.group(1)}:{m.group(2)}"
    return tid


class ProtoStringBuilder:
    """用于构建ArkSim Action Proto消息的工具类
    
    该类封装了arksimActions.proto中定义的所有指令类型，
    提供简单的接口来构建protobuf消息并返回ActionsFromOutside对象。
    """
    
    def __init__(self):
        """初始化ProtoStringBuilder"""
        self.actions = ActionsFromOutside()
    
    def clear_actions(self):
        """清空当前的指令集合"""
        self.actions = ActionsFromOutside()
    
    def get_actions(self) -> ActionsFromOutside:
        """获取当前构建的ActionsFromOutside对象"""
        return self.actions
    
    def serialize_actions(self) -> bytes:
        """序列化ActionsFromOutside对象为字节数据"""
        return self.actions.SerializeToString()
    
    # ==================== 基础控制指令 ====================
    
    def set_agent_outside_control(self, agent_id: str) -> ActionsFromOutside:
        """设置可控制实体
        
        Parameters
        ----------
            agent_id
                实体ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        agent_ctrl = AgentContrl()
        agent_ctrl.action = E_Actions.E_SetAgentOutsideControl
        agent_ctrl.agent_id = agent_id
        
        self.actions.a_agentcontrl.append(agent_ctrl)
        return self.actions
    
    def release_outside_control(self, agent_id: str) -> ActionsFromOutside:
        """取消可控制实体
        
        Parameters
        ----------
            agent_id
                实体ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        agent_ctrl = AgentContrl()
        agent_ctrl.action = E_Actions.E_ReleaseOutsideControl
        agent_ctrl.agent_id = agent_id
        
        self.actions.a_agentcontrl.append(agent_ctrl)
        return self.actions
    
    # ==================== 运动控制指令 ====================
    
    def set_desired_velocity(self, agent_id: str, desired_velocity: float, 
                           linear_accel: float = 0.0) -> ActionsFromOutside:
        """设置期望速度
        
        Parameters
        ----------
            agent_id
                实体ID
            desired_velocity
                期望速度 (m/s)
            linear_accel
                线性加速度 (m/s²)
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        velocity_msg = DesiredVelocity()
        velocity_msg.agent_id = agent_id
        velocity_msg.desired_velocity = desired_velocity
        velocity_msg.linearAccel = linear_accel
        
        self.actions.a_desiredvelocity.append(velocity_msg)
        return self.actions
    
    def set_desired_altitude(self, agent_id: str, desired_altitude: float,
                           altitude_rate: Optional[float] = None) -> ActionsFromOutside:
        """设置期望高度
        
        Parameters
        ----------
            agent_id
                实体ID
            desired_altitude
                期望高度 (m)
            altitude_rate
                上升/下降速率 (m/s)，可选
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        altitude_msg = DesiredAltitude()
        altitude_msg.agent_id = agent_id
        altitude_msg.desired_altitude = desired_altitude
        
        if altitude_rate is not None:
            altitude_msg.has_desired_altitude_rate = True
            altitude_msg.desired_altitude_rate = altitude_rate
        else:
            altitude_msg.has_desired_altitude_rate = False
        
        self.actions.a_desiredaltitude.append(altitude_msg)
        return self.actions
    
    def set_desired_heading(self, agent_id: str, desired_heading: float,
                          desired_velocity: Optional[float] = None,
                          turn_direction: Optional[int] = None) -> ActionsFromOutside:
        """设置期望朝向
        
        Parameters
        ----------
            agent_id
                实体ID
            desired_heading
                期望朝向 (弧度)
            desired_velocity
                转弯时的速度 (m/s)，可选
            turn_direction
                转向方向 (0-左，1-右)，可选
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        heading_msg = DesiredHeading()
        heading_msg.agent_id = agent_id
        heading_msg.desired_heading = desired_heading
        
        if desired_velocity is not None:
            heading_msg.has_desired_velocity = True
            heading_msg.desired_velocity = desired_velocity
        else:
            heading_msg.has_desired_velocity = False
        
        if turn_direction is not None:
            heading_msg.has_desired_turn_direction = True
            heading_msg.desired_turn_direction = turn_direction
        else:
            heading_msg.has_desired_turn_direction = False
        
        self.actions.a_desiredheading.append(heading_msg)
        return self.actions
    
    def go_to_location(self, agent_id: str, location_lla: List[float], 
                      priority: int = 1) -> ActionsFromOutside:
        """改变位置
        
        Parameters
        ----------
            agent_id
                实体ID
            location_lla
                目标位置 [纬度, 经度, 高度]
            priority
                优先级
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        location_msg = GoToLocation()
        location_msg.agent_id = agent_id
        location_msg.priority = priority
        location_msg.reportedLocationLLA.extend(location_lla)
        
        self.actions.a_gotolocation.append(location_msg)
        return self.actions
    
    def follow_route(self, agent_id: str, route_name: str, 
                    waypoints: List[Dict[str, Any]]) -> ActionsFromOutside:
        """设置跟随路线
        
        Parameters
        ----------
            agent_id
                实体ID
            route_name
                路线名称
            waypoints
                路径点列表，每个点包含 {"id": str, "speed": float, "location": [lat, lon, alt]}
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        route_msg = FollowRoute()
        route_msg.agent_id = agent_id
        route_msg.aRouteName = route_name
        
        for wp in waypoints:
            waypoint = Waypoint()
            waypoint.WaypointID = wp.get("id", "")
            waypoint.speed = wp.get("speed", 0.0)
            waypoint.reportedLocationLLA.extend(wp.get("location", [0.0, 0.0, 0.0]))
            route_msg.repeatedpoint.append(waypoint)
        
        self.actions.a_followroute.append(route_msg)
        return self.actions
    
    # ==================== 传感器控制指令 ====================
    
    def turn_on_sensor(self, agent_id: str, component_id: str = "") -> ActionsFromOutside:
        """打开传感器
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                传感器组件ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        sensor_action = SensorAction()
        sensor_action.action = E_Actions.E_TurnOnSensor
        sensor_action.agent.agent_id = agent_id
        sensor_action.agent.Component_id = component_id
        
        self.actions.a_sensoraction.append(sensor_action)
        return self.actions
    
    def turn_off_sensor(self, agent_id: str, component_id: str = "") -> ActionsFromOutside:
        """关闭传感器
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                传感器组件ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        sensor_action = SensorAction()
        sensor_action.action = E_Actions.E_TurnOffSensor
        sensor_action.agent.agent_id = agent_id
        sensor_action.agent.Component_id = component_id
        
        self.actions.a_sensoraction.append(sensor_action)
        return self.actions
    
    def change_sensor_mode(self, agent_id: str, component_id: str, mode: str) -> ActionsFromOutside:
        """改变传感器工作模式
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                传感器组件ID
            mode
                工作模式
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        mode_msg = ChangeSensorMode()
        mode_msg.agent.agent_id = agent_id
        mode_msg.agent.Component_id = component_id
        mode_msg.mode = mode
        
        self.actions.a_changesensormode.append(mode_msg)
        return self.actions
    
    def get_sensor_current_mode(self, agent_id: str, component_id: str = "") -> ActionsFromOutside:
        """获取传感器工作模式
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                传感器组件ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        sensor_action = SensorAction()
        sensor_action.action = E_Actions.E_GetSensorCurrentMode
        sensor_action.agent.agent_id = agent_id
        sensor_action.agent.Component_id = component_id
        
        self.actions.a_sensoraction.append(sensor_action)
        return self.actions
    
    # ==================== 武器控制指令 ====================
    
    def fire_at_target(self, agent_id: str, component_id: str, track_id: str) -> ActionsFromOutside:
        """开火
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                武器组件ID
            track_id
                目标跟踪ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        fire_msg = FireAtTarget()
        fire_msg.action = E_Actions.E_FireAtTarget
        fire_msg.agent.agent_id = agent_id
        fire_msg.agent.Component_id = component_id
        fire_msg.trck_id = normalize_track_id(track_id)
        
        self.actions.a_fireattarget.append(fire_msg)
        return self.actions
    
    def fire_salvo_at_target(self, agent_id: str, component_id: str, 
                           track_id: str, salvo_size: int) -> ActionsFromOutside:
        """用特定武器向目标齐射
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                武器组件ID
            track_id
                目标跟踪ID
            salvo_size
                齐射数量
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        salvo_msg = FireSlavoAtTarget()
        salvo_msg.agent.agent_id = agent_id
        salvo_msg.agent.Component_id = component_id
        salvo_msg.trck_id = normalize_track_id(track_id)
        salvo_msg.slavo_size = salvo_size
        
        self.actions.a_fireslavoattarget.append(salvo_msg)
        return self.actions
    
    def update_target(self, agent_id: str, component_id: str = "") -> ActionsFromOutside:
        """更新当前目标
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                组件ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        sensor_action = SensorAction()
        sensor_action.action = E_Actions.E_UpdateTarget
        sensor_action.agent.agent_id = agent_id
        sensor_action.agent.Component_id = component_id
        
        self.actions.a_sensoraction.append(sensor_action)
        return self.actions
    
    # ==================== 干扰控制指令 ====================
    
    def start_jamming(self, agent_id: str, component_id: str, track_id: str) -> ActionsFromOutside:
        """开干扰
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                干扰组件ID
            track_id
                目标跟踪ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        jamming_msg = FireAtTarget()
        jamming_msg.action = E_Actions.E_StartJamming
        jamming_msg.agent.agent_id = agent_id
        jamming_msg.agent.Component_id = component_id
        jamming_msg.trck_id = normalize_track_id(track_id)
        
        self.actions.a_fireattarget.append(jamming_msg)
        return self.actions
    
    def stop_jamming(self, agent_id: str, component_id: str = "") -> ActionsFromOutside:
        """关干扰
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                干扰组件ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        sensor_action = SensorAction()
        sensor_action.action = E_Actions.E_StopJamming
        sensor_action.agent.agent_id = agent_id
        sensor_action.agent.Component_id = component_id
        
        self.actions.a_sensoraction.append(sensor_action)
        return self.actions
    
    def change_jamming_mode(self, agent_id: str, component_id: str,
                          frequency: float, bandwidth: float, beam_number: int) -> ActionsFromOutside:
        """改变干扰工作模式
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                干扰组件ID
            frequency
                频率
            bandwidth
                带宽
            beam_number
                波束数量
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        jamming_mode = ChangeJammingMode()
        jamming_mode.agent.agent_id = agent_id
        jamming_mode.agent.Component_id = component_id
        jamming_mode.mode.aFrequency = frequency
        jamming_mode.mode.aBandwidth = bandwidth
        jamming_mode.mode.aBeamNumber = beam_number
        
        self.actions.a_changejammingmode.append(jamming_mode)
        return self.actions
    
    # ==================== 通信控制指令 ====================
    
    def turn_on_comm(self, agent_id: str, component_id: str = "") -> ActionsFromOutside:
        """通信开机
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                通信组件ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        sensor_action = SensorAction()
        sensor_action.action = E_Actions.E_TurnOnComm
        sensor_action.agent.agent_id = agent_id
        sensor_action.agent.Component_id = component_id
        
        self.actions.a_sensoraction.append(sensor_action)
        return self.actions
    
    def turn_off_comm(self, agent_id: str, component_id: str = "") -> ActionsFromOutside:
        """通信关机
        
        Parameters
        ----------
            agent_id
                实体ID
            component_id
                通信组件ID
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        sensor_action = SensorAction()
        sensor_action.action = E_Actions.E_TurnOffComm
        sensor_action.agent.agent_id = agent_id
        sensor_action.agent.Component_id = component_id
        
        self.actions.a_sensoraction.append(sensor_action)
        return self.actions
    
    def send_msg_to_platform(self, agent_id: str, component_id: str,
                           target_id: str, message_content: str) -> ActionsFromOutside:
        """向特定平台发送消息
        
        Parameters
        ----------
            agent_id
                发送方实体ID
            component_id
                通信组件ID
            target_id
                目标平台ID
            message_content
                消息内容
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        msg_platform = SendMsgToPlatform()
        msg_platform.agent.agent_id = agent_id
        msg_platform.agent.Component_id = component_id
        msg_platform.target_id = target_id
        msg_platform.message = message_content
        
        self.actions.a_sendmsgtoplatform.append(msg_platform)
        return self.actions
    
    def send_msg_to_command_chain(self, agent_id: str, component_id: str,
                                target_id: str, mode: int, message_content: str) -> ActionsFromOutside:
        """向特定指挥链发送消息
        
        Parameters
        ----------
            agent_id
                发送方实体ID
            component_id
                通信组件ID
            target_id
                目标指挥链ID
            mode
                模式
            message_content
                消息内容
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        msg_chain = SendMsgToCommandChain()
        msg_chain.agent.agent_id = agent_id
        msg_chain.agent.Component_id = component_id
        msg_chain.target_id = target_id
        msg_chain.mode = mode
        msg_chain.message = message_content
        
        self.actions.a_sendmsgtocommandchain.append(msg_chain)
        return self.actions
    
    # ==================== 辅助数据指令 ====================
    
    def set_aux_data(self, platform_name: str, aux_data_list: List[Dict[str, Any]], 
                    index: int = 0) -> ActionsFromOutside:
        """设置辅助数据
        
        Parameters
        ----------
            platform_name
                平台名称
            aux_data_list
                辅助数据列表，每个元素包含 {"key": str, "type": int, "value": Any}
                          type
                              0=STRING, 1=DOUBLE, 2=BOOL, 3=DICT
            index
                平台在 StateMessage.PlatformState.index 中的序号（非固定 0；
                mid_ark 从态势回填 seenPlatformNames[name]）
            
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        aux_command = AfsimAuxCommand()
        platform_aux = PlatformAuxData()
        platform_aux.name = platform_name
        platform_aux.index = index
        
        for data in aux_data_list:
            aux_data = AuxData()
            aux_data.key = data["key"]
            aux_data.type = data["type"]
            
            if data["type"] == 0:  # STRING
                aux_data.stringValue = str(data["value"])
            elif data["type"] == 1:  # DOUBLE
                aux_data.doubleValue = float(data["value"])
            elif data["type"] == 2:  # BOOL
                aux_data.boolValue = bool(data["value"])
            elif data["type"] == 3:  # DICT
                # 对于字典类型，需要递归处理
                for key, value in data["value"].items():
                    dict_aux = AuxData()
                    dict_aux.key = key
                    if isinstance(value, str):
                        dict_aux.type = 0
                        dict_aux.stringValue = value
                    elif isinstance(value, bool):
                        dict_aux.type = 2
                        dict_aux.boolValue = value
                    elif isinstance(value, (int, float)):
                        dict_aux.type = 1
                        dict_aux.doubleValue = float(value)
                    aux_data.dictValue.append(dict_aux)
            
            platform_aux.auxdata.append(aux_data)
        
        aux_command.platformAux.append(platform_aux)
        self.actions.a_afsimauxcommand.append(aux_command)
        return self.actions
    
    # ==================== 组合指令构建器 ====================
    
    def merge_actions(self, other_builder: 'ProtoStringBuilder') -> ActionsFromOutside:
        """合并另一个构建器的指令
        
        Parameters
        ----------
            other_builder
                另一个ProtoStringBuilder实例
            
        Returns
        -------
            合并后的ActionsFromOutside对象
        """
        other_actions = other_builder.get_actions()
        
        # 合并各类指令
        self.actions.a_agentcontrl.extend(other_actions.a_agentcontrl)
        self.actions.a_desiredheading.extend(other_actions.a_desiredheading)
        self.actions.a_desiredaltitude.extend(other_actions.a_desiredaltitude)
        self.actions.a_desiredvelocity.extend(other_actions.a_desiredvelocity)
        self.actions.a_gotolocation.extend(other_actions.a_gotolocation)
        self.actions.a_followroute.extend(other_actions.a_followroute)
        self.actions.a_sensoraction.extend(other_actions.a_sensoraction)
        self.actions.a_changesensormode.extend(other_actions.a_changesensormode)
        self.actions.a_fireattarget.extend(other_actions.a_fireattarget)
        self.actions.a_fireslavoattarget.extend(other_actions.a_fireslavoattarget)
        self.actions.a_changejammingmode.extend(other_actions.a_changejammingmode)
        self.actions.a_sendmsgtoplatform.extend(other_actions.a_sendmsgtoplatform)
        self.actions.a_sendmsgtocommandchain.extend(other_actions.a_sendmsgtocommandchain)
        self.actions.a_afsimauxcommand.extend(other_actions.a_afsimauxcommand)
        # 新增：实体数量与指挥链更换指令集合
        self.actions.a_ChangeCommander.extend(other_actions.a_ChangeCommander)
        self.actions.a_ChangePlatformNumber.extend(other_actions.a_ChangePlatformNumber)
        
        return self.actions

    # ==================== 实体/指挥链管理指令 ====================
    def change_platform_number(
        self,
        name: str,
        ordertype: bool,
        type: str,
        side: str,
        lon: float,
        lat: float,
        alt: float,
        direction: float,
        speed: float
    ) -> ActionsFromOutside:
        """修改仿真实体数量（增加/删除）
        
        Parameters
        ----------
            name
                平台名称
            ordertype
                操作类型（True=增加，False=删除）
            type
                实体类型（例如 'Fighter'、'Bomber'）
            side
                阵营（例如 'blue'、'red'）
            lon
                经度（范围 [-180, 180]）
            lat
                纬度（范围 [-90, 90]）
            alt
                高度（米）
            direction
                朝向（度，范围 [0, 360)）
            speed
                速度（m/s，非负）
        
        Returns
        -------
            ActionsFromOutside protobuf对象
        
        Raises:
            ValueError: 当参数无效时抛出
        
        Examples:
            >>> builder.change_platform_number(
            ...     name='Platform_003', ordertype=True, type='Fighter', side='blue',
            ...     lon=116.4, lat=39.9, alt=10000.0, direction=90.0, speed=250.0
            ... )
        """
        # 基本校验
        if not isinstance(name, str) or not name.strip():
            raise ValueError("name 不能为空")
        if not isinstance(type, str) or not type.strip():
            raise ValueError("type 不能为空")
        if not isinstance(side, str) or not side.strip():
            raise ValueError("side 不能为空")
        # 数值校验
        try:
            lon = float(lon)
            lat = float(lat)
            alt = float(alt)
            direction = float(direction)
            speed = float(speed)
        except Exception:
            raise ValueError("经纬度/高度/朝向/速度必须为数值类型")
        if not (-180.0 <= lon <= 180.0):
            raise ValueError("lon 越界，合法范围 [-180, 180]")
        if not (-90.0 <= lat <= 90.0):
            raise ValueError("lat 越界，合法范围 [-90, 90]")
        if speed < 0:
            raise ValueError("speed 必须为非负数")
        if not (0.0 <= direction < 360.0):
            raise ValueError("direction 越界，合法范围 [0, 360)")
        if not isinstance(ordertype, bool):
            raise ValueError("ordertype 必须为布尔类型")

        msg = ChangePlatformNumber()
        msg.name = name
        msg.ordertype = ordertype
        msg.type = type
        msg.side = side
        msg.lon = lon
        msg.lat = lat
        msg.alt = alt
        msg.direction = direction
        msg.speed = speed
        
        self.actions.a_ChangePlatformNumber.append(msg)
        return self.actions

    def handle_change_platform_number(
        self,
        name: str,
        ordertype: bool,
        type: str,
        side: str,
        lon: float,
        lat: float,
        alt: float,
        direction: float,
        speed: float
    ) -> ActionsFromOutside:
        """包装处理接口：修改仿真实体数量（与 change_platform_number 一致）
        
        Parameters
        ----------
            同 change_platform_number
        
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        return self.change_platform_number(
            name=name, ordertype=ordertype, type=type, side=side,
            lon=lon, lat=lat, alt=alt, direction=direction, speed=speed
        )

    def change_commander(self, name: str, commander: str) -> ActionsFromOutside:
        """更换实体的指挥链上级
        
        Parameters
        ----------
            name
                平台名称
            commander
                新的上级实体名称
        
        Returns
        -------
            ActionsFromOutside protobuf对象
        
        Raises:
            ValueError: 当平台名或上级名为空时抛出
        
        Examples:
            >>> builder.change_commander(name='Platform_001', commander='Commander_A')
        """
        if not isinstance(name, str) or not name.strip():
            raise ValueError("name 不能为空")
        if not isinstance(commander, str) or not commander.strip():
            raise ValueError("commander 不能为空")
        
        msg = ChangeCommander()
        msg.name = name
        msg.commander = commander
        
        self.actions.a_ChangeCommander.append(msg)
        return self.actions

    def handle_change_commander(self, name: str, commander: str) -> ActionsFromOutside:
        """包装处理接口：更换实体的指挥链上级（与 change_commander 一致）
        
        Parameters
        ----------
            name
                平台名称
            commander
                新的上级实体名称
        
        Returns
        -------
            ActionsFromOutside protobuf对象
        """
        return self.change_commander(name=name, commander=commander)

