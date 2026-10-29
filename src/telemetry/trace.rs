//! 分布式追踪导出（feature `otel`）。
//!
//! 把 `tracing` 的 span 经 `tracing-opentelemetry` 送入 OpenTelemetry SDK，
//! 由 **OTLP/gRPC** 导出到后端（Jaeger / Tempo / OTLP Collector 等）。
//!
//! - 端点取 `[telemetry].otel.endpoint`（如 `http://localhost:4317`）
//! - 服务名取 `[telemetry].otel.service_name`
//! - 进程退出前须调用 [`shutdown`] 刷新未发送的 span（`boot::serve` 已调用）
//!
//! **与 trace-id 的关系**：`telemetry::layer` 的 `TraceId` 是**框架自己的**
//! 请求级标识（写日志与 `X-Trace-Id`），与 OTel 的 128-bit trace-id 各自独立；
//! 需要对齐时以 OTel 的 span context 为准（见 `docs/03` §4.5）。

use std::sync::OnceLock;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::trace::{Sampler, SdkTracer, SdkTracerProvider};
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::registry::LookupSpan;

use crate::boot::startup::StartupError;
use crate::config::sections::OtelSection;

/// 全局 trace provider：用于退出前 `shutdown`（flush）。
static PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// 全局日志 provider：与 trace 一起在退出前 flush。
static LOGGER_PROVIDER: OnceLock<SdkLoggerProvider> = OnceLock::new();

/// 构建 OTel 层；`otel.enabled = false` 时返回 `None`。
///
/// # Errors
/// OTLP exporter 构建失败（端点非法 / TLS 初始化失败等）时返回
/// [`StartupError::Telemetry`]。
pub(crate) fn layer<S>(
    cfg: &OtelSection,
) -> Result<Option<OpenTelemetryLayer<S, SdkTracer>>, StartupError>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    if !cfg.enabled {
        return Ok(None);
    }

    let endpoint = endpoint(cfg)?;

    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .map_err(|error| StartupError::Telemetry(format!("OTLP exporter 初始化失败：{error}")))?;

    let resource = Resource::builder()
        .with_service_name(cfg.service_name.clone())
        .build();
    let provider = SdkTracerProvider::builder()
        .with_sampler(sampler(cfg.sample_ratio))
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("aurora");
    // 重复初始化（如测试里多次 `init_tracing`）时保留首个 provider
    let _ = PROVIDER.set(provider);

    Ok(Some(
        tracing_opentelemetry::layer::<S>().with_tracer(tracer),
    ))
}

/// 构建日志导出层；`otel.enabled && otel.logs` 为假时返回 `None`。
///
/// 同一份 `tracing` 事件既进 fmt 层（本地日志），也经桥接层成为 OTel 日志记录，
/// 在 OTLP 后端可按 trace-id 与链路关联。
///
/// # Errors
/// OTLP 日志导出器构建失败时返回 [`StartupError::Telemetry`]。
pub(crate) fn logs_layer(
    cfg: &OtelSection,
) -> Result<Option<OpenTelemetryTracingBridge<SdkLoggerProvider, SdkLogger>>, StartupError> {
    if !cfg.enabled || !cfg.logs {
        return Ok(None);
    }

    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint(cfg)?)
        .build()
        .map_err(|error| StartupError::Telemetry(format!("OTLP 日志导出器初始化失败：{error}")))?;

    let resource = Resource::builder()
        .with_service_name(cfg.service_name.clone())
        .build();
    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let layer = OpenTelemetryTracingBridge::new(&provider);
    // 重复初始化时保留首个 provider
    let _ = LOGGER_PROVIDER.set(provider);
    Ok(Some(layer))
}

/// 取并校验 OTLP 端点。
fn endpoint(cfg: &OtelSection) -> Result<&str, StartupError> {
    cfg.endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .ok_or_else(|| {
            StartupError::Telemetry("[telemetry].otel.enabled = true 但未配置 endpoint".to_string())
        })
}

/// 按 `[telemetry].otel.sample_ratio` 构造采样器。
///
/// 默认 `always_on` 在低 QPS 下没问题，但生产高 QPS 时 100% 采样会把 collector
/// 与出网带宽打满（等于给自己制造一次 DoS）。比例采样用 `ParentBased` 包一层：
/// 上游已决定采样的链路继续采，避免同一条链路上半段有、下半段没有。
fn sampler(ratio: f64) -> Sampler {
    if ratio >= 1.0 {
        Sampler::AlwaysOn
    } else if ratio <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
    }
}

/// 刷新并关闭导出器；未启用 `otel` 时为空操作。
///
/// 必须在 Tokio 运行时**仍存活**时调用（batch exporter 需要运行时发送）。
pub fn shutdown() {
    if let Some(provider) = PROVIDER.get()
        && let Err(error) = provider.shutdown()
    {
        tracing::warn!(%error, "OTel trace provider 关闭失败");
    }
    if let Some(provider) = LOGGER_PROVIDER.get()
        && let Err(error) = provider.shutdown()
    {
        tracing::warn!(%error, "OTel logger provider 关闭失败");
    }
}
