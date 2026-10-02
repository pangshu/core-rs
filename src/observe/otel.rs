//! OpenTelemetry 链路导出（feature = "otel"）：
//! 通过 OTLP gRPC 上报 spans，与 tracing 订阅链装配（见 `crate::logging`）。
//! endpoint 为空时视为未启用。

use opentelemetry_sdk::trace::SdkTracerProvider;

use crate::config::OtelConfig;

/// 持有 provider 保证存活，进程退出时 flush
pub struct OtelGuard {
    provider: SdkTracerProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        let _ = self.provider.force_flush();
    }
}

/// 初始化 OTLP exporter 并设置全局 tracer provider，返回可挂到订阅链上的 layer 与 guard。
///
/// `S` 为最终订阅器类型，由 `logging::init` 的调用点推断。
pub fn setup<S>(
    cfg: &OtelConfig,
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
    if cfg.endpoint.is_empty() {
        return None;
    }

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(&cfg.endpoint)
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
                .with_service_name(cfg.service_name.clone())
                .build(),
        )
        .with_batch_exporter(exporter)
        .build();

    opentelemetry::global::set_tracer_provider(provider.clone());

    let tracer = provider.tracer("core-rs");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    Some((layer, OtelGuard { provider }))
}
