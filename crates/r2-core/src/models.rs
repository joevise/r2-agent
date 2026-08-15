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
    /// 已知 API 端点提示（openai_compat 填 openai 格式端点；anthropic 填 anthropic 端点；空=未知）
    pub endpoint: &'static str,
    /// 订阅计划说明（如 "Kimi Coding Plan 包月"）；空串 = 按量计费
    pub coding_plan: &'static str,
}

/// 内置注册表（价格为参考值，以官网为准。
/// 国际模型：OpenRouter 2026-08-16 实时价（美元×7.2 折元，注释标美元原价）；
/// 国内模型：官方国内定价（OpenRouter 平台价常有补贴，仅注释参考））
static REGISTRY: &[ModelInfo] = &[
    // ===== 国内 =====
    ModelInfo {
        key: "glm-5.2-air",
        display_name: "glm-5.2-air",
        context_window: 128_000,
        input_price_per_m: 0.8,
        output_price_per_m: 2.0,
        tool_support: true,
        provider_hint: "zhipu",
        endpoint: "https://open.bigmodel.cn/api/paas/v4",
        coding_plan: "",
    },
    ModelInfo {
        key: "glm-5.2-flash",
        display_name: "glm-5.2-flash",
        context_window: 128_000,
        input_price_per_m: 0.5,
        output_price_per_m: 2.0,
        tool_support: true,
        provider_hint: "zhipu",
        endpoint: "https://open.bigmodel.cn/api/paas/v4",
        coding_plan: "",
    },
    ModelInfo {
        key: "glm-5.2",
        display_name: "glm-5.2",
        context_window: 1_000_000,
        input_price_per_m: 8.0,
        output_price_per_m: 8.0,
        tool_support: true,
        provider_hint: "zhipu",
        endpoint: "https://open.bigmodel.cn/api/paas/v4",
        coding_plan: "智谱 Coding Plan（open.bigmodel.cn/api/coding/paas/v4）",
    },
    ModelInfo {
        key: "kimi-for-coding",
        display_name: "kimi-for-coding",
        context_window: 256_000,
        input_price_per_m: 8.0,
        output_price_per_m: 24.0,
        tool_support: true,
        provider_hint: "moonshot",
        endpoint: "https://api.kimi.com/coding/v1",
        coding_plan: "Kimi Coding Plan 包月",
    },
    ModelInfo {
        key: "kimi-k3",
        display_name: "kimi-k3",
        context_window: 1_048_576,
        input_price_per_m: 20.0,
        output_price_per_m: 100.0,
        tool_support: true,
        provider_hint: "moonshot",
        endpoint: "https://api.moonshot.cn/v1",
        coding_plan: "Kimi Coding Plan（api.kimi.com/coding，k3/kimi-for-coding 包月）",
    },
    // deepseek-v4-pro 缓存命中输入仅 0.025 元/M（未命中按 3 元/M 计）
    ModelInfo {
        key: "deepseek-v4-pro",
        display_name: "deepseek-v4-pro",
        context_window: 128_000,
        input_price_per_m: 3.0,
        output_price_per_m: 6.0,
        tool_support: true,
        provider_hint: "deepseek",
        endpoint: "https://api.deepseek.com/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "deepseek-v4",
        display_name: "deepseek-v4",
        context_window: 128_000,
        input_price_per_m: 2.0,
        output_price_per_m: 8.0,
        tool_support: true,
        provider_hint: "deepseek",
        endpoint: "https://api.deepseek.com/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "deepseek-r2",
        display_name: "deepseek-r2",
        context_window: 128_000,
        input_price_per_m: 4.0,
        output_price_per_m: 16.0,
        tool_support: true,
        provider_hint: "deepseek",
        endpoint: "https://api.deepseek.com/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "qwen3.5-max",
        display_name: "qwen3.5-max",
        context_window: 1_000_000,
        input_price_per_m: 6.0,
        output_price_per_m: 18.0,
        tool_support: true,
        provider_hint: "alibaba",
        endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "qwen3-plus",
        display_name: "qwen3-plus",
        context_window: 131_072,
        input_price_per_m: 0.8,
        output_price_per_m: 3.0,
        tool_support: true,
        provider_hint: "alibaba",
        endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "doubao-seed-1.6",
        display_name: "doubao-seed-1.6",
        context_window: 256_000,
        input_price_per_m: 0.8,
        output_price_per_m: 2.0,
        tool_support: true,
        provider_hint: "volcengine",
        endpoint: "https://ark.cn-beijing.volces.com/api/v3",
        coding_plan: "火山方舟",
    },
    ModelInfo {
        key: "minimax-m3",
        display_name: "minimax-m3",
        context_window: 1_000_000,
        input_price_per_m: 2.0,
        output_price_per_m: 8.0,
        tool_support: true,
        provider_hint: "minimax",
        endpoint: "https://api.minimaxi.com/v1",
        coding_plan: "MiniMax Anthropic 端点：api.minimaxi.com/anthropic/v1",
    },
    ModelInfo {
        key: "minimax-m2.7",
        display_name: "minimax-m2.7",
        context_window: 1_000_000,
        input_price_per_m: 1.0,
        output_price_per_m: 4.0,
        tool_support: true,
        provider_hint: "minimax",
        endpoint: "https://api.minimaxi.com/v1",
        coding_plan: "MiniMax Anthropic 端点：api.minimaxi.com/anthropic/v1",
    },
    ModelInfo {
        key: "hunyuan-t2",
        display_name: "hunyuan-t2",
        context_window: 256_000,
        input_price_per_m: 4.0,
        output_price_per_m: 12.0,
        tool_support: true,
        provider_hint: "tencent",
        endpoint: "https://api.hunyuan.cloud.tencent.com/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "mimo-v2.5-pro",
        display_name: "mimo-v2.5-pro",
        context_window: 128_000,
        input_price_per_m: 1.0,
        output_price_per_m: 4.0,
        tool_support: true,
        provider_hint: "xiaomi",
        endpoint: "https://token-plan-cn.xiaomimimo.com/v1",
        coding_plan: "小米 Token Plan（免费额度大）",
    },
    // ===== 国际（美元价按 7.2 汇率折元） =====
    // OpenRouter $5/$25（2026-08 大幅降价）
    ModelInfo {
        key: "claude-opus-4.5",
        display_name: "claude-opus-4.5",
        context_window: 200_000,
        input_price_per_m: 36.0,
        output_price_per_m: 180.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // OpenRouter $2/$10（已降价）
    ModelInfo {
        key: "claude-sonnet-5",
        display_name: "claude-sonnet-5",
        context_window: 1_000_000,
        input_price_per_m: 14.4,
        output_price_per_m: 72.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // $3/$15（1M 窗口档）
    ModelInfo {
        key: "claude-sonnet-4.6",
        display_name: "claude-sonnet-4.6",
        context_window: 1_000_000,
        input_price_per_m: 22.0,
        output_price_per_m: 110.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "GitHub Copilot 可用",
    },
    // $1/$5
    ModelInfo {
        key: "claude-haiku-4.5",
        display_name: "claude-haiku-4.5",
        context_window: 200_000,
        input_price_per_m: 7.2,
        output_price_per_m: 36.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // OpenRouter $2.50/$15（此前误用 GPT-5.2 旧价，已修正）
    ModelInfo {
        key: "gpt-5.4",
        display_name: "gpt-5.4",
        context_window: 1_050_000,
        input_price_per_m: 18.0,
        output_price_per_m: 108.0,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "GitHub Copilot 可用",
    },
    ModelInfo {
        key: "gpt-5.2-mini",
        display_name: "gpt-5.2-mini",
        context_window: 256_000,
        input_price_per_m: 9.0,
        output_price_per_m: 32.0, // 约 $1.25/$4.4
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // 上代旗舰，OpenRouter 约 $1.25/$10
    ModelInfo {
        key: "gpt-5.2",
        display_name: "gpt-5.2",
        context_window: 400_000,
        input_price_per_m: 9.0,
        output_price_per_m: 72.0,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // $2/$12
    ModelInfo {
        key: "gemini-3-pro",
        display_name: "gemini-3-pro",
        context_window: 2_000_000,
        input_price_per_m: 14.0,
        output_price_per_m: 84.0,
        tool_support: true,
        provider_hint: "google",
        endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
        coding_plan: "",
    },
    // OpenRouter $0.5/$3（flash 档）
    ModelInfo {
        key: "gemini-3.1-flash",
        display_name: "gemini-3.1-flash",
        context_window: 1_000_000,
        input_price_per_m: 3.6,
        output_price_per_m: 21.6,
        tool_support: true,
        provider_hint: "google",
        endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
        coding_plan: "",
    },
    // OpenRouter $2/$6
    ModelInfo {
        key: "grok-4.6",
        display_name: "grok-4.6",
        context_window: 500_000,
        input_price_per_m: 14.4,
        output_price_per_m: 43.2,
        tool_support: true,
        provider_hint: "xai",
        endpoint: "https://api.x.ai/v1",
        coding_plan: "",
    },
    // OpenRouter $0.75/$4.5
    ModelInfo {
        key: "gpt-5.4-mini",
        display_name: "gpt-5.4-mini",
        context_window: 400_000,
        input_price_per_m: 5.4,
        output_price_per_m: 32.4,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // OpenRouter $0.20/$1.25
    ModelInfo {
        key: "gpt-5.4-nano",
        display_name: "gpt-5.4-nano",
        context_window: 400_000,
        input_price_per_m: 1.4,
        output_price_per_m: 9.0,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // OpenRouter $0.06/$0.13（极致性价比；国产官方价略高）
    ModelInfo {
        key: "deepseek-v4-flash",
        display_name: "deepseek-v4-flash",
        context_window: 1_048_000,
        input_price_per_m: 0.43,
        output_price_per_m: 0.94,
        tool_support: true,
        provider_hint: "deepseek",
        endpoint: "https://api.deepseek.com/v1",
        coding_plan: "",
    },
    // OpenRouter $0.71/$3.5
    ModelInfo {
        key: "kimi-k2.7-code",
        display_name: "kimi-k2.7-code",
        context_window: 262_000,
        input_price_per_m: 5.1,
        output_price_per_m: 25.2,
        tool_support: true,
        provider_hint: "moonshot",
        endpoint: "https://api.moonshot.cn/v1",
        coding_plan: "Kimi Coding Plan 包月",
    },
    ModelInfo {
        key: "mock",
        display_name: "mock",
        context_window: 8_192,
        input_price_per_m: 0.0,
        output_price_per_m: 0.0,
        tool_support: false,
        provider_hint: "test",
        endpoint: "",
        coding_plan: "",
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
        assert_eq!(info.context_window, 1_000_000);
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
    fn test_lookup_longest_kimi_for_coding() {
        // kimi-for-coding 不被 kimi-k3 / kimi 抢
        let info = lookup("kimi-for-coding").unwrap();
        assert_eq!(info.key, "kimi-for-coding");
        assert_eq!(info.context_window, 256_000);
        // kimi-k3 独立命中
        let info = lookup("kimi-k3").unwrap();
        assert_eq!(info.key, "kimi-k3");
        assert_eq!(info.context_window, 1_048_576);
    }

    #[test]
    fn test_lookup_glm_air_independent() {
        // glm-5.2-air 不被 glm-5.2 抢
        let info = lookup("glm-5.2-air").unwrap();
        assert_eq!(info.key, "glm-5.2-air");
        assert_eq!(info.context_window, 128_000);
        assert_eq!(info.provider_hint, "zhipu");
    }

    #[test]
    fn test_coding_plan_and_endpoint_fields() {
        let kimi = lookup("kimi-for-coding").unwrap();
        assert!(!kimi.coding_plan.is_empty());
        assert_eq!(kimi.endpoint, "https://api.kimi.com/coding/v1");
        let glm = lookup("glm-5.2").unwrap();
        assert!(glm.coding_plan.contains("Coding Plan"));
        assert!(!glm.endpoint.is_empty());
        // 按量计费模型 coding_plan 为空
        let glm_air = lookup("glm-5.2-air").unwrap();
        assert!(glm_air.coding_plan.is_empty());
        // mock 端点与订阅均为空
        let mock = lookup("mock").unwrap();
        assert!(mock.endpoint.is_empty());
        assert!(mock.coding_plan.is_empty());
    }

    #[test]
    fn test_lookup_miss() {
        assert!(lookup("no-such-model-xyz").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn test_estimate_cost_known() {
        // glm-5.2：输入 8 元/M，输出 8 元/M
        let usage = UsageStats {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            llm_calls: 2,
        };
        let cost = estimate_cost("glm-5.2", &usage).unwrap();
        assert!((cost - 16.0).abs() < 1e-9);

        // 小量级：输入 12_000，输出 4_500 → 0.096 + 0.036 = 0.132
        let usage = UsageStats {
            input_tokens: 12_000,
            output_tokens: 4_500,
            llm_calls: 1,
        };
        let cost = estimate_cost("glm-5.2", &usage).unwrap();
        assert!((cost - 0.132).abs() < 1e-9);

        // mock 价格为 0：有命中但成本为 0
        let cost = estimate_cost("mock", &usage).unwrap();
        assert!((cost - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_estimate_cost_deepseek_v4_pro() {
        // deepseek-v4-pro：输入 3 元/M，输出 6 元/M
        let usage = UsageStats {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            llm_calls: 2,
        };
        let cost = estimate_cost("deepseek-v4-pro", &usage).unwrap();
        assert!((cost - 9.0).abs() < 1e-9);

        // 小量级：输入 500_000，输出 100_000 → 1.5 + 0.6 = 2.1
        let usage = UsageStats {
            input_tokens: 500_000,
            output_tokens: 100_000,
            llm_calls: 1,
        };
        let cost = estimate_cost("deepseek-v4-pro", &usage).unwrap();
        assert!((cost - 2.1).abs() < 1e-9);
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
