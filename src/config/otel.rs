use serde::{Deserialize, Serialize};

fn default_endpoint() -> String {
    "http://127.0.0.1:4317".to_string()
}
fn default_service_name() -> String {
    "core-rs".to_string()
}

/// `[otel]` 配置段（feature = "otel"）。endpoint 为空时不启用链路导出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelConfig {
    /// OTLP gRPC 端点，如 http://127.0.0.1:4317
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_service_name")]
    pub service_name: String,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            service_name: default_service_name(),
        }
    }
}
