#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""
态势解析类 - 用于解析仿真服务器传来的态势信息

该类提供了对态势数据的结构化访问接口，包括：
- 平台信息解析（位置、速度、武器等）
- 航迹信息解析
- 按阵营、类型、空间域等条件筛选
- 距离计算、威胁评估等辅助功能
"""

import math
from typing import Dict, List, Optional, Tuple, Any
import copy


class SituationParser:
    """态势解析类"""
    
    def __init__(self, situation_data: Dict[str, Any]):
        """
        初始化态势解析器
        
        Parameters
        ----------
            situation_data
                从get_latest_situation()获取的态势数据字典
        """
        self.raw_data = situation_data
        self.customized_situation = situation_data.get('customizedsituation', {})
        self.platforms = self.customized_situation.get('platforms', [])
        self.end_time = self.customized_situation.get('endTime', 0)
        
        # 缓存处理后的数据
        self._platforms_by_side = {}
        self._platforms_by_type = {}
        self._platforms_by_id = {}
        self._platforms_by_name = {}
        self._tracks_cache = None
        
        # 指挥链相关缓存
        self._commander_relations_by_side = {}
        self._root_commanders_by_side = {}
        self._parent_by_id = {}
        self._group_roots_by_side = {}
        
        self._build_cache()
        self._build_command_chain()
    
    @classmethod
    def from_json_file(cls, json_file_path: str) -> 'SituationParser':
        """
        从JSON文件创建态势解析器
        
        Parameters
        ----------
            json_file_path
                JSON文件路径
            
        Returns
        -------
            SituationParser实例
        """
        import json
        with open(json_file_path, 'r', encoding='utf-8') as f:
            situation_data = json.load(f)
        return cls(situation_data)
    
    def _build_cache(self):
        """构建缓存数据结构以提高查询效率"""
        for platform in self.platforms:
            # 按阵营分类
            side = platform.get('side', 'unknown')
            if side not in self._platforms_by_side:
                self._platforms_by_side[side] = []
            self._platforms_by_side[side].append(platform)
            
            # 按类型分类
            platform_type = platform.get('type', 'unknown')
            if platform_type not in self._platforms_by_type:
                self._platforms_by_type[platform_type] = []
            self._platforms_by_type[platform_type].append(platform)
            
            # 按ID索引
            unique_id = platform.get('uniqueId')
            if unique_id is not None:
                self._platforms_by_id[unique_id] = platform
            
            # 按名称索引
            name = platform.get('name')
            if isinstance(name, str) and name:
                self._platforms_by_name[name] = platform

    def _is_root_commander(self, platform: Dict[str, Any]) -> bool:
        """
        判断平台是否为自己阵营的根指挥官
        根指挥官的特征：无 commanderId 且 commander 为空/SELF/DEFAULT
        """
        commander_id = platform.get('commanderId')
        commander_name = platform.get('commander')
        if isinstance(commander_id, int):
            return False
        if isinstance(commander_name, str):
            stripped = commander_name.strip().upper()
            if stripped and stripped not in ('', 'SELF', 'DEFAULT'):
                return False
        return True

    def _resolve_commander(self, platform: Dict[str, Any]) -> Optional[int]:
        """
        解析平台的上级指挥官 uniqueId
        返回值: commander_uid 或 None
        """
        commander_id = platform.get('commanderId')
        commander_name = platform.get('commander')
        subordinate_uid = platform.get('uniqueId')
        side = platform.get('side', 'unknown')

        resolved_uid = None

        # 优先级 1: commanderId 直接关联（最可靠）
        if isinstance(commander_id, int) and commander_id in self._platforms_by_id:
            resolved_uid = commander_id

        # 优先级 2: commander 名称精确匹配平台名
        elif isinstance(commander_name, str):
            stripped = commander_name.strip()
            if stripped and stripped.upper() not in ('', 'SELF', 'DEFAULT'):
                if stripped in self._platforms_by_name:
                    commander_platform = self._platforms_by_name[stripped]
                    resolved_uid = commander_platform.get('uniqueId')

        # 避免自己指向自己
        if resolved_uid is not None and resolved_uid == subordinate_uid:
            resolved_uid = None

        return resolved_uid

    def _build_command_chain(self):
        """
        构建指挥链关系缓存

        关联规则（按优先级）：
        1. commanderId 直接关联 — 最高优先级，适用于数值 ID 引用
        2. commander 名称精确匹配 — 适用于 "alpha"/"red_leader" 等平台名引用

        根指挥官判定：无 commanderId 且 commander 为空/SELF/DEFAULT
        """
        self._commander_relations_by_side = {}
        self._root_commanders_by_side = {}
        self._parent_by_id = {}

        for platform in self.platforms:
            side = platform.get('side', 'unknown')
            subordinate_uid = platform.get('uniqueId')
            if subordinate_uid is None:
                continue

            relations = self._commander_relations_by_side.setdefault(side, {})

            commander_uid = self._resolve_commander(platform)

            if commander_uid is not None:
                relations.setdefault(commander_uid, []).append(subordinate_uid)
                self._parent_by_id[subordinate_uid] = commander_uid

        # 计算每个阵营的根指挥官：无上级者的所有平台
        for side, relations in self._commander_relations_by_side.items():
            all_uids_in_side = {
                p.get('uniqueId')
                for p in self.platforms
                if p.get('side') == side and p.get('uniqueId') is not None
            }
            children_uids = set(self._parent_by_id.keys())
            side_children = {
                uid for uid in children_uids
                if self._platforms_by_id.get(uid, {}).get('side') == side
            }
            root_uids = all_uids_in_side - side_children
            self._root_commanders_by_side[side] = root_uids
    
    # ==================== 指挥链查询 ====================
    
    def get_commander_of(self, unique_id: int) -> Optional[int]:
        """获取指定平台的上级指挥官 uniqueId，无上级时返回 None"""
        return self._parent_by_id.get(unique_id)
    
    def get_subordinates_of(self, unique_id: int) -> List[int]:
        """获取指定平台的所有直属下属 uniqueId 列表"""
        subordinates = []
        for side, relations in self._commander_relations_by_side.items():
            if unique_id in relations:
                subordinates.extend(relations[unique_id])
        return sorted(subordinates)
    
    def get_root_commanders(self, side: str) -> List[int]:
        """获取指定阵营的所有根指挥官 uniqueId 列表"""
        return sorted(self._root_commanders_by_side.get(side, set()))
    
    # ==================== 基础信息获取 ====================
    
    def get_end_time(self) -> float:
        """获取仿真结束时间"""
        return self.end_time
    
    def get_all_platforms(self) -> List[Dict[str, Any]]:
        """获取所有平台信息"""
        return copy.deepcopy(self.platforms)
    
    def get_platform_count(self) -> int:
        """获取平台总数"""
        return len(self.platforms)
    
    def get_platform_by_id(self, unique_id: int) -> Optional[Dict[str, Any]]:
        """根据唯一ID获取平台信息"""
        platform = self._platforms_by_id.get(unique_id)
        return copy.deepcopy(platform) if platform else None
    
    def get_platform_by_name(self, name: str) -> Optional[Dict[str, Any]]:
        """根据名称获取平台信息"""
        platform = self._platforms_by_name.get(name)
        return copy.deepcopy(platform) if platform else None
    
    # ==================== 按条件筛选 ====================
    
    def get_platforms_by_side(self, side: str) -> List[Dict[str, Any]]:
        """根据阵营获取平台列表"""
        platforms = self._platforms_by_side.get(side, [])
        return copy.deepcopy(platforms)
    
    def get_platforms_by_type(self, platform_type: str) -> List[Dict[str, Any]]:
        """根据类型获取平台列表"""
        platforms = self._platforms_by_type.get(platform_type, [])
        return copy.deepcopy(platforms)
    
    def get_platforms_by_category(self, category: str) -> List[Dict[str, Any]]:
        """根据类别获取平台列表"""
        result = []
        for platform in self.platforms:
            if platform.get('category') == category:
                result.append(copy.deepcopy(platform))
        return result
    
    def get_platforms_by_spatial_domain(self, domain: str) -> List[Dict[str, Any]]:
        """根据空间域获取平台列表 (land, air, surface, subsurface, space)"""
        result = []
        for platform in self.platforms:
            if platform.get('spatialDomain') == domain:
                result.append(copy.deepcopy(platform))
        return result
    
    def get_blue_platforms(self) -> List[Dict[str, Any]]:
        """获取蓝方平台"""
        return self.get_platforms_by_side('blue')
    
    def get_red_platforms(self) -> List[Dict[str, Any]]:
        """获取红方平台"""
        return self.get_platforms_by_side('red')
    
    def get_neutral_platforms(self) -> List[Dict[str, Any]]:
        """获取中立平台"""
        return self.get_platforms_by_side('neutral')
    
    # ==================== 位置和运动信息 ====================
    
    def get_platform_location_lla(self, unique_id: int) -> Optional[Tuple[float, float, float]]:
        """获取平台LLA坐标 (纬度, 经度, 高度)"""
        platform = self.get_platform_by_id(unique_id)
        if platform and 'locationLLA' in platform:
            lla = platform['locationLLA']
            if len(lla) >= 3:
                return (lla[0], lla[1], lla[2])
        return None
    
    def get_platform_location_wcs(self, unique_id: int) -> Optional[Tuple[float, float, float]]:
        """获取平台WCS坐标 (X, Y, Z)"""
        platform = self.get_platform_by_id(unique_id)
        if platform and 'locationWCS' in platform:
            wcs = platform['locationWCS']
            if len(wcs) >= 3:
                return (wcs[0], wcs[1], wcs[2])
        return None
    
    def get_platform_velocity_ned(self, unique_id: int) -> Optional[Tuple[float, float, float]]:
        """获取平台NED速度 (北, 东, 下)"""
        platform = self.get_platform_by_id(unique_id)
        if platform and 'velocityNED' in platform:
            vel = platform['velocityNED']
            if len(vel) >= 3:
                return (vel[0], vel[1], vel[2])
        return None
    
    def get_platform_orientation_ned(self, unique_id: int) -> Optional[Tuple[float, float, float]]:
        """获取平台NED姿态 (航向, 俯仰, 横滚) - 弧度"""
        platform = self.get_platform_by_id(unique_id)
        if platform and 'orientationNED' in platform:
            orient = platform['orientationNED']
            if len(orient) >= 3:
                return (orient[0], orient[1], orient[2])
        return None
    
    # ==================== 武器信息 ====================
    
    def get_platform_weapons(self, unique_id: int) -> Dict[str, Dict[str, Any]]:
        """获取平台武器信息"""
        platform = self.get_platform_by_id(unique_id)
        if platform and 'weapons' in platform:
            return copy.deepcopy(platform['weapons'])
        return {}
    
    def get_weapon_count(self, unique_id: int, weapon_name: str) -> float:
        """获取指定武器的剩余数量"""
        weapons = self.get_platform_weapons(unique_id)
        if weapon_name in weapons:
            return weapons[weapon_name].get('quantityRemaining', 0)
        return 0
    
    def get_all_weapon_types(self) -> List[str]:
        """获取所有武器类型"""
        weapon_types = set()
        for platform in self.platforms:
            weapons = platform.get('weapons', {})
            for weapon_info in weapons.values():
                weapon_type = weapon_info.get('type')
                if weapon_type:
                    weapon_types.add(weapon_type)
        return list(weapon_types)
    
    # ==================== 航迹信息 ====================
    
    def get_all_tracks(self) -> List[Dict[str, Any]]:
        """获取所有航迹信息"""
        if self._tracks_cache is None:
            tracks = []
            for platform in self.platforms:
                platform_tracks = platform.get('tracks', [])
                for track in platform_tracks:
                    # 添加所属平台信息
                    track_copy = copy.deepcopy(track)
                    track_copy['ownerPlatformId'] = platform.get('uniqueId')
                    track_copy['ownerPlatformName'] = platform.get('name')
                    tracks.append(track_copy)
            self._tracks_cache = tracks
        return copy.deepcopy(self._tracks_cache)
    
    def get_tracks_by_platform(self, unique_id: int) -> List[Dict[str, Any]]:
        """获取指定平台的航迹"""
        platform = self.get_platform_by_id(unique_id)
        if platform and 'tracks' in platform:
            return copy.deepcopy(platform['tracks'])
        return []
    
    def get_tracks_by_side(self, side: str) -> List[Dict[str, Any]]:
        """获取指定阵营的航迹"""
        tracks = []
        for track in self.get_all_tracks():
            if track.get('side') == side:
                tracks.append(track)
        return tracks
    
    def get_enemy_tracks_for_platform(self, unique_id: int) -> List[Dict[str, Any]]:
        """获取指定平台探测到的敌方航迹"""
        platform = self.get_platform_by_id(unique_id)
        if not platform:
            return []
        
        platform_side = platform.get('side')
        tracks = self.get_tracks_by_platform(unique_id)
        
        enemy_tracks = []
        for track in tracks:
            track_side = track.get('side')
            # 简单的敌我判断逻辑
            if (platform_side == 'blue' and track_side == 'red') or \
               (platform_side == 'red' and track_side == 'blue'):
                enemy_tracks.append(track)
        
        return enemy_tracks
    
    # ==================== 距离和几何计算 ====================
    
    def calculate_distance_lla(self, pos1: Tuple[float, float, float], 
                              pos2: Tuple[float, float, float]) -> float:
        """计算两个LLA坐标之间的距离（米）"""
        lat1, lon1, alt1 = pos1
        lat2, lon2, alt2 = pos2
        
        # 转换为弧度
        lat1_rad = math.radians(lat1)
        lon1_rad = math.radians(lon1)
        lat2_rad = math.radians(lat2)
        lon2_rad = math.radians(lon2)
        
        # 地球半径（米）
        R = 6371000
        
        # Haversine公式
        dlat = lat2_rad - lat1_rad
        dlon = lon2_rad - lon1_rad
        a = math.sin(dlat/2)**2 + math.cos(lat1_rad) * math.cos(lat2_rad) * math.sin(dlon/2)**2
        c = 2 * math.atan2(math.sqrt(a), math.sqrt(1-a))
        
        # 水平距离
        horizontal_distance = R * c
        
        # 考虑高度差
        altitude_diff = alt2 - alt1
        
        # 三维距离
        distance_3d = math.sqrt(horizontal_distance**2 + altitude_diff**2)
        
        return distance_3d
    
    def calculate_distance_wcs(self, pos1: Tuple[float, float, float], 
                              pos2: Tuple[float, float, float]) -> float:
        """计算两个WCS坐标之间的距离（米）"""
        x1, y1, z1 = pos1
        x2, y2, z2 = pos2
        
        return math.sqrt((x2-x1)**2 + (y2-y1)**2 + (z2-z1)**2)
    
    def get_distance_between_platforms(self, id1: int, id2: int, 
                                     coordinate_system: str = 'lla') -> Optional[float]:
        """计算两个平台之间的距离"""
        if coordinate_system == 'lla':
            pos1 = self.get_platform_location_lla(id1)
            pos2 = self.get_platform_location_lla(id2)
            if pos1 and pos2:
                return self.calculate_distance_lla(pos1, pos2)
        elif coordinate_system == 'wcs':
            pos1 = self.get_platform_location_wcs(id1)
            pos2 = self.get_platform_location_wcs(id2)
            if pos1 and pos2:
                return self.calculate_distance_wcs(pos1, pos2)
        
        return None
    def get_formation_centroid(self, side: str, coordinate_system: str = 'lla') -> Optional[Tuple[float, float, float]]:
        """计算指定阵营的编队质心坐标
        
        Parameters
        ----------
            side
                阵营名称（blue/red/neutral）
            coordinate_system
                坐标系选择，'lla'（经纬度）或 'wcs'（笛卡尔）
        
        Returns
        -------
            质心坐标元组，无有效平台时返回 None
        """
        platforms = self._platforms_by_side.get(side, [])
        if not platforms:
            return None
        
        if coordinate_system == 'lla':
            sum_lat = sum_lon = sum_alt = 0.0
            count = 0
            for p in platforms:
                lla = p.get('locationLLA')
                if lla and len(lla) >= 3:
                    sum_lat += lla[0]
                    sum_lon += lla[1]
                    sum_alt += lla[2]
                    count += 1
            if count == 0:
                return None
            return (sum_lat / count, sum_lon / count, sum_alt / count)
        else:
            sum_x = sum_y = sum_z = 0.0
            count = 0
            for p in platforms:
                wcs = p.get('locationWCS')
                if wcs and len(wcs) >= 3:
                    sum_x += wcs[0]
                    sum_y += wcs[1]
                    sum_z += wcs[2]
                    count += 1
            if count == 0:
                return None
            return (sum_x / count, sum_y / count, sum_z / count)
    
    # ==================== 统计信息 ====================
    
    def get_side_statistics(self) -> Dict[str, int]:
        """获取各阵营平台数量统计"""
        stats = {}
        for side, platforms in self._platforms_by_side.items():
            stats[side] = len(platforms)
        return stats
    
    def get_type_statistics(self) -> Dict[str, int]:
        """获取各类型平台数量统计"""
        stats = {}
        for platform_type, platforms in self._platforms_by_type.items():
            stats[platform_type] = len(platforms)
        return stats
    
    def get_spatial_domain_statistics(self) -> Dict[str, int]:
        """获取各空间域平台数量统计"""
        stats = {}
        for platform in self.platforms:
            domain = platform.get('spatialDomain', 'unknown')
            stats[domain] = stats.get(domain, 0) + 1
        return stats
    
    def get_track_statistics(self) -> Dict[str, int]:
        """获取航迹统计信息"""
        all_tracks = self.get_all_tracks()
        stats = {
            'total_tracks': len(all_tracks),
            'blue_tracks': len([t for t in all_tracks if t.get('side') == 'blue']),
            'red_tracks': len([t for t in all_tracks if t.get('side') == 'red']),
            'unknown_tracks': len([t for t in all_tracks if t.get('side') == 'unknown'])
        }
        return stats
    
    # ==================== 威胁评估 ====================
    
    def get_threats_for_platform(self, unique_id: int, max_distance: float = None) -> List[Dict[str, Any]]:
        """获取对指定平台的威胁列表"""
        platform = self.get_platform_by_id(unique_id)
        if not platform:
            return []
        
        platform_side = platform.get('side')
        threats = []
        
        for other_platform in self.platforms:
            other_side = other_platform.get('side')
            other_id = other_platform.get('uniqueId')
            
            # 跳过自己和同阵营
            if other_id == unique_id or other_side == platform_side:
                continue
            
            # 计算距离
            distance = self.get_distance_between_platforms(unique_id, other_id)
            if distance is None:
                continue
                
            if max_distance and distance > max_distance:
                continue
            
            # 添加到威胁列表
            threat_info = copy.deepcopy(other_platform)
            threat_info['distance'] = distance
            threats.append(threat_info)
            
        # 按距离排序
        threats.sort(key=lambda x: x['distance'])
        return threats
    
    def get_global_threat_matrix(self, max_distance: float = None,
                                 coordinate_system: str = 'lla') -> List[Dict[str, Any]]:
        """获取全局威胁矩阵（所有异阵营平台两两之间的威胁关系）
        
        Parameters
        ----------
            max_distance
                最大距离阈值（米），超过则不纳入
            coordinate_system
                坐标系选择，'lla' 或 'wcs'
        
        Returns
        -------
            按距离升序排列的威胁关系列表，每条包含平台双方信息与距离
        """
        matrix = []
        platform_ids = [p.get('uniqueId') for p in self.platforms if p.get('uniqueId') is not None]
        
        for i, id1 in enumerate(platform_ids):
            for id2 in platform_ids[i+1:]:
                platform1 = self._platforms_by_id.get(id1, {})
                platform2 = self._platforms_by_id.get(id2, {})
                
                if platform1.get('side') == platform2.get('side'):
                    continue
                
                distance = self.get_distance_between_platforms(id1, id2, coordinate_system)
                if distance is None:
                    continue
                
                if max_distance is not None and distance > max_distance:
                    continue
                
                matrix.append({
                    'platform_1_id': id1,
                    'platform_1_name': platform1.get('name'),
                    'platform_1_side': platform1.get('side'),
                    'platform_2_id': id2,
                    'platform_2_name': platform2.get('name'),
                    'platform_2_side': platform2.get('side'),
                    'distance': distance,
                })
        
        matrix.sort(key=lambda x: x['distance'])
        return matrix
    
    # ==================== 缓存管理 ====================
    
    def invalidate_cache(self):
        """使所有内部缓存失效并重建，适用于态势数据更新后的刷新场景"""
        self._platforms_by_side = {}
        self._platforms_by_type = {}
        self._platforms_by_id = {}
        self._platforms_by_name = {}
        self._tracks_cache = None
        self._commander_relations_by_side = {}
        self._root_commanders_by_side = {}
        self._parent_by_id = {}
        self._group_roots_by_side = {}
        self._build_cache()
        self._build_command_chain()
