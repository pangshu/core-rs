//! 链路追踪（文档 三·8）：trace_id 由服务端生成（`middleware::request_id` 承担
//! 生成与注入），绑定到 tracing 根 span；跨服务链路可导出 OpenTelemetry
//! （feature = "otel"，OTLP gRPC）。
//!
//! OTel 配置走社区标准环境变量（不新增配置节）：
//! - `OTEL_EXPORTER_OTLP_ENDPOINT`（如 http://127.0.0.1:4317；缺省 = 不导出）；
//! - `OTEL_SERVICE_NAME`（缺省回落 `[log].service_name` → "core-rs"）。

#[cfg(feature = "otel")] // 仅在开启 otel feature 时引入下面的类型
use opentelemetry_sdk::trace::SdkTracerProvider; // OTel SDK 的 tracer provider

/// 持有 provider 保证存活，进程退出时 flush
#[cfg(feature = "otel")] // 仅在开启 otel feature 时定义
pub struct OtelGuard { // OTel 保活句柄
    provider: SdkTracerProvider, // 持有 provider，Drop 时触发 flush
}

#[cfg(feature = "otel")] // 仅在开启 otel feature 时实现
impl Drop for OtelGuard { // Drop 时刷出未发送的 span
    fn drop(&mut self) { // 析构逻辑
        let _ = self.provider.force_flush(); // 强制 flush，忽略错误避免析构 panic
    }
}

/// 读 OTel 标准环境变量
#[cfg(feature = "otel")] // 仅在开启 otel feature 时编译
fn env_endpoint() -> Option<String> { // 读取 OTLP 导出端点
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") // 读取标准环境变量
        .ok() // 未设置则 None
        .filter(|s| !s.is_empty()) // 空串视为未设置
}

/// 初始化 OTLP exporter 并返回可挂到订阅链上的 layer 与 guard。
/// 未设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 或 exporter 建立失败时返回 None
/// （降级纯本地输出）。
#[cfg(feature = "otel")] // 仅在开启 otel feature 时编译
pub(crate) fn setup_layer<S>( // 构造 OTel layer 与 guard
    service_name: &str, // 上报的服务名
) -> Option<( // 成功返回 (layer, guard) 二元组
    tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>, // 可挂到订阅链的 OTel layer
    OtelGuard, // 保活 guard
)>
where // 泛型约束
    S: tracing::Subscriber // S 必须是订阅者
        + for<'a> tracing_subscriber::registry::LookupSpan<'a> // 且支持 span 查找
        + Send // 可跨线程发送
        + Sync, // 可跨线程共享
{ // 函数体开始
    let endpoint = env_endpoint()?; // 未配置端点则直接返回 None（降级）
    use opentelemetry::trace::TracerProvider as _; // 引入 trait 以便调用 tracer()
    use opentelemetry_otlp::WithExportConfig; // 引入 trait 以使用 with_endpoint()

    let exporter = match opentelemetry_otlp::SpanExporter::builder() // 构造 OTLP span exporter
        .with_tonic() // 使用 tonic（gRPC）传输
        .with_endpoint(&endpoint) // 指定导出端点
        .build() // 构建 exporter
    { // 处理构建结果
        Ok(e) => e, // 构建成功
        Err(e) => { // 构建失败
            eprintln!("otel exporter init failed, tracing 降级为纯本地输出: {e}"); // 打印错误并降级
            return None;
        }
    };

    let provider = SdkTracerProvider::builder() // 构造 tracer provider
        .with_resource( // 设置资源属性
            opentelemetry_sdk::Resource::builder() // 构造资源
                .with_service_name(service_name.to_string()) // 设置服务名
                .build(), // 构建资源
        )
        .with_batch_exporter(exporter) // 挂载批量导出器
        .build(); // 构建 provider

    opentelemetry::global::set_tracer_provider(provider.clone()); // 注册为全局 provider

    let tracer = provider.tracer("core-rs"); // 取一个名为 core-rs 的 tracer
    let layer = tracing_opentelemetry::layer().with_tracer(tracer); // 用该 tracer 构造 OTel layer
    Some((layer, OtelGuard { provider })) // 返回 layer 与保活 guard
}

/// 非 otel 构建下的占位（保持调用点无 cfg 分支）
#[cfg(not(feature = "otel"))] // 未开启 otel feature 时编译
pub struct OtelGuard; // 空占位类型，使调用点无需 cfg 分支
