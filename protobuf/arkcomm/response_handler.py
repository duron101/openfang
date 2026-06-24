from collections import deque
import uuid
import zmq
import json
import traceback
import time
import threading
import copy
from threading import Thread, Event
from queue import Queue, Empty
from concurrent.futures import ThreadPoolExecutor
import multiprocessing
import logging
# 消息类型常量映射
MESSAGE_KEYS = {
    "cmd": "command",
    "code": "command",  # 兼容文档中提到的code字段
    "customizedsituation": "customizedsituation",
    "state": "customizedsituation",  # 兼容文档中提到的state字段
    "progressValue": "progress",
    "scenarios": "scenarios",
    "situation": "customizedsituation"  # 兼容文档中提到的situation字段
}

# 消息处理状态常量
MESSAGE_STATUS = {
    "PENDING": 0,    # 待处理
    "PROCESSING": 1, # 处理中
    "COMPLETED": 2,  # 处理完成
    "FAILED": 3      # 处理失败
}

class SerializableLock:
    """可序列化的锁对象，用于在多进程环境中替代threading.Lock"""
    def __init__(self):
        self._lock = multiprocessing.Lock()
        
    def __getstate__(self):
        # 在序列化时返回空字典，因为multiprocessing.Lock已经是可序列化的
        return {}
        
    def __setstate__(self, state):
        # 在反序列化时创建新的multiprocessing.Lock
        self._lock = multiprocessing.Lock()
    
    def acquire(self, *args, **kwargs):
        # 将'blocking'参数转换为'block'参数，因为multiprocessing.Lock使用'block'而不是'blocking'
        if 'blocking' in kwargs:
            kwargs['block'] = kwargs.pop('blocking')
        return self._lock.acquire(*args, **kwargs)
    
    def release(self):
        return self._lock.release()
    
    def __enter__(self):
        self._lock.acquire()
        return self
    
    def __exit__(self, exc_type, exc_val, exc_tb):
        self._lock.release()

class MessageQueue:
    """线程安全的消息队列，用于存储特定类型的消息 - 增强版本
    添加了队列大小监控、自动清理和内存优化功能
    """
    def __init__(self, max_size=1000, buffer_size=1, min_put_interval=0.0001):
        self.queue = Queue(maxsize=max_size)
        self.lock = SerializableLock()
        self.buffer_size = buffer_size  # 保留最新的几条消息
        self._last_put_time = 0  # 上次添加消息的时间
        self._min_put_interval = min_put_interval  # 最小添加间隔（秒）
        self._latest_message = None  # 缓存最新消息，减少锁竞争
        self._latest_message_time = 0  # 最新消息的时间戳
        self._message_count = 0  # 消息计数器，用于监控队列大小
        self._last_cleanup_time = time.time()  # 上次清理时间
        self._cleanup_interval = 3.0  # 清理间隔（秒）
    
    def put(self, message):
        """添加消息到队列，保留最新的buffer_size条消息"""
        current_time = time.time()
        
        # 节流控制：如果距离上次添加时间太短，更新最新消息但不加入队列
        if current_time - self._last_put_time < self._min_put_interval:
            # 只更新最新消息缓存，不加入队列
            self._latest_message = message  # 避免深拷贝，减少内存开销
            self._latest_message_time = current_time
            return
            
        # 更新上次添加时间
        self._last_put_time = current_time
        
        # 定期清理检查 - 避免队列无限增长
        if current_time - self._last_cleanup_time > self._cleanup_interval:
            self._perform_cleanup()
            self._last_cleanup_time = current_time
        
        # 使用阻塞方式获取锁，确保消息不丢失
        # 设置超时防止死锁，虽然理论上不应该发生
        if not self.lock.acquire(timeout=1.0):
            print("Warning: MessageQueue lock acquire timeout, message dropped")
            return
            
        try:
            # 如果队列中的消息数量超过buffer_size，则移除旧消息
            while self.queue.qsize() >= self.buffer_size:
                try:
                    self.queue.get_nowait()
                    self._message_count = max(0, self._message_count - 1)  # 更新计数器
                except Empty:
                    break
            # 添加新消息
            self.queue.put(message)  # 避免深拷贝，减少内存开销
            self._message_count += 1  # 更新计数器
            # 同时更新最新消息缓存
            self._latest_message = message  # 避免深拷贝，减少内存开销
            self._latest_message_time = current_time
        finally:
            self.lock.release()
            
    def _perform_cleanup(self):
        """执行队列清理，防止内存泄漏"""
        # 尝试获取锁，但不阻塞
        if not self.lock.acquire(blocking=False):
            return  # 如果无法获取锁，跳过此次清理
            
        try:
            # 检查队列大小，如果超过阈值，强制清理到只保留最新的消息
            current_size = self.queue.qsize()
            if current_size > self.buffer_size * 2:  # 如果队列大小超过缓冲区大小的两倍
                # 保留最新的buffer_size条消息
                temp_messages = []
                # 先获取所有消息
                while not self.queue.empty():
                    try:
                        msg = self.queue.get_nowait()
                        if msg is not None:
                            temp_messages.append(msg)
                    except Empty:
                        break
                
                # 只保留最新的buffer_size条消息
                keep_count = min(len(temp_messages), self.buffer_size)
                for i in range(max(0, len(temp_messages) - keep_count), len(temp_messages)):
                    self.queue.put(temp_messages[i])
                
                # 更新计数器
                self._message_count = keep_count
        finally:
            self.lock.release()
    
    def get(self, block=True, timeout=None):
        """从队列获取消息，并将该消息从队列中移除"""
        # 先检查队列是否为空，避免不必要的锁操作
        if self.queue.empty() and not block:
            return None
            
        try:
            # 直接使用Queue的线程安全方法，不需要额外的锁
            return self.queue.get(block=block, timeout=timeout)
        except Empty:
            return None
        except Exception as e:
            # 记录异常但不抛出，返回None
            print(f"获取消息时发生异常: {e}")
            return None
    
    def get_nowait(self):
        """非阻塞方式获取消息，并将该消息从队列中移除"""
        try:
            # 直接使用Queue的线程安全方法，不需要额外的锁
            return self.queue.get_nowait()
        except Empty:
            return None
    
    def empty(self):
        """检查队列是否为空"""
        # 直接使用Queue的线程安全方法，不需要额外的锁
        return self.queue.empty()
    
    def size(self):
        """获取队列中消息数量"""
        # 直接使用Queue的线程安全方法，不需要额外的锁
        return self.queue.qsize()
    
    def clear(self):
        """清空队列"""
        # 使用非阻塞方式获取锁
        if not self.lock.acquire(blocking=False):
            return  # 如果无法立即获取锁，放弃此次操作
            
        try:
            while not self.queue.empty():
                try:
                    self.queue.get_nowait()
                except Empty:
                    break
        finally:
            self.lock.release()

class ResponseHandler(Thread):
    """响应处理器，负责接收消息并根据类型分发到对应队列"""
    
    def __init__(self,  # 共享主线程的 Context
            service_address,
            socket_id,  # 服务地址
            operation_name, result_queue, request_id=None):
        super().__init__()
        self.context = zmq.Context()  # 传入全局 zmq.Context（线程安全）
        self.service_address = service_address  # 服务地址（如 "tcp://
        self.operation_name = operation_name
        self.socket = None
        from arkcore.logging import setup_logger
        self.logger = setup_logger('ResponseHandler','ResponseHandler',True)
        self.result_queue = result_queue  # 兼容原有接口的结果队列
        self._uuid = None  # UUID缓存变量
        self.uuid_event = threading.Event()  # 用于等待UUID的事件
        
        # 添加Socket操作命令队列和结果队列
        # 确保socket_lock是可序列化的
        self.socket_lock = threading.Lock()
        self.response_queue=MessageQueue()
        self.daemon = True
        self.request_id = request_id
        self.stop_event = Event()
        
        # 创建原始消息队列，用于存储接收到的原始消息，必须保证不丢消息
        self.raw_message_queue = MessageQueue(max_size=5000, buffer_size=5000, min_put_interval=0)
        
        # 创建各类型消息的专用队列
        self.sim_command_queue = deque(maxlen=100)# 命令缓存，主线程发送命令到子线程到仿真
        self.command_queue = MessageQueue(max_size=1000, buffer_size=1000, min_put_interval=0) # 关键命令不能丢
        self.customizedsituation_queue = MessageQueue(max_size=100, buffer_size=1) # 态势只保留最新
        self.progress_queue = MessageQueue(max_size=100, buffer_size=1) # 进度只保留最新
        self.scenarios_queue = MessageQueue(max_size=100, buffer_size=100, min_put_interval=0)
        self.socket_id = socket_id
        # 消息队列映射，用于快速查找
        self.message_queues = {
            "command": self.command_queue,
            "customizedsituation": self.customizedsituation_queue,
            "progress": self.progress_queue,
            "scenarios": self.scenarios_queue
        }

        # 接收超时设置
        self.polling_timeout = 100  # 0.1秒
        self.total_timeout = 5.0    # 总超时5秒
        self.start_time = time.time()
        
        # 创建消息处理线程
        self.processor_thread = Thread(target=self._process_messages,name='_process_messages', daemon=True)
        self.processor_stop_event = Event()
        self.logger.debug('ResponseHandler初始化完成')
        
    def run(self):
        """线程主函数，负责接收消息并分发到对应队列。

        此方法启动ZeroMQ socket连接，开启消息处理线程，并进入命令处理循环。
        它处理发送、接收、设置路由ID和关闭命令。
        """
        try:
            self.socket = self.context.socket(zmq.DEALER)
            self.socket.setsockopt(zmq.LINGER, 0)  # 关闭时不阻塞
            self.socket.setsockopt(zmq.ROUTING_ID,self.socket_id.encode())  # 设置 ROUTING_ID
            # 2. 在线程内连接服务（确保 connect 与后续 recv 在同一线程）
            self.socket.connect(self.service_address)
            self.logger.info(f"线程内成功连接服务: {self.service_address} (SocketID: {self.socket_id}) (线程ID: {threading.get_ident()})")
                
            self.logger.debug(f"开始等待{self.operation_name}响应 [请求ID:{self.request_id}]")
            # 启动消息处理线程
            self.processor_thread.start()
            # 添加命令处理循环
            while not self.stop_event.is_set():
                try:
                    # 检查是否有命令需要处理
                    if len(self.sim_command_queue) > 0:
                        command = self.sim_command_queue.pop()
                        if command['type'] == 'send':
                            result = self._handle_send_command(command)
                            self.response_queue.put(result)
                        elif command['type'] == 'recv':
                            result = self._handle_recv_command(command)
                            self.response_queue.put(result)
                        elif command['type'] == 'set_routing_id':
                            result = self._handle_set_routing_id(command)
                            self.response_queue.put(result)
                        elif command['type'] == 'close':
                            self._handle_close_command()
                            break

                except Empty:
                    pass
                
                # 处理消息接收
                self._receive_messages()
                
        except Exception as e:
            self.logger.error(f"响应接收错误: {e}")
            self.logger.error(traceback.format_exc())
            # 出错时放入False作为结果（兼容原有接口）
            self.result_queue.put(False)
        finally:
            # 停止处理线程
            self.logger.debug('开始停止处理线程')
            self.processor_stop_event.set()
            
            # 等待处理线程结束
            if self.processor_thread.is_alive():
                self.processor_thread.join(timeout=1.0)
            # 清理自身引用，帮助垃圾回收
            self.socket = None
            self.logger.debug(f"结束响应接收 [请求ID:{self.request_id}]")
            
    def stop(self):
        """停止处理线程"""
        self.stop_event.set()
        self.processor_stop_event.set()
        self.logger.debug("线程停止")

    def _receive_messages(self):
        """接收消息并存储到原始消息队列，不进行处理"""
        poller = zmq.Poller()
        
        try:
            with self.socket_lock:
                poller.register(self.socket, zmq.POLLIN)
            
            try:
                socks = dict(poller.poll(self.polling_timeout)) 
            except zmq.ZMQError as e:
                self.logger.error(f"ZeroMQ轮询错误：{e}，错误码={e.errno}")  # 捕获ZMQError
            except Exception as e:
                self.logger.error(f"轮询未知异常：{str(e)}，类型={type(e)}")  # 捕获其他异常
                            
            if self.socket in socks and socks[self.socket] == zmq.POLLIN:
                with self.socket_lock:
                    #self.logger.debug('开始接收消息')
                    frames = self.socket.recv_multipart()
                    #self.logger.debug('接收消息完成')
                if not frames:
                    return
                    
                # 解析消息
                reply = frames[-1].decode('utf-8')
                message = json.loads(reply)
                
                # 将原始消息放入队列，由处理线程负责分类
                self.raw_message_queue.put(copy.deepcopy(message))
                

        except Exception as e:
            self.logger.error(f"消息接收错误: {e}")
            raise
        finally:
            # 清理poller
            try:
                with self.socket_lock:
                    poller.unregister(self.socket)
                    self.logger.debug('poller清理完成')
            except:
                pass
            
    def _handle_set_routing_id(self, command):
        """处理设置UUID路由ID的命令"""
        try:
            uuid = command['uuid']
            with self.socket_lock:
                self.socket.setsockopt(zmq.ROUTING_ID, uuid.encode())
            self.logger.info(f"更新socket ID为仿真UUID: {uuid}")
            return {'status': 'success', 'uuid': uuid}
        except Exception as e:
            self.logger.error(f"配置socket失败: {e}")
            return {'status': 'error', 'message': str(e)}    
                
    def _process_messages(self):
        """从原始消息队列中获取消息并进行分类处理 - 增强版
        添加了内存管理和资源清理功能，减少长时间运行时的性能下降
        """
        message_count = 0  # 消息计数器，用于定期清理
        last_cleanup_time = time.time()  # 上次清理时间
        cleanup_interval = 60.0  # 清理间隔（秒）
        
        while not self.processor_stop_event.is_set():
            try:
                # 从原始消息队列获取消息，使用较短的超时时间
                message = self.raw_message_queue.get(block=True, timeout=0.05)
                if message is None:
                    # 队列为空，继续等待
                    continue
                if self._uuid is None and isinstance(message, dict):
                    self._uuid = message.get('data', {}).get('uuid',None)
                    if self._uuid:
                        self.uuid_event.set()  # 触发事件，通知等待的线程
                # 增加消息计数
                message_count += 1
                # 定期执行清理操作
                current_time = time.time()
                if current_time - last_cleanup_time > cleanup_interval or message_count > 1000:
                    # 清理各个队列
                    self.cleanup_queues()
                    last_cleanup_time = current_time
                    message_count = 0
                    
                # 检查消息是否为字典类型
                if not isinstance(message, dict):
                    # 简化日志，减少I/O操作
                    if message_count % 100 == 1:  # 只记录每100条非字典消息中的第一条
                        self.logger.warning(f"收到非字典类型消息: {type(message)}")
                    try:
                        # 尝试转换为字典
                        if isinstance(message, str):
                            message = json.loads(message)
                        elif hasattr(message, '__dict__'):
                            message = vars(message)
                        else:
                            # 无法转换，创建包装字典
                            message = {"raw_data": str(message)}
                    except Exception:
                        # 简化错误处理，减少日志输出
                        message = {"raw_data": str(message)[:100]}  # 只保留前100个字符
                
                # 确定消息类型并分发到对应队列
                message_type = self._determine_message_type(message)
                
                # 分发消息到对应队列
                self._dispatch_message(message_type, message)
                
                # 只在调试模式下记录队列状态，减少日志量
                if message_count % 1000 == 0:  # 每500条消息记录一次队列状态
                    self.logger.debug(f"队列状态 - command: {self.command_queue.size()}, "
                                   f"customizedsituation: {self.customizedsituation_queue.size()}, "
                                   f"progress: {self.progress_queue.size()}, "
                                   f"scenarios: {self.scenarios_queue.size()}")
                
            except Empty:
                # 队列超时，继续循环w
                continue
            except Exception as e:
                # 减少错误日志频率
                if message_count % 100 == 1:  # 只记录每100个错误中的第一个
                    self.logger.error(f"消息处理错误: {type(e).__name__}")
                # 继续处理下一条消息，不让异常中断处理线程
                
    def cleanup_queues(self):
        """清理所有消息队列，防止内存泄漏"""
        try:
            # 清理各个队列，只保留最新的消息
            self.command_queue._perform_cleanup()
            self.customizedsituation_queue._perform_cleanup()
            self.progress_queue._perform_cleanup()
            self.scenarios_queue._perform_cleanup()
            self.raw_message_queue._perform_cleanup()
            self.sim_command_queue.clear()
            # 释放不再需要的引用
            self._latest_message = None
        except Exception as e:
            self.logger.debug(f"队列清理出错: {e}")
            # 错误不影响主流程
    
    def _determine_message_type(self, message):
        """根据消息内容确定消息类型"""
        # 检查消息是否为None或非字典类型
        if message is None:
            self.logger.warning("收到空消息")
            return "command"  # 默认作为命令处理
        # 使用MESSAGE_KEYS常量映射快速判断消息类型
        if isinstance(message, dict):
            # 首先检查顶层键
            for key, msg_type in MESSAGE_KEYS.items():
                if key in message:
                    # self.logger.debug(f"识别到消息类型: {msg_type} (通过键: {key})")
                    return msg_type
            
            # 检查是否有嵌套的结构
            for key, value in message.items():
                # 检查嵌套的字典
                if isinstance(value, dict):
                    # 检查嵌套字典中的键
                    for nested_key, msg_type in MESSAGE_KEYS.items():
                        if nested_key in value:
                            # self.logger.debug(f"在嵌套结构中识别到消息类型: {msg_type} (通过键: {key}.{nested_key})")
                            # 将嵌套的值提取到顶层，以便后续处理
                            message[nested_key] = value[nested_key]
                            return msg_type
                    
                    # 特别检查progressValue，因为它很常见
                    if "progressValue" in value:
                        # self.logger.debug(f"在嵌套结构中识别到进度消息: {key}.progressValue")
                        # 将嵌套的progressValue提取到顶层，以便后续处理
                        message["progressValue"] = value["progressValue"]
                        return "progress"
                
                # 检查数据字段中可能包含的消息
                if key == "data" and isinstance(value, dict):
                    # 检查data字段中是否包含customizedsituation或state
                    if "customizedsituation" in value or "state" in value:
                        self.logger.debug(f"在data字段中识别到态势消息")
                        # 将data中的值提取到顶层
                        if "customizedsituation" in value:
                            message["customizedsituation"] = value["customizedsituation"]
                        if "state" in value:
                            message["state"] = value["state"]
                        return "customizedsituation"
                    # 检查data字段中是否包含code或cmd
                    if "code" in value or "cmd" in value:
                        self.logger.debug(f"在data字段中识别到命令消息")
                        # 将data中的值提取到顶层
                        if "code" in value:
                            message["code"] = value["code"]
                        if "cmd" in value:
                            message["cmd"] = value["cmd"]
                        return "command"
            
            # 检查是否包含特定的结构特征
            if "platforms" in message or "Weapons" in message:
                self.logger.debug(f"通过结构特征识别到态势消息")
                return "customizedsituation"
            # 检查是否包含situation字段
            if "situation" in message:
                self.logger.debug(f"识别到situation消息")
                return "customizedsituation"
        
        # 默认作为命令响应处理
        self.logger.debug(f"未识别的消息类型，默认作为命令响应处理")
        try:
            self.logger.info(f"未识别的消息类型，默认作为命令响应处理: {json.dumps(message, ensure_ascii=False, indent=2)[:500]}...")
        except:
            self.logger.info(f"未识别的消息类型且无法序列化，类型: {type(message)}")
        return "command"
    
    def _dispatch_message(self, message_type, message):
        """将消息分发到对应的队列"""
        if message_type in self.message_queues:
            self.message_queues[message_type].put(copy.deepcopy(message))
            # self.logger.debug(f"消息已分发到 {message_type} 队列")
        else:
            self.logger.warning(f"未找到消息类型 {message_type} 的队列")
    
    # 对外提供的消息获取接口
    def get_command(self, block=False, timeout=None):
        """获取命令消息，并从队列中移除该消息"""
        try:
            return self.command_queue.get(block=block, timeout=timeout)
        except Exception as e:
            self.logger.warning(f"获取命令消息失败: {e}")
            return None
    
    def get_customizedsituation(self, block=False, timeout=None):
        """获取自定义状态消息，并从队列中移除该消息"""
        try:
            return self.customizedsituation_queue.get(block=block, timeout=timeout)
        except Empty:
            raise
        except Exception as e:
            self.logger.warning(f"获取自定义状态消息失败: {e}")
            return None
    
    def get_progress(self, block=False, timeout=None):
        """获取进度消息，并从队列中移除该消息"""
        try:
            return self.progress_queue.get(block=block, timeout=timeout)
        except Exception as e:
            self.logger.warning(f"获取进度消息失败: {e}")
            return None
    
    def get_scenarios(self, block=False, timeout=None):
        """获取场景消息，并从队列中移除该消息"""
        try:
            return self.scenarios_queue.get(block=block, timeout=timeout)
        except Exception as e:
            self.logger.warning(f"获取场景消息失败: {e}")
            return None
    
    # 批量获取接口
    def get_all_commands(self, max_count=100):
        """获取所有可用的命令消息，最多max_count条，并从队列中移除这些消息"""
        return self._get_all_messages(self.command_queue, max_count)
    
    def get_all_customizedsituations(self, max_count=100):
        """获取所有可用的自定义状态消息，最多max_count条，并从队列中移除这些消息
        优化版本：直接操作队列，减少锁竞争
        """
        messages = []
        # 使用非阻塞方式快速获取消息
        try:
            # 直接获取最新的一条消息
            if not self.customizedsituation_queue.empty():
                with self.customizedsituation_queue.lock:
                    # 如果队列不为空，获取所有消息但只返回最新的一条
                    while not self.customizedsituation_queue.queue.empty() and len(messages) < max_count:
                        try:
                            msg = self.customizedsituation_queue.queue.get_nowait()
                            if msg is not None:
                                messages.append(msg)
                        except Empty:
                            break
        except Exception as e:
            # 简化错误处理，避免日志过多
            pass
            
        # 返回获取到的消息，如果没有消息则返回空列表
        return messages
    
    def get_all_progress(self, max_count=100):
        """获取所有可用的进度消息，最多max_count条，并从队列中移除这些消息"""
        return self._get_all_messages(self.progress_queue, max_count)
    
    def get_all_scenarios(self, max_count=100):
        """获取所有可用的场景消息，最多max_count条，并从队列中移除这些消息"""
        return self._get_all_messages(self.scenarios_queue, max_count)
    
    def _get_all_messages(self, queue, max_count):
        """从指定队列获取所有可用消息，最多max_count条，并从队列中移除这些消息"""
        messages = []
        for _ in range(max_count):
            msg = queue.get_nowait()
            if msg is None:
                break
            messages.append(msg)
        return messages
    
    # 队列状态查询接口
    def has_command(self):
        """检查是否有命令消息"""
        return not self.command_queue.empty()
    
    def has_customizedsituation(self):
        """检查是否有自定义状态消息"""
        return not self.customizedsituation_queue.empty()
    
    def has_progress(self):
        """检查是否有进度消息"""
        return not self.progress_queue.empty()
    
    def has_scenarios(self):
        """检查是否有场景消息"""
        return not self.scenarios_queue.empty()
    
    # 清空队列接口
    def clear_command_queue(self):
        """清空命令队列"""
        self.command_queue.clear()
    def clear_sim_command_queue(self):
        """清空命令队列"""
        self.sim_command_queue.clear()
    def clear_customizedsituation_queue(self):
        """清空自定义状态队列"""
        self.customizedsituation_queue.clear()
    
    def clear_progress_queue(self):
        """清空进度队列"""
        self.progress_queue.clear()
    
    def clear_scenarios_queue(self):
        """清空场景队列"""
        self.scenarios_queue.clear()
    
    def clear_all_queues(self):
        """清空所有队列"""
        self.clear_command_queue()
        self.clear_customizedsituation_queue()
        self.clear_progress_queue()
        self.clear_scenarios_queue()
        self.clear_sim_command_queue()
    def get_uuid(self, timeout=None):
        """获取UUID"""
        if not self.uuid_event.wait(timeout=timeout):
            return None
        return self._uuid

    def _handle_send_command(self, command):
        """处理发送命令"""
        try:
            with self.socket_lock:
                self.socket.send_string(command['data'])
            return {'success': True, 'error': None}
        except Exception as e:
            self.logger.error(f"发送消息错误: {e}")
            return {'success': False, 'error': str(e)}
    
    def _handle_recv_command(self, command):
        """处理接收命令"""
        try:
            with self.socket_lock:
                frames = self.socket.recv_multipart(flags=command.get('flags', 0))
            return {'success': True, 'data': frames, 'error': None}
        except Exception as e:
            self.logger.error(f"接收消息错误: {e}")
            return {'success': False, 'error': str(e)}
    
    def _handle_close_command(self):
        """处理关闭命令"""
        with self.socket_lock:
            self.socket.close()
        self.logger.info("Socket已关闭")
    
    # 添加对外接口方法
    def send(self, data):
        """主线程调用发送消息"""
        command = {'type': 'send', 'data': json.dumps(data)}
        self.sim_command_queue.append(command)
        return self.response_queue.get()
    
    def recv(self, flags=0):
        """主线程调用接收消息"""
        command = {'type': 'recv', 'flags': flags}
        self.sim_command_queue.append(command)
        return self.response_queue.get()
    
    def close(self):
        """主线程调用关闭连接"""
        command = {'type': 'close'}
        self.sim_command_queue.append(command)
        try:
            return self.response_queue.get(timeout=1.0)
        except Empty:
            return {'success': True}
        
    def send_command(self,json_message):
        command = {
                    'type': 'send',
                    'data': json.dumps(json_message)
                    }
        self.sim_command_queue.append(command)
        return True
    
    def send_routing_id_command(self,uuid_mesage):
        command = {
                    'type': 'set_routing_id',
                    'uuid': uuid_mesage
                    }
        self.sim_command_queue.append(command)
        return True