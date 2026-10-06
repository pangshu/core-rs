//! 链路追踪（文档 三·8）：trace_id 由服务端生成（`middleware::request_id` 承担
//! 生成与注入），绑定到 tracing 根 span；跨服务链路可导出 OpenTelemetry
//! （feature = "otel"，OTLP gRPC）。
//!
//! OTel 配置走社区标准环境变量（不新增配置节）：
//! - `OTEL_EXPORTER_OTLP_ENDPOINT`（如 http://127.0.0.1:4317；缺省 = 不导出）；
//! - `OTEL_SERVICE_NAME`（缺省回落 `[log].service_name` → "core-rs"）。

#[cfg(feature = "otel")]
use opentelemetry_sdk::trace::SdkTracerProvider;

/// 持有 provider 保证存活，进程退出时 flush
#[cfg(feature = "otel")]
pub struct OtelGuard {
    provider: SdkTracerProvider,
}

#[cfg(feature = "otel")]
impl Drop for OtelGuard {
    fn drop(&mut self) {
        let _ = self.provider.force_flush();
    }
}

/// 读 OTel 标准环境变量
#[cfg(feature = "otel")]
fn env_endpoint() -> Option<String> {
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())
}

/// 初始化 OTLP exporter 并返回可挂到订阅链上的 layer 与 guard。
/// 未设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 或 exporter 建立失败时返回 None
/// （降级纯本地输出）。
#[cfg(feature = "otel")]
pub(crate) fn setup_layer<S>(
    service_name: &str,
) -> Option<(
    tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::SdkTracer>,
    OtelGuard,
)>
where
    S: tracing::Subscriber
        + for<'a> tracing_subscriber::registry::LookupSpan<'a>
        + Send
        + Sync,
{
    let endpoint = env_endpoint()?;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&endpoint)
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("otel exporter init failed, tracing 降级为纯本地输出: {e}");
            return None;
        }
    };

    let provider = SdkTracerProvider::builder()
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(service_name.to_string())
                .build(),
        )
        .with_batch_exporter(exporter)
        .build();

    opentelemetry::global::set_tracer_provider(provider.clone());

    let tracer = provider.tracer("core-rs");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    Some((layer, OtelGuard { provider }))
}

/// 非 otel 构建下的占位（保持调用点无 cfg 分支）
#[cfg(not(feature = "otel"))]
pub struct OtelGuard;
