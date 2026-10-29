//! 校验错误 → 统一错误体的映射规则。
//!
//! 把 `validator::ValidationErrors` 摊平为 `{字段路径: [错误信息]}` 的 `details`，
//! 并挑出首个错误作为对外 `message`。
//!
//! 字段路径规则：嵌套结构用 `.` 连接（`address.city`），列表元素用 `[i]` 追加
//! （`items[0].name`），保证明细可被前端直接定位到字段。
//!
//! # 三条硬约束
//!
//! 1. **条数有上限**：`[validation].max_errors`（默认 10，硬上限 100）必须真正生效。
//!    不设上限时，`Vec<Nested>` 型 DTO 在 2 MiB 请求体下可产生十几万条明细
//!    （每条一次 `format!` + 哈希插入，单请求数十 MB），而响应侧
//!    `ApiResponse::into_response` 的 4 KiB 闸门又会把整份明细丢掉——**纯浪费**，
//!    是现成的内存 / CPU 放大面。
//! 2. **顺序确定**：`ValidationErrors` 内部是 `HashMap`，迭代序随进程随机；
//!    直接取「第一个」会让同一非法请求每次返回不同的 `message`（前端提示抖动、
//!    测试 flaky、按文案做告警匹配的应用漏报）。这里按键排序后再遍历。
//! 3. **递归有深度上限**：当前深度本已受 `serde_json` 默认 128 层限制，但那是
//!    外部库的隐式行为；显式封顶后，将来换 deserializer 或开 `unbounded_depth`
//!    也不会栈溢出。
//!
//! # 不反射用户输入
//!
//! 键来自 `validator` 的**编译期字段名**（`&'static str`），值只取 `message`
//! 属性或静态 `code`。**不要**改用 `ValidationError::params` 生成对外文案：
//! 部分校验器的 `params` 里带 `value`（即用户输入原文）。

use serde_json::Value;
use validator::{ValidationErrors, ValidationErrorsKind};

use crate::errors::{AppError, ErrorCode};
use crate::utils::string::log_safe;
use crate::validation::options::ValidationOptions;

/// 明细里允许出现的最大嵌套深度（超出即停止下钻）。
///
/// 正常 DTO 的嵌套不会超过 3~4 层；8 层已足够覆盖「订单 → 明细 → 收货地址 →
/// 经纬度」这类结构，同时把递归栈钉死在一个可证明的上界。
const MAX_NESTING_DEPTH: usize = 8;

/// 日志里 `message` 的最大字符数（与 `errors::response` 的取值一致）。
const MAX_LOG_CHARS: usize = 512;

/// 把 `ValidationErrors` 汇总为 `10001`（400）错误。
///
/// 无字段明细时退化为纯 `message`；有明细时附带 `details`。
pub(crate) fn to_app_error(errors: &ValidationErrors, options: ValidationOptions) -> AppError {
    let mut collector = Collector::new(options.effective_limit());
    collector.collect(errors, "", 0);

    let message = collector
        .summary
        .take()
        .unwrap_or_else(|| "validation failed".to_string());

    // 校验失败此前完全静默：撞库、参数 fuzz、绕过尝试在日志里没有痕迹。
    // 只记「哪些字段、多少条、首个错误码」——`path` 是编译期字段名，
    // `code` 是 validator 的静态串，均不含用户输入。
    tracing::debug!(
        fields = collector.details.len(),
        errors = collector.count,
        limit = collector.limit,
        truncated = collector.truncated,
        depth_capped = collector.depth_capped,
        first = %log_safe(&message, MAX_LOG_CHARS),
        "validation rejected"
    );

    // 计数器与日志是两种粒度：日志给单次请求的取证，计数器给「10001 突增」告警
    #[cfg(feature = "metrics")]
    crate::validation::metrics::record_rejection();

    if collector.details.is_empty() {
        AppError::biz(ErrorCode::INVALID_PARAM, message)
    } else {
        AppError::biz_with_details(
            ErrorCode::INVALID_PARAM,
            message,
            Value::Object(collector.details),
        )
    }
}

/// 一次校验的收集状态。
struct Collector {
    details: serde_json::Map<String, Value>,
    summary: Option<String>,
    /// 最多收集多少条错误。
    limit: usize,
    /// 已收集条数（可能略大于 `details` 的实际条数：字段路径碰撞时会覆盖）。
    count: usize,
    /// 是否因达到 `limit` 而提前停止。
    truncated: bool,
    /// 是否因嵌套过深而停止下钻。
    depth_capped: bool,
}

impl Collector {
    fn new(limit: usize) -> Self {
        Self {
            details: serde_json::Map::new(),
            summary: None,
            limit,
            count: 0,
            truncated: false,
            depth_capped: false,
        }
    }

    /// 递归收集字段错误（含嵌套结构 / 列表）。
    fn collect(&mut self, errors: &ValidationErrors, prefix: &str, depth: usize) {
        // 上限判定放在**遍历内**：进入本层时若已达上限，本层可能一个错误都没有
        // （空嵌套结构），在这里置 `truncated` 会误报「有内容被丢弃」。
        if depth > MAX_NESTING_DEPTH {
            self.depth_capped = true;
            return;
        }

        // 按键排序 → 遍历顺序与 `HashMap` 的随机种子无关（见模块文档约束 2）
        let mut entries: Vec<(&str, &ValidationErrorsKind)> = errors
            .errors()
            .iter()
            .map(|(field, kind)| (field.as_ref(), kind))
            .collect();
        entries.sort_unstable_by(|left, right| left.0.cmp(right.0));

        for (field, kind) in entries {
            if self.count >= self.limit {
                self.truncated = true;
                return;
            }

            let path = if prefix.is_empty() {
                field.to_string()
            } else {
                format!("{prefix}.{field}")
            };

            match kind {
                ValidationErrorsKind::Struct(nested) => {
                    self.collect(nested, &path, depth + 1);
                }
                ValidationErrorsKind::List(items) => {
                    // `List` 内部是 `BTreeMap<usize, Box<ValidationErrors>>`：下标本身有序，
                    // 无需再排序
                    for (index, nested) in items {
                        if self.count >= self.limit {
                            self.truncated = true;
                            return;
                        }
                        self.collect(nested.as_ref(), &format!("{path}[{index}]"), depth + 1);
                    }
                }
                ValidationErrorsKind::Field(field_errors) => {
                    self.push_field(path, field_errors);
                }
            }
        }
    }

    /// 收一个字段的错误（`message` 缺省时回退到静态 `code`）。
    fn push_field(&mut self, path: String, field_errors: &[validator::ValidationError]) {
        let remaining = self.limit.saturating_sub(self.count);
        if remaining == 0 {
            self.truncated = true;
            return;
        }

        let mut messages: Vec<String> = field_errors
            .iter()
            .map(|err| {
                err.message
                    .as_ref()
                    .map(|message| message.to_string())
                    .unwrap_or_else(|| err.code.to_string())
            })
            .collect();
        if messages.is_empty() {
            return;
        }
        if messages.len() > remaining {
            messages.truncate(remaining);
            self.truncated = true;
        }

        if self.summary.is_none() {
            // 排序后的首个字段 = 确定性文案
            self.summary = Some(format!("{path}: {}", messages[0]));
        }
        self.count += messages.len();
        self.details.insert(path, Value::from(messages));
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use validator::Validate;

    use super::*;

    #[derive(Debug, Deserialize, Validate)]
    struct Nested {
        #[validate(length(min = 2))]
        city: String,
    }

    #[derive(Debug, Deserialize, Validate)]
    struct Outer {
        #[validate(email)]
        email: String,
        #[validate(nested)]
        address: Nested,
        #[validate(nested)]
        items: Vec<Nested>,
    }

    #[derive(Debug, Deserialize, Validate)]
    struct ManyFields {
        #[validate(length(min = 2))]
        f01: String,
        #[validate(length(min = 2))]
        f02: String,
        #[validate(length(min = 2))]
        f03: String,
        #[validate(length(min = 2))]
        f04: String,
        #[validate(length(min = 2))]
        f05: String,
    }

    #[derive(Debug, Deserialize, Validate)]
    struct WideList {
        #[validate(nested)]
        items: Vec<Nested>,
    }

    fn invalid_outer() -> Outer {
        Outer {
            email: "not-an-email".to_string(),
            address: Nested {
                city: "x".to_string(),
            },
            items: vec![Nested {
                city: "y".to_string(),
            }],
        }
    }

    #[test]
    fn should_flatten_nested_and_list_paths_when_invalid() {
        let errors = invalid_outer().validate().expect_err("应校验失败");
        let err = to_app_error(&errors, ValidationOptions::default());

        assert_eq!(err.code(), ErrorCode::INVALID_PARAM);
        let details = err.details().expect("details");
        assert!(details["email"].is_array(), "字段错误应被收集");
        assert!(details["address.city"].is_array(), "嵌套结构应拼成点路径");
        assert!(details["items[0].city"].is_array(), "列表元素应带下标");
    }

    #[test]
    fn should_point_message_at_the_only_invalid_field() {
        let value = Outer {
            email: "a@b.com".to_string(),
            address: Nested {
                city: "ok".to_string(),
            },
            items: vec![Nested {
                city: "y".to_string(),
            }],
        };
        let errors = value.validate().expect_err("应校验失败");
        let err = to_app_error(&errors, ValidationOptions::default());
        assert!(
            err.to_string().contains("items[0].city"),
            "message 应定位到出错字段，实际：{err}"
        );
    }

    #[test]
    fn should_be_deterministic_across_repeated_mappings() {
        // 同一份 `ValidationErrors` 反复映射，文案与明细顺序必须恒定
        let errors = invalid_outer().validate().expect_err("应校验失败");
        let first = to_app_error(&errors, ValidationOptions::default());
        for _ in 0..16 {
            let next = to_app_error(&errors, ValidationOptions::default());
            assert_eq!(first.to_string(), next.to_string(), "message 不得抖动");
            assert_eq!(first.details(), next.details(), "details 不得抖动");
        }
    }

    #[test]
    fn should_cap_details_at_max_errors() {
        let value = ManyFields {
            f01: String::new(),
            f02: String::new(),
            f03: String::new(),
            f04: String::new(),
            f05: String::new(),
        };
        let errors = value.validate().expect_err("应校验失败");

        let err = to_app_error(&errors, ValidationOptions::with_max_errors(2));
        let details = err.details().expect("details").as_object().expect("对象");
        assert_eq!(details.len(), 2, "max_errors = 2 时只返回 2 个字段");
    }

    #[test]
    fn should_return_single_error_when_fail_fast() {
        let value = ManyFields {
            f01: String::new(),
            f02: String::new(),
            f03: String::new(),
            f04: String::new(),
            f05: String::new(),
        };
        let errors = value.validate().expect_err("应校验失败");

        let options = ValidationOptions::new(10, true);
        let err = to_app_error(&errors, options);
        let details = err.details().expect("details").as_object().expect("对象");
        assert_eq!(details.len(), 1, "fail_fast 只返回首条");
    }

    #[test]
    fn should_not_blow_up_on_wide_list() {
        // 1000 个非法列表元素：明细必须被上限截断，不能逐个累积
        let value = WideList {
            items: (0..1000)
                .map(|_| Nested {
                    city: String::new(),
                })
                .collect(),
        };
        let errors = value.validate().expect_err("应校验失败");
        let err = to_app_error(&errors, ValidationOptions::with_max_errors(5));
        let details = err.details().expect("details").as_object().expect("对象");
        assert_eq!(details.len(), 5, "宽列表必须被截断");
    }
}
