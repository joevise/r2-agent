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

/// 内置注册表（价格以官网为准）。
/// 国际模型：OpenRouter 2026-08-16 实时价（美元×7.2 折元，注释标美元原价）；
/// 国内模型：官方国内定价（OpenRouter 平台补贴价仅注释参考）。
static REGISTRY: &[ModelInfo] = &[
    // 国内官方 8/8 元；OpenRouter 补贴价 $0.46/$1.45
    ModelInfo {
        key: "glm-5.2",
        display_name: "glm-5.2",
        context_window: 1000000,
        input_price_per_m: 8.0,
        output_price_per_m: 8.0,
        tool_support: true,
        provider_hint: "zhipu",
        endpoint: "https://open.bigmodel.cn/api/paas/v4",
        coding_plan: "智谱 Coding Plan（open.bigmodel.cn/api/coding/paas/v4）",
    },
    ModelInfo {
        key: "glm-5.2-air",
        display_name: "glm-5.2-air",
        context_window: 128000,
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
        context_window: 128000,
        input_price_per_m: 0.5,
        output_price_per_m: 2.0,
        tool_support: true,
        provider_hint: "zhipu",
        endpoint: "https://open.bigmodel.cn/api/paas/v4",
        coding_plan: "",
    },
    // 上代；OpenRouter $0.97/$3.04
    ModelInfo {
        key: "glm-5.1",
        display_name: "glm-5.1",
        context_window: 204000,
        input_price_per_m: 7.0,
        output_price_per_m: 21.9,
        tool_support: true,
        provider_hint: "zhipu",
        endpoint: "https://open.bigmodel.cn/api/paas/v4",
        coding_plan: "",
    },
    // 国内官方 20/100；OpenRouter $3/$15
    ModelInfo {
        key: "kimi-k3",
        display_name: "kimi-k3",
        context_window: 1048576,
        input_price_per_m: 20.0,
        output_price_per_m: 100.0,
        tool_support: true,
        provider_hint: "moonshot",
        endpoint: "https://api.moonshot.cn/v1",
        coding_plan: "Kimi Coding Plan（api.kimi.com/coding，k3/kimi-for-coding 包月）",
    },
    // OpenRouter $0.71/$3.5
    ModelInfo {
        key: "kimi-k2.7-code",
        display_name: "kimi-k2.7-code",
        context_window: 262000,
        input_price_per_m: 5.1,
        output_price_per_m: 25.2,
        tool_support: true,
        provider_hint: "moonshot",
        endpoint: "https://api.moonshot.cn/v1",
        coding_plan: "Kimi Coding Plan 包月",
    },
    ModelInfo {
        key: "kimi-for-coding",
        display_name: "kimi-for-coding",
        context_window: 256000,
        input_price_per_m: 8.0,
        output_price_per_m: 24.0,
        tool_support: true,
        provider_hint: "moonshot",
        endpoint: "https://api.kimi.com/coding/v1",
        coding_plan: "Kimi Coding Plan 包月",
    },
    // 0813 版 $0.43/$0.87；缓存命中输入 0.025 元
    ModelInfo {
        key: "deepseek-v4-pro",
        display_name: "deepseek-v4-pro",
        context_window: 1048000,
        input_price_per_m: 3.0,
        output_price_per_m: 6.0,
        tool_support: true,
        provider_hint: "deepseek",
        endpoint: "https://api.deepseek.com/v1",
        coding_plan: "",
    },
    // OpenRouter $0.06/$0.13，极致性价比
    ModelInfo {
        key: "deepseek-v4-flash",
        display_name: "deepseek-v4-flash",
        context_window: 1048000,
        input_price_per_m: 0.43,
        output_price_per_m: 0.94,
        tool_support: true,
        provider_hint: "deepseek",
        endpoint: "https://api.deepseek.com/v1",
        coding_plan: "",
    },
    // 推理系
    ModelInfo {
        key: "deepseek-r2",
        display_name: "deepseek-r2",
        context_window: 128000,
        input_price_per_m: 4.0,
        output_price_per_m: 16.0,
        tool_support: true,
        provider_hint: "deepseek",
        endpoint: "https://api.deepseek.com/v1",
        coding_plan: "",
    },
    // OpenRouter $2/$6
    ModelInfo {
        key: "qwen3.8-max",
        display_name: "qwen3.8-max",
        context_window: 1000000,
        input_price_per_m: 14.4,
        output_price_per_m: 43.2,
        tool_support: true,
        provider_hint: "alibaba",
        endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        coding_plan: "",
    },
    // 2.4 万亿参数开源旗舰；OpenRouter $2/$6
    ModelInfo {
        key: "qwen3.8-2.4t",
        display_name: "qwen3.8-2.4t",
        context_window: 1048000,
        input_price_per_m: 14.4,
        output_price_per_m: 43.2,
        tool_support: true,
        provider_hint: "alibaba",
        endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        coding_plan: "",
    },
    // OpenRouter $0.03/$0.13
    ModelInfo {
        key: "qwen3.7-flash",
        display_name: "qwen3.7-flash",
        context_window: 1000000,
        input_price_per_m: 0.22,
        output_price_per_m: 0.94,
        tool_support: true,
        provider_hint: "alibaba",
        endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "doubao-seed-1.6",
        display_name: "doubao-seed-1.6",
        context_window: 256000,
        input_price_per_m: 0.8,
        output_price_per_m: 2.0,
        tool_support: true,
        provider_hint: "volcengine",
        endpoint: "https://ark.cn-beijing.volces.com/api/v3",
        coding_plan: "火山方舟",
    },
    // OpenRouter $0.30/$1.20
    ModelInfo {
        key: "minimax-m3",
        display_name: "minimax-m3",
        context_window: 1048000,
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
        context_window: 1048000,
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
        context_window: 256000,
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
        context_window: 128000,
        input_price_per_m: 1.0,
        output_price_per_m: 4.0,
        tool_support: true,
        provider_hint: "xiaomi",
        endpoint: "https://token-plan-cn.xiaomimimo.com/v1",
        coding_plan: "小米 Token Plan（免费额度大）",
    },
    // $5/$25
    ModelInfo {
        key: "claude-opus-5",
        display_name: "claude-opus-5",
        context_window: 1000000,
        input_price_per_m: 36.0,
        output_price_per_m: 180.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // $10/$50
    ModelInfo {
        key: "claude-opus-5-fast",
        display_name: "claude-opus-5-fast",
        context_window: 1000000,
        input_price_per_m: 72.0,
        output_price_per_m: 360.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // $10/$50，Anthropic 新系列
    ModelInfo {
        key: "claude-fable-5",
        display_name: "claude-fable-5",
        context_window: 1000000,
        input_price_per_m: 72.0,
        output_price_per_m: 360.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // $2/$10
    ModelInfo {
        key: "claude-sonnet-5",
        display_name: "claude-sonnet-5",
        context_window: 1000000,
        input_price_per_m: 14.4,
        output_price_per_m: 72.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // $3/$15
    ModelInfo {
        key: "claude-sonnet-4.6",
        display_name: "claude-sonnet-4.6",
        context_window: 1000000,
        input_price_per_m: 21.6,
        output_price_per_m: 108.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "GitHub Copilot 可用",
    },
    // $1/$5
    ModelInfo {
        key: "claude-haiku-4.5",
        display_name: "claude-haiku-4.5",
        context_window: 200000,
        input_price_per_m: 7.2,
        output_price_per_m: 36.0,
        tool_support: true,
        provider_hint: "anthropic",
        endpoint: "https://api.anthropic.com",
        coding_plan: "",
    },
    // $5/$30，GPT-5.6 天体系旗舰
    ModelInfo {
        key: "gpt-5.6-sol",
        display_name: "gpt-5.6-sol",
        context_window: 1050000,
        input_price_per_m: 36.0,
        output_price_per_m: 216.0,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // $1/$6
    ModelInfo {
        key: "gpt-5.6-terra",
        display_name: "gpt-5.6-terra",
        context_window: 1050000,
        input_price_per_m: 7.2,
        output_price_per_m: 43.2,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // $0.10/$0.60，轻量极速档
    ModelInfo {
        key: "gpt-5.6-luna",
        display_name: "gpt-5.6-luna",
        context_window: 1050000,
        input_price_per_m: 0.72,
        output_price_per_m: 4.32,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // $30/$180，最贵旗舰
    ModelInfo {
        key: "gpt-5.5-pro",
        display_name: "gpt-5.5-pro",
        context_window: 1050000,
        input_price_per_m: 216.0,
        output_price_per_m: 1296.0,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // $0.75/$4.5
    ModelInfo {
        key: "gpt-5.4-mini",
        display_name: "gpt-5.4-mini",
        context_window: 400000,
        input_price_per_m: 5.4,
        output_price_per_m: 32.4,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // $0.20/$1.25
    ModelInfo {
        key: "gpt-5.4-nano",
        display_name: "gpt-5.4-nano",
        context_window: 400000,
        input_price_per_m: 1.4,
        output_price_per_m: 9.0,
        tool_support: true,
        provider_hint: "openai",
        endpoint: "https://api.openai.com/v1",
        coding_plan: "",
    },
    // $2/$12
    ModelInfo {
        key: "gemini-3-pro",
        display_name: "gemini-3-pro",
        context_window: 2000000,
        input_price_per_m: 14.4,
        output_price_per_m: 86.4,
        tool_support: true,
        provider_hint: "google",
        endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
        coding_plan: "",
    },
    // $0.38/$1.88
    ModelInfo {
        key: "gemini-3.7-flash",
        display_name: "gemini-3.7-flash",
        context_window: 1048000,
        input_price_per_m: 2.7,
        output_price_per_m: 13.5,
        tool_support: true,
        provider_hint: "google",
        endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
        coding_plan: "",
    },
    // $2/$6
    ModelInfo {
        key: "grok-4.6",
        display_name: "grok-4.6",
        context_window: 500000,
        input_price_per_m: 14.4,
        output_price_per_m: 43.2,
        tool_support: true,
        provider_hint: "xai",
        endpoint: "https://api.x.ai/v1",
        coding_plan: "",
    },
    // $1.25/$2.50，2M 窗口
    ModelInfo {
        key: "grok-4.20",
        display_name: "grok-4.20",
        context_window: 2000000,
        input_price_per_m: 9.0,
        output_price_per_m: 18.0,
        tool_support: true,
        provider_hint: "xai",
        endpoint: "https://api.x.ai/v1",
        coding_plan: "",
    },
    // $0.10/$0.25，英伟达自研
    ModelInfo {
        key: "nemotron-3.5-lightning",
        display_name: "nemotron-3.5-lightning",
        context_window: 1000000,
        input_price_per_m: 0.72,
        output_price_per_m: 1.8,
        tool_support: true,
        provider_hint: "nvidia",
        endpoint: "https://integrate.api.nvidia.com/v1",
        coding_plan: "",
    },
    // $0.60/$3.60
    ModelInfo {
        key: "nemotron-3-ultra-550b",
        display_name: "nemotron-3-ultra-550b",
        context_window: 512000,
        input_price_per_m: 4.3,
        output_price_per_m: 25.9,
        tool_support: true,
        provider_hint: "nvidia",
        endpoint: "https://integrate.api.nvidia.com/v1",
        coding_plan: "",
    },
    // $0.08/$0.40
    ModelInfo {
        key: "nemotron-3-super-120b",
        display_name: "nemotron-3-super-120b",
        context_window: 1000000,
        input_price_per_m: 0.58,
        output_price_per_m: 2.88,
        tool_support: true,
        provider_hint: "nvidia",
        endpoint: "https://integrate.api.nvidia.com/v1",
        coding_plan: "",
    },
    ModelInfo {
        key: "mock",
        display_name: "mock",
        context_window: 8192,
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
        let info = lookup("Gpt-5.6-sol").unwrap();
        assert_eq!(info.key, "gpt-5.6-sol");
    }

    #[test]
    fn test_lookup_longest_key_wins() {
        // glm-5.2-flash 也包含 glm-5.2，应命中更具体的 flash
        let info = lookup("glm-5.2-flash").unwrap();
        assert_eq!(info.key, "glm-5.2-flash");
        assert_eq!(info.context_window, 128_000);
        let info = lookup("gpt-5.4-mini").unwrap();
        assert_eq!(info.key, "gpt-5.4-mini");
        // kimi-k3 包含 kimi-k2.7-code? 不；qwen3.8-max vs qwen3.8-2.4t 互不包含，各自独立
        let info = lookup("qwen3.8-2.4t-a95b").unwrap();
        assert_eq!(info.key, "qwen3.8-2.4t");
        let info = lookup("claude-opus-5-fast").unwrap();
        assert_eq!(info.key, "claude-opus-5-fast");
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
