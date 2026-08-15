//! Provider 注册表：内置模型元数据（上下文窗口 / 参考价格 / 工具支持）+ 成本估算
//!
//! 静态注册表，不引外部文件。匹配规则：模型名小写后【包含】key 即命中，
//! 多个命中时取 key 最长者（保证 glm-5.2-flash 不会被 glm-5.2 抢先命中）。

use crate::types::UsageStats;

/// 模型元数据
pub struct ModelInfo {
    /// 匹配关键词（小写，模型名包含即命中，如 "glm-5.2" 匹配 "GLM-5.2"/"glm-5.2-0520"）
    pub key: &'static str,
    pub display_name: &'static str,
    /// 上下文窗口（tokens）
    pub context_window: usize,
    /// 输入价格（元/百万token）
    pub input_price_per_m: f64,
    /// 输出价格（元/百万token）
    pub output_price_per_m: f64,
    /// 是否支持工具调用
    pub tool_support: bool,
    pub provider_hint: &'static str, // "zhipu"/"moonshot"/"deepseek"/...
}

/// 内置注册表（价格为参考值——2026-08 大致行情，以官网为准）
static REGISTRY: &[ModelInfo] = &[
    ModelInfo {
        key: "glm-5.2-flash",
        display_name: "glm-5.2-flash",
        context_window: 128_000,
        input_price_per_m: 0.5,
        output_price_per_m: 2.0,
        tool_support: true,
        provider_hint: "zhipu",
    },
    ModelInfo {
        key: "glm-5.2",
        display_name: "glm-5.2",
        context_window: 200_000,
        input_price_per_m: 4.0,
        output_price_per_m: 16.0,
        tool_support: true,
        provider_hint: "zhipu",
    },
    ModelInfo {
        key: "glm-4.7",
        display_name: "glm-4.7",
        context_window: 200_000,
        input_price_per_m: 2.0,
        output_price_per_m: 8.0,
        tool_support: true,
        provider_hint: "zhipu",
    },
    ModelInfo {
        key: "kimi-for-coding",
        display_name: "kimi-for-coding",
        context_window: 1_000_000,
        input_price_per_m: 8.0,
        output_price_per_m: 24.0,
        tool_support: true,
        provider_hint: "moonshot",
    },
    ModelInfo {
        key: "kimi-k3",
        display_name: "kimi-k3",
        context_window: 1_000_000,
        input_price_per_m: 8.0,
        output_price_per_m: 24.0,
        tool_support: true,
        provider_hint: "moonshot",
    },
    ModelInfo {
        key: "deepseek-v4",
        display_name: "deepseek-v4",
        context_window: 128_000,
        input_price_per_m: 2.0,
        output_price_per_m: 8.0,
        tool_support: true,
        provider_hint: "deepseek",
    },
    ModelInfo {
        key: "deepseek-r2",
        display_name: "deepseek-r2",
        context_window: 128_000,
        input_price_per_m: 4.0,
        output_price_per_m: 16.0,
        tool_support: true,
        provider_hint: "deepseek",
    },
    ModelInfo {
        key: "qwen3-max",
        display_name: "qwen3-max",
        context_window: 1_000_000,
        input_price_per_m: 6.0,
        output_price_per_m: 18.0,
        tool_support: true,
        provider_hint: "alibaba",
    },
    ModelInfo {
        key: "qwen3-plus",
        display_name: "qwen3-plus",
        context_window: 131_072,
        input_price_per_m: 0.8,
        output_price_per_m: 3.0,
        tool_support: true,
        provider_hint: "alibaba",
    },
    ModelInfo {
        key: "claude-sonnet-5",
        display_name: "claude-sonnet-5",
        context_window: 200_000,
        input_price_per_m: 22.0,
        output_price_per_m: 110.0,
        tool_support: true,
        provider_hint: "anthropic",
    },
    ModelInfo {
        key: "claude-haiku-4.5",
        display_name: "claude-haiku-4.5",
        context_window: 200_000,
        input_price_per_m: 5.5,
        output_price_per_m: 27.5,
        tool_support: true,
        provider_hint: "anthropic",
    },
    ModelInfo {
        key: "gpt-5.2-mini",
        display_name: "gpt-5.2-mini",
        context_window: 256_000,
        input_price_per_m: 2.8,
        output_price_per_m: 11.0,
        tool_support: true,
        provider_hint: "openai",
    },
    ModelInfo {
        key: "gpt-5.2",
        display_name: "gpt-5.2",
        context_window: 400_000,
        input_price_per_m: 12.0,
        output_price_per_m: 48.0,
        tool_support: true,
        provider_hint: "openai",
    },
    ModelInfo {
        key: "mock",
        display_name: "mock",
        context_window: 8_192,
        input_price_per_m: 0.0,
        output_price_per_m: 0.0,
        tool_support: false,
        provider_hint: "test",
    },
];

/// 按模型名查元数据（小写包含匹配，多命中取 key 最长者）。未命中返回 None。
pub fn lookup(model_name: &str) -> Option<&'static ModelInfo> {
    let name = model_name.to_lowercase();
    REGISTRY
        .iter()
        .filter(|m| name.contains(m.key))
        .max_by_key(|m| m.key.len())
}

/// 只读访问整个注册表（`r2 models` 列表用）
pub fn registry() -> &'static [ModelInfo] {
    REGISTRY
}

/// 按注册表价格估算成本（元）。未命中模型返回 None。
pub fn estimate_cost(model: &str, usage: &UsageStats) -> Option<f64> {
    let info = lookup(model)?;
    Some(
        info.input_price_per_m * usage.input_tokens as f64 / 1_000_000.0
            + info.output_price_per_m * usage.output_tokens as f64 / 1_000_000.0,
    )
}

/// token 数的紧凑显示：999 → "999"，1234 → "1.2k"，1_000_000 → "1.0M"
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lookup_exact() {
        let info = lookup("glm-5.2").unwrap();
        assert_eq!(info.key, "glm-5.2");
        assert_eq!(info.context_window, 200_000);
        assert_eq!(info.provider_hint, "zhipu");
        assert!(info.tool_support);
    }

    #[test]
    fn test_lookup_contains_and_case() {
        // 包含命中：带日期后缀
        let info = lookup("glm-5.2-0520").unwrap();
        assert_eq!(info.key, "glm-5.2");
        // 大小写不敏感
        let info = lookup("GLM-5.2").unwrap();
        assert_eq!(info.key, "glm-5.2");
        let info = lookup("Gpt-5.2").unwrap();
        assert_eq!(info.key, "gpt-5.2");
    }

    #[test]
    fn test_lookup_longest_key_wins() {
        // glm-5.2-flash 也包含 glm-5.2，应命中更具体的 flash
        let info = lookup("glm-5.2-flash").unwrap();
        assert_eq!(info.key, "glm-5.2-flash");
        assert_eq!(info.context_window, 128_000);
        let info = lookup("gpt-5.2-mini").unwrap();
        assert_eq!(info.key, "gpt-5.2-mini");
    }

    #[test]
    fn test_lookup_miss() {
        assert!(lookup("no-such-model-xyz").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn test_estimate_cost_known() {
        // glm-5.2：输入 4 元/M，输出 16 元/M
        let usage = UsageStats {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            llm_calls: 2,
        };
        let cost = estimate_cost("glm-5.2", &usage).unwrap();
        assert!((cost - 20.0).abs() < 1e-9);

        // 小量级：输入 12_000，输出 4_500 → 0.048 + 0.072 = 0.12
        let usage = UsageStats {
            input_tokens: 12_000,
            output_tokens: 4_500,
            llm_calls: 1,
        };
        let cost = estimate_cost("glm-5.2", &usage).unwrap();
        assert!((cost - 0.12).abs() < 1e-9);

        // mock 价格为 0：有命中但成本为 0
        let cost = estimate_cost("mock", &usage).unwrap();
        assert!((cost - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_estimate_cost_unknown() {
        let usage = UsageStats::default();
        assert!(estimate_cost("no-such-model", &usage).is_none());
    }

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_300), "12.3k");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }
}
