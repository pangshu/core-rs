//! Accept-Language 协商：[`negotiate`] 在支持语言与回退链内选最佳匹配。

use crate::config::sections::I18nSettings; // 引入 i18n 配置节（默认语言、支持语言、目录、回退链）

use super::Locale; // 引入 Locale 类型

/// Accept-Language 协商：在支持语言与回退链内选最佳匹配；
/// 无匹配时回落默认语言（i18n 关闭时等于默认语言）
pub fn negotiate(accept_language: &str, settings: &I18nSettings) -> Locale { // 依据请求头协商出最佳 Locale
    let candidates = parse_accept_language(accept_language); // 把请求头解析为按权重降序的候选语言列表
    for candidate in candidates { // 依次尝试每个候选语言
        // 精确匹配（zh-CN）→ 主语言匹配（zh）→ 回退链
        if settings.supported.iter().any(|s| s.eq_ignore_ascii_case(&candidate)) { // 候选与支持列表精确匹配（忽略大小写）
            return Locale::parse(&candidate, settings); // 命中则归一化后返回该语言
        }
        let main = candidate.split('-').next().unwrap_or("").to_string(); // 取候选的主语言部分（如 zh-CN → zh）
        if settings.supported.iter().any(|s| s.eq_ignore_ascii_case(&main)) { // 主语言在支持列表内也视为命中
            return Locale::parse(&main, settings); // 用主语言归一化后返回
        }
    }
    Locale::default_for(settings) // 全部未命中，回落配置的默认语言
}

/// `Accept-Language: zh-CN,zh;q=0.9,en;q=0.8` → 按权重降序的候选列表
fn parse_accept_language(header: &str) -> Vec<String> { // 解析 Accept-Language 头为降序候选语言列表
    let mut items: Vec<(f32, String)> = header // 累积 (权重 q, 语言标签) 二元组
        .split(',') // 按逗号切分各语言项
        .filter_map(|part| { // 逐项解析，无效项返回 None 被过滤掉
            let part = part.trim(); // 去掉该项首尾空白
            if part.is_empty() { // 空项直接跳过
                return None;
            }
            let mut segments = part.split(';'); // 以分号切出语言标签与参数段
            let tag = segments.next()?.trim().to_string(); // 第一段是语言标签
            let mut q = 1.0f32; // 默认权重为 1.0
            for seg in segments { // 遍历参数段寻找 q 值
                let seg = seg.trim(); // 去掉参数段空白
                if let Some(v) = seg.strip_prefix("q=") { // 识别 q= 前缀的参数
                    // RFC 9110：q=0 表示明确不可接受；畸形 q 不能当成最高优先级，
                    // 一律按 0 处理（丢弃），而不是 unwrap_or(1.0)
                    q = v.parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0); // 解析权重并夹到 [0,1]，畸形按 0
                }
            }
            if q <= 0.0 || tag.eq_ignore_ascii_case("*") { // 权重为 0 或通配符 * 均视为不可接受
                None
            } else {
                Some((q, tag)) // 保留有效项
            }
        })
        .collect();
    // sort_by 是稳定排序：等 q 保持出现顺序（RFC 建议的 tie-break）
    items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)); // 按权重降序稳定排序
    items.into_iter().map(|(_, tag)| tag).collect() // 只取语言标签、丢弃权重后返回
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        let settings = I18nSettings {
            enabled: true,
            default_locale: "zh-CN".into(),
            supported: vec!["zh-CN".into(), "en".into()],
            ..Default::default()
        };
        assert_eq!(negotiate("en-US,en;q=0.9", &settings).as_str(), "en");
        assert_eq!(negotiate("zh-Hant", &settings).as_str(), "zh-CN"); // zh 不在支持列表，回默认
        assert_eq!(negotiate("", &settings).as_str(), "zh-CN");
        assert_eq!(negotiate("fr,de;q=0.5", &settings).as_str(), "zh-CN");
    }
}
