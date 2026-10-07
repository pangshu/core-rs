//! Redis Stream 队列后端（feature = "queue-redis"）：消费组 + ACK，at-least-once。
//!
//! - 发布：`XADD`（payload/headers 打包为单字段 JSON，跨后端无损）；
//! - 注册：`XGROUP CREATE ... MKSTREAM`（订阅即建组，注册与启动之间发布的消息不丢）；
//! - 消费：`XREADGROUP GROUP g c COUNT n BLOCK t STREAMS k1 k2 >`（同组多实例
//!   负载均衡分摊，异组各收一份）；每轮先 `XAUTOCLAIM` 接管闲置 pending
//!   （消费者宕机自愈，需 Redis 6.2+）；
//! - 确认：Worker 成功后 `XACK`；失败不 ACK，pending 里的消息闲置超过
//!   `claim_min_idle_secs` 被任意实例接手重投；
//! - 死信：重试耗尽的消息由 Worker 转发死信 topic 后 ACK；无死信 topic 时
//!   留在 pending 列表（XPENDING 可观测），人工处理后 XACK/XDEL。

use std::collections::{BTreeMap, HashSet}; // 引入有序映射与哈希集合，用于 headers 与 topic 去重
use std::sync::atomic::{AtomicBool, Ordering}; // 引入原子布尔与内存序，用于关闭标志
use std::sync::{Arc, Mutex}; // 引入共享所有权指针与互斥锁，保护内部状态

use redis::aio::ConnectionManager; // 引入异步连接管理器（断线自动重连）
use redis::AsyncCommands; // 引入异步命令 trait（xread_options 等需要）

use super::{Delivery, Queue, QueueError}; // 引入队列公共类型：投递体、trait 与错误
use crate::config::sections::QueueRedisSettings; // 引入 Redis 队列配置节

/// 消息打包字段名（XADD 拒绝全空 entry）
const FIELD: &str = "m"; // 固定字段名，payload/headers 整体 JSON 存该字段

struct State {
    topics: Vec<String>, // 已注册的完整 stream key 列表（消费侧直接用）
    known_groups: HashSet<String>, // 已建组的裸 topic 集合，publish 校验用
}

pub struct RedisQueue {
    conn: ConnectionManager, // Redis 连接管理器（可克隆、自动重连）
    group: String, // 消费组名，同组实例负载均衡
    consumer: String, // 消费者名，组内唯一标识
    key_prefix: String, // stream key 前缀
    block_secs: u64, // XREADGROUP BLOCK 阻塞秒数
    claim_min_idle_secs: u64, // 接管闲置 pending 的最小空闲秒数
    max_attempts: u64, // 最大投递次数，超过则不再接管
    batch: usize, // 每轮批量条数
    state: Arc<Mutex<State>>, // 共享可变状态（topic/组集合）
    closed: Arc<AtomicBool>, // 关闭标志，置位后 receive 直接返回空
}

impl RedisQueue {
    /// 建连即验证（坏地址在装配期报错，而非首条消息时）
    pub async fn new(settings: &QueueRedisSettings) -> Result<Self, QueueError> { // 异步构造：建立连接并填充配置
        let client = redis::Client::open(settings.url.as_str()) // 按 URL 创建 Redis 客户端
            .map_err(|e| QueueError::Backend(format!("bad redis url: {e}")))?; // URL 非法则转成后端错误返回
        // ConnectionManager：断线自动重连
        let conn = ConnectionManager::new(client) // 用客户端创建连接管理器
            .await // 等待连接建立
            .map_err(|e| QueueError::Backend(format!("redis connect failed: {e}")))?; // 连接失败即装配期报错
        let consumer = if settings.consumer.is_empty() { // 未显式配置消费者名时自动生成
            format!( // 拼接 host-pid-时间戳 作为唯一消费者名
                "{}-{}-{:x}", // 三段式格式：主机名-进程号-毫秒时间戳
                std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string()), // 取主机名，缺失回退 "host"
                std::process::id(), // 当前进程号，区分同机多实例
                crate::utils::time::now_ms() // 当前毫秒时间戳，避免同名
            )
        } else { // 配置了则直接使用
            settings.consumer.clone() // 克隆配置中的消费者名
        };
        Ok(Self { // 组装 RedisQueue 实例
            conn, // 连接管理器
            group: if settings.group.is_empty() { // 组名未配置时用默认值
                "core-rs".to_string() // 默认消费组名
            } else {
                settings.group.clone() // 使用配置的组名
            },
            consumer, // 前面算出的消费者名
            key_prefix: settings.key_prefix.clone(), // 配置的 key 前缀
            block_secs: settings.block_secs.max(1), // 阻塞秒数至少为 1
            claim_min_idle_secs: settings.claim_min_idle_secs.max(1), // 最小空闲秒数至少为 1
            max_attempts: settings.max_attempts.max(1), // 最大尝试次数至少为 1
            batch: settings.batch.clamp(1, 1000), // 批量大小限制在 1~1000
            state: Arc::new(Mutex::new(State { // 初始化共享状态
                topics: Vec::new(), // 初始无已注册 topic
                known_groups: HashSet::new(), // 初始无已建组
            })),
            closed: Arc::new(AtomicBool::new(false)), // 初始未关闭
        })
    }

    fn stream_key(&self, topic: &str) -> String { // 由 topic 拼出完整 stream key
        format!("{}{}", self.key_prefix, topic) // 前缀 + topic 直接拼接
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> { // 加锁并容忍 poison
        m.lock().unwrap_or_else(std::sync::PoisonError::into_inner) // 锁被 poison 时取出内部值，避免连锁 panic
    }

    /// 接管闲置 pending（宕机自愈，需 Redis 6.2+；失败只记 debug，不影响新消息消费）。
    /// 先 XPENDING 读 `times_delivered`：超过 `max_attempts` 的不再接管（留 pending
    /// 告警），否则 ack 失败的消息会无限重投。之后 XCLAIM 接管并取回 payload。
    async fn claim_idle( // 接管超过空闲阈值的 pending 消息
        &self, // 自身引用
        conn: &mut ConnectionManager, // 复用的连接管理器
        key: &str, // 目标 stream key
    ) -> Vec<(String, String)> { // 返回 (消息 id, payload) 列表
        let mut out = Vec::new(); // 结果收集容器
        let min_idle = self.claim_min_idle_secs * 1000; // 空闲阈值换算为毫秒
        let reply: Result<redis::Value, _> = redis::cmd("XPENDING") // 查询 pending 明细
            .arg(key) // stream key
            .arg(&self.group) // 消费组
            .arg("IDLE") // 按空闲时长过滤的扩展形式
            .arg(min_idle) // 最小空闲毫秒数
            .arg("-") // 起始 id 下界
            .arg("+") // 结束 id 上界
            .arg(self.batch) // 最多取 batch 条
            .query_async(conn) // 异步执行命令
            .await; // 等待结果
        let mut ids = Vec::new(); // 待接管的 id 列表
        match reply { // 解析 XPENDING 返回
            Ok(redis::Value::Array(entries)) => { // 正常返回数组
                for entry in entries { // 遍历每条 pending 记录
                    // 每条：[id, consumer, idle_ms, times_delivered]
                    if let redis::Value::Array(item) = entry { // 记录本身应是数组
                        if item.len() >= 4 { // 至少含 4 个字段才可用
                            let id = match &item[0] { // 取第 0 项作为消息 id
                                redis::Value::BulkString(d) => { // 字节串转字符串
                                    String::from_utf8_lossy(d).to_string() // 有损转 UTF-8
                                }
                                _ => continue, // 类型不符则跳过该条
                            };
                            let times_delivered = match &item[3] { // 取第 3 项为投递次数
                                redis::Value::Int(n) => *n as u64, // 整数转 u64
                                _ => 0, // 缺失则视为 0 次
                            };
                            if times_delivered > self.max_attempts { // 超过最大尝试次数
                                tracing::warn!( // 记警告，留待人工处理
                                    key = %key, // 关联 stream key
                                    id = %id, // 关联消息 id
                                    times_delivered, // 实际投递次数
                                    "pending message exceeded queue.redis.max_attempts, left for manual handling" // 告警文案
                                );
                                continue; // 不再接管该条，避免无限重投
                            }
                            ids.push(id); // 收进待接管列表
                        }
                    }
                }
            }
            Ok(_) => {} // 非数组（无 pending）视为无操作
            Err(e) => { // 命令失败（如老版本无 XPENDING 扩展）
                tracing::debug!(key = %key, error = %e, "XPENDING unavailable, skip idle reclaim"); // 仅记 debug，不影响新消息
                return out; // 直接返回空
            }
        }
        if ids.is_empty() { // 没有可接管的 id
            return out; // 提前返回
        }
        let mut cmd = redis::cmd("XCLAIM"); // 构造 XCLAIM 命令
        cmd.arg(key).arg(&self.group).arg(&self.consumer).arg(min_idle); // 依次传入 key、组、消费者、空闲阈值
        for id in &ids { // 逐个追加待接管的 id
            cmd.arg(id); // 追加一个 id
        }
        match cmd.query_async::<redis::Value>(conn).await { // 执行 XCLAIM 并解析
            Ok(redis::Value::Array(entries)) => { // 正常返回被接管的消息
                for entry in entries { // 遍历每条消息
                    if let redis::Value::Array(item) = entry { // 消息应为数组
                        if item.len() >= 2 { // 至少含 id 与字段值
                            let id = match &item[0] { // 取消息 id
                                redis::Value::BulkString(d) => String::from_utf8_lossy(d).to_string(), // 有损转字符串
                                _ => continue, // 类型不符跳过
                            };
                            if let Some(payload) = extract_field(&item[1], FIELD) { // 从字段数组中取 payload
                                out.push((id, payload)); // 收进结果
                            }
                        }
                    }
                }
            }
            Ok(_) => {} // 非数组视为无接管
            Err(e) => { // XCLAIM 失败
                tracing::debug!(key = %key, error = %e, "XCLAIM unavailable, skip idle reclaim") // 记 debug，下轮再试
            }
        }
        out // 返回接管的 (id, payload) 列表
    }
}

/// 从 XADD/XCLAIM 的 field-value 扁平数组中取指定字段
fn extract_field(fv: &redis::Value, field: &str) -> Option<String> { // 在 [field, value, ...] 中查字段值
    let fv = match fv { // 先取出内部数组
        redis::Value::Array(b) => b, // 是数组则解出引用
        _ => return None, // 非数组直接失败
    };
    for pair in fv.chunks(2) { // 每两个元素为一组 field/value
        if pair.len() == 2 { // 完整成对才处理
            if let redis::Value::BulkString(d) = &pair[0] { // 字段名是字节串
                if d == field.as_bytes() { // 字段名匹配目标
                    return match &pair[1] { // 返回对应值
                        redis::Value::BulkString(v) => Some(String::from_utf8_lossy(v).to_string()), // 有损转字符串
                        _ => None, // 值类型不符返回 None
                    };
                }
            }
        }
    }
    None // 未找到字段
}

#[async_trait::async_trait] // 为异步 trait 实现提供宏支持
impl Queue for RedisQueue { // 为 RedisQueue 实现统一队列接口
    fn name(&self) -> &'static str { // 后端名称
        "redis" // 固定标识
    }

    async fn register(&self, topic: &str) -> Result<(), QueueError> { // 注册 topic 并建消费组
        let key = self.stream_key(topic); // 拼出完整 stream key
        let mut conn = self.conn.clone(); // 克隆连接管理器
        // XGROUP CREATE ... MKSTREAM；BUSYGROUP = 组已存在，幂等
        let res: Result<String, _> = redis::cmd("XGROUP") // 建组命令
            .arg("CREATE") // 子命令：创建组
            .arg(&key) // 目标 stream key
            .arg(&self.group) // 组名
            .arg("$") // 从最新消息开始（仅消费新消息）
            .arg("MKSTREAM") // 流不存在时自动创建
            .query_async(&mut conn) // 异步执行
            .await; // 等待结果
        match res { // 处理建组结果
            Ok(_) => {} // 成功即完成
            Err(e) => { // 失败时判断是否为组已存在
                let msg = e.to_string(); // 错误转字符串
                if !msg.contains("BUSYGROUP") { // 非「组已存在」才是真错误
                    return Err(QueueError::Backend(format!("XGROUP CREATE failed: {msg}"))); // 返回后端错误
                }
            }
        }
        let mut st = Self::lock(&self.state); // 加锁访问共享状态
        if !st.topics.contains(&key) { // 尚未登记该 stream key
            // 存全名 stream key：消费侧（receive）直接拿它 XAUTOCLAIM/XREADGROUP——
            // 此前存裸 topic，组建立在带前缀的 key 上而消费读裸 key，NOGROUP 静默空转
            st.topics.push(key.clone()); // 登记完整 key，保证消费侧一致
        }
        st.known_groups.insert(topic.to_string()); // 记录裸 topic 供 publish 校验
        Ok(())
    }

    async fn publish( // 发布消息到指定 topic
        &self, // 自身引用
        topic: &str, // 目标 topic
        payload: serde_json::Value, // 消息体
        headers: BTreeMap<String, String>, // 消息头
    ) -> Result<String, QueueError> { // 返回消息 id
        if !Self::lock(&self.state).known_groups.contains(topic) { // 未注册的 topic 不允许发布
            return Err(QueueError::NoHandler(topic.to_string())); // 返回无处理器错误
        }
        let body = serde_json::json!({ "payload": payload, "headers": headers }); // 打包为单字段 JSON
        let mut conn = self.conn.clone(); // 克隆连接管理器
        let id: String = redis::cmd("XADD") // 追加消息命令
            .arg(self.stream_key(topic)) // 目标 stream key
            .arg("*") // 由服务端自动生成 id
            .arg(FIELD) // 固定字段名
            .arg(body.to_string()) // JSON 字符串作为值
            .query_async(&mut conn) // 异步执行
            .await // 等待结果
            .map_err(|e| QueueError::Backend(format!("XADD failed: {e}")))?; // 失败转后端错误
        Ok(id) // 返回服务端分配的 id
    }

    async fn receive(&self, max: usize) -> Result<Vec<Delivery>, QueueError> { // 拉取一批消息
        if self.closed.load(Ordering::SeqCst) { // 已关闭则不再拉取
            return Ok(Vec::new()); // 返回空批
        }
        let (keys, ) = { // 快照当前所有 stream key
            let st = Self::lock(&self.state); // 加锁读状态
            (st.topics.clone(),) // 克隆一份 key 列表后释放锁
        };
        if keys.is_empty() { // 尚无注册 topic
            return Ok(Vec::new()); // 返回空批
        }

        let mut conn = self.conn.clone(); // 克隆连接管理器
        let mut out = Vec::new(); // 结果收集

        // 1) 接管闲置 pending（宕机自愈）。claim 已把消息所有权转到本消费者，
        //    不能因超出 max 而 truncate 丢弃——全部交给 worker 处理
        for key in &keys { // 逐个 stream 接管闲置消息
            let topic = key // 由完整 key 还原裸 topic
                .trim_start_matches(&self.key_prefix) // 去掉前缀
                .to_string(); // 转拥有所有权的 String
            for (id, payload) in self.claim_idle(&mut conn, key).await { // 接管该 key 的闲置 pending
                if let Some(msg) = decode_message(&id, &topic, &payload) { // 解析消息体
                    out.push(Delivery { // 组装投递体
                        ack_token: id, // ack 令牌为消息 id
                        message: msg, // 解析后的消息
                    });
                }
            }
        }
        if !out.is_empty() { // 有接管的闲置消息优先返回
            return Ok(out); // 直接返回，避免与新消息混淆
        }

        // 2) XREADGROUP 新消息（BLOCK 超时即本轮空）
        let opts = redis::streams::StreamReadOptions::default() // 构造读取选项
            .group(&self.group, &self.consumer) // 指定消费组与消费者
            .count(max.max(1)) // 每轮最多 max 条
            .block((self.block_secs * 1000) as usize); // 阻塞毫秒数
        let reply: Result<redis::streams::StreamReadReply, _> = conn // 发起 XREADGROUP
            .xread_options::<_, _, redis::streams::StreamReadReply>(&keys, &[">"], &opts) // 只读新消息（>）
            .await; // 等待结果
        match reply { // 处理读取结果
            Ok(data) => { // 正常返回
                for stream in data.keys { // 遍历各 stream 的返回
                    let topic = stream // 由 key 还原裸 topic
                        .key
                        .trim_start_matches(&self.key_prefix) // 去前缀
                        .to_string(); // 转 String
                    for entry in stream.ids { // 遍历每条消息
                        let payload = entry.map.get(FIELD).and_then(|v| match v { // 取固定字段值
                            redis::Value::BulkString(d) => Some(String::from_utf8_lossy(d).to_string()), // 字节串转字符串
                            _ => None, // 类型不符为 None
                        });
                        if let Some(payload) = payload { // 成功取到 payload
                            if let Some(msg) = decode_message(&entry.id, &topic, &payload) { // 解析消息
                                out.push(Delivery { // 组装投递体
                                    ack_token: entry.id, // ack 令牌为消息 id
                                    message: msg, // 解析后的消息
                                });
                            }
                        }
                    }
                }
            }
            // NOGROUP（流被删/组未建完）等错误：记日志，下一轮 register 重建
            Err(e) => { // 读取失败
                tracing::warn!(error = %e, "XREADGROUP failed"); // 记警告，下轮重试
            }
        }
        Ok(out) // 返回本批结果
    }

    async fn ack(&self, delivery: &Delivery) -> Result<(), QueueError> { // 确认消息已处理
        if delivery.ack_token.is_empty() { // 无令牌（如自接管场景）无需确认
            return Ok(()); // 直接成功
        }
        let key = self.stream_key(&delivery.message.topic); // 由消息 topic 还原 stream key
        let mut conn = self.conn.clone(); // 克隆连接管理器
        let _: i64 = redis::cmd("XACK") // 确认命令
            .arg(key) // stream key
            .arg(&self.group) // 消费组
            .arg(&delivery.ack_token) // 消息 id
            .query_async(&mut conn) // 异步执行
            .await // 等待结果
            .map_err(|e| QueueError::Backend(format!("XACK failed: {e}")))?; // 失败转后端错误
        Ok(())
    }

    async fn nack(&self, _delivery: &Delivery) -> Result<(), QueueError> { // 否定确认：不 ACK
        // 不 ACK：消息留在 pending，闲置后由任意实例 XAUTOCLAIM 接管；
        // Worker 重试耗尽时若配置了死信 topic 会先转发再 ACK
        Ok(()) // 空实现，交由 pending 机制重投
    }

    async fn close(&self) -> Result<(), QueueError> { // 关闭后端
        self.closed.store(true, Ordering::SeqCst); // 置关闭标志，receive 立即返回空
        Ok(())
    }

    async fn ping(&self) -> Result<(), QueueError> { // 健康检查：探活 Redis
        let mut conn = self.conn.clone(); // 克隆连接管理器
        redis::cmd("PING") // PING 命令
            .query_async::<()>(&mut conn) // 异步执行
            .await // 等待结果
            .map_err(|e| QueueError::Backend(e.to_string()))?; // 失败转后端错误
        Ok(())
    }
}

fn decode_message(id: &str, topic: &str, payload: &str) -> Option<crate::queue::Message> { // 反序列化消息体
    #[derive(serde::Deserialize)] // 派生反序列化实现
    struct Body { // 消息体结构（与 publish 打包格式对应）
        payload: serde_json::Value, // 业务载荷
        #[serde(default)] // headers 缺失时用默认空表
        headers: BTreeMap<String, String>, // 消息头
    }
    let body: Body = match serde_json::from_str(payload) { // 解析 JSON
        Ok(b) => b, // 解析成功
        Err(e) => { // 解析失败：消息损坏
            tracing::error!(id = %id, topic = %topic, error = %e, "queue message corrupt, dropped"); // 记错误并丢弃
            return None; // 返回 None
        }
    };
    let mut msg = crate::queue::Message::new(topic, body.payload); // 构造框架消息对象
    msg.id = id.to_string(); // 回填消息 id
    msg.headers = body.headers; // 回填消息头
    Some(msg) // 返回消息
}
