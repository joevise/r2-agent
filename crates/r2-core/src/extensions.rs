//! Extension 体系（v0.14.0 模块化阶段二）：把「工具 + prompt 段 + hooks」打包成
//! 命名单元，编译期注册表 + AGENT.toml 声明式裁剪——分身能力可裁剪。
//! Rust 编译型约束下刻意不做动态加载/热重载/文件系统扫描：Extension 是
//! 编译进二进制的注册表条目（借鉴 pi-durable「状态和能力分离」）。

use crate::tools::Tool;
use std::sync::Arc;

/// 工具构造上下文（Extension 造工具所需的参数，不暴露 sandbox 细节）
pub struct ExtCtx<'a> {
    pub work_dir: &'a str,
    pub session_dir: &'a str,
    /// MCP 配置写回路径（mcp_admin 需要）
    pub config_path: Option<&'a str>,
}

/// Extension：命名单元 = 工具 + prompt 段 + hooks（三者皆可空）
pub trait Extension: Send + Sync {
    /// 清单里的标识（如 "evolution"）
    fn name(&self) -> &str;
    /// prompt 段标题（如 "成长系统"）；无 prompt 段可空
    fn title(&self) -> &str;
    /// 一句话说明（将来 UI/文档用）
    fn description(&self) -> &str;
    /// 每条一段，注入 system prompt
    fn prompt_sections(&self) -> Vec<String> {
        vec![]
    }
    fn tools(&self, _ctx: &ExtCtx) -> Vec<Box<dyn Tool>> {
        vec![]
    }
    fn hooks(&self) -> Vec<Arc<dyn crate::hooks::Hook>> {
        vec![]
    }
}

/// 内置注册表（编译期目录）
pub fn builtin() -> Vec<Arc<dyn Extension>> {
    vec![
        Arc::new(EvolutionExtension),
        Arc::new(TasksExtension),
        Arc::new(SummonExtension),
        Arc::new(McpAdminExtension),
    ]
}

/// 按清单顺序 resolve：未知名字静默跳过 + eprintln 警告（配置容错，不 panic）。
/// 注入循环顺序必须稳定（= 清单顺序），防 system prompt 前缀抖动击穿 KV-cache。
pub fn resolve(names: &[String]) -> Vec<Arc<dyn Extension>> {
    let all = builtin();
    let mut out = Vec::new();
    for n in names {
        match all.iter().find(|e| e.name() == n) {
            Some(e) => out.push(e.clone()),
            None => eprintln!("[extensions] 未知 extension「{n}」，已跳过"),
        }
    }
    out
}

/// 按 agent 配置 resolve：evolution_enabled 是 v0.13 遗留兼容开关，语义优先级
/// 低于 extensions——false 时即便清单列了 evolution 也过滤掉（prompt 段与
/// EvolutionHook 同步消失，与 v0.13 关开关的行为等价）。
pub fn resolve_for(agent: &crate::config::AgentConfig) -> Vec<Arc<dyn Extension>> {
    let mut exts = resolve(&agent.extensions);
    if !agent.evolution_enabled {
        exts.retain(|e| e.name() != "evolution");
    }
    exts
}

/// 成长系统 prompt 段正文（v0.7.2 起内嵌于 agent.rs，v0.14.0 原样迁入；
/// 字节级不变——默认配置下 system prompt 全文与 v0.13 一致）
const EVOLUTION_SECTION: &str = "\
你有自我成长机制，用户口头表达时请主动使用：\n\
· 目标代笔：用户说出目标/身份/期望（如「我希望你成为…」）时，把原话忠实写入 ~/.r2/GOAL.md\n\
（用 bash 写入），写入前先向用户复述确认。未经用户明确表达，绝不创建或修改目标。\n\
· 学习沉淀：完成有意义的学习（方法论/框架/流程/领域知识）后，主动蒸馏成技能文件\n\
写入 ~/.r2/skills/<名字>/SKILL.md（frontmatter 含 name 和 description），\n\
下次会话自动生效——学习不沉淀等于白学。\n\
· 你的目标与技能会出现在成长档案里，持续积累成为你的一部分。";

/// evolution：成长系统 prompt 段 + EvolutionHook（反思/草稿/技能追踪）
struct EvolutionExtension;

impl Extension for EvolutionExtension {
    fn name(&self) -> &str {
        "evolution"
    }
    fn title(&self) -> &str {
        "成长系统"
    }
    fn description(&self) -> &str {
        "自我成长机制：目标代笔 / 学习沉淀 / 回合反思与成长记账"
    }
    fn prompt_sections(&self) -> Vec<String> {
        vec![EVOLUTION_SECTION.to_string()]
    }
    fn hooks(&self) -> Vec<Arc<dyn crate::hooks::Hook>> {
        vec![Arc::new(crate::evolution::EvolutionHook::new())]
    }
}

/// tasks：后台任务工具
struct TasksExtension;

impl Extension for TasksExtension {
    fn name(&self) -> &str {
        "tasks"
    }
    fn title(&self) -> &str {
        ""
    }
    fn description(&self) -> &str {
        "后台任务管理（task 工具）"
    }
    fn tools(&self, _ctx: &ExtCtx) -> Vec<Box<dyn Tool>> {
        vec![Box::new(crate::tools::task_tool::TaskTool::new())]
    }
}

/// summon：分身召唤工具
struct SummonExtension;

impl Extension for SummonExtension {
    fn name(&self) -> &str {
        "summon"
    }
    fn title(&self) -> &str {
        ""
    }
    fn description(&self) -> &str {
        "分身召唤（summon 工具，起草 pending 档案）"
    }
    fn tools(&self, _ctx: &ExtCtx) -> Vec<Box<dyn Tool>> {
        vec![Box::new(crate::tools::summon::SummonTool::new())]
    }
}

/// mcp-admin：MCP server 管理工具（需要配置写回路径）
struct McpAdminExtension;

impl Extension for McpAdminExtension {
    fn name(&self) -> &str {
        "mcp-admin"
    }
    fn title(&self) -> &str {
        ""
    }
    fn description(&self) -> &str {
        "MCP 外部工具服务器管理（mcp 工具，写回 MCP.toml/config.toml）"
    }
    fn tools(&self, ctx: &ExtCtx) -> Vec<Box<dyn Tool>> {
        vec![Box::new(crate::tools::mcp_admin::McpAdminTool::new(
            ctx.config_path,
        ))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExtCtx<'static> {
        ExtCtx {
            work_dir: "/tmp/r2-ext-test",
            session_dir: "/tmp/r2-ext-test-sessions",
            config_path: Some("/tmp/r2-ext-test/config.toml"),
        }
    }

    #[test]
    fn test_builtin_registry_names() {
        let names: Vec<String> = builtin().iter().map(|e| e.name().to_string()).collect();
        assert_eq!(names, vec!["evolution", "tasks", "summon", "mcp-admin"]);
        for e in builtin() {
            assert!(!e.description().is_empty(), "{} 缺 description", e.name());
        }
    }

    #[test]
    fn test_resolve_follows_list_order() {
        // resolve 顺序 = 清单顺序（不是注册表顺序），保证 prompt 前缀字节级稳定
        let names = vec!["summon".to_string(), "evolution".to_string()];
        let out = resolve(&names);
        let got: Vec<&str> = out.iter().map(|e| e.name()).collect();
        assert_eq!(got, vec!["summon", "evolution"]);
    }

    #[test]
    fn test_resolve_unknown_skipped_no_panic() {
        // 未知名字：eprintln 警告 + 静默跳过，不 panic
        let names = vec!["no-such-ext".to_string(), "tasks".to_string()];
        let out = resolve(&names);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name(), "tasks");
        // 全未知 → 空
        assert!(resolve(&["ghost".to_string()]).is_empty());
    }

    #[test]
    fn test_resolve_for_evolution_enabled_filter() {
        let mut agent = crate::config::AgentConfig::default();
        // 默认全开：4 个全在
        assert_eq!(resolve_for(&agent).len(), 4);
        // v0.13 兼容开关：false 时过滤 evolution（即便清单里有）
        agent.evolution_enabled = false;
        let out = resolve_for(&agent);
        assert_eq!(out.len(), 3);
        assert!(!out.iter().any(|e| e.name() == "evolution"));
    }

    #[test]
    fn test_extension_contents() {
        let all = builtin();
        let by_name = |n: &str| all.iter().find(|e| e.name() == n).unwrap().clone();
        let c = ctx();

        // evolution：1 段 prompt + 1 个 hook，无工具
        let evo = by_name("evolution");
        assert_eq!(evo.title(), "成长系统");
        let sections = evo.prompt_sections();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("自我成长机制"));
        assert_eq!(evo.hooks().len(), 1);
        assert!(evo.tools(&c).is_empty());

        // tasks / summon / mcp-admin：各 1 个工具，无 prompt 段、无 hooks
        let tool_name = |e: &Arc<dyn Extension>| {
            let tools = e.tools(&c);
            assert_eq!(tools.len(), 1);
            assert!(e.prompt_sections().is_empty());
            assert!(e.hooks().is_empty());
            tools[0].name().to_string()
        };
        assert_eq!(tool_name(&by_name("tasks")), "task");
        assert_eq!(tool_name(&by_name("summon")), "summon");
        assert_eq!(tool_name(&by_name("mcp-admin")), "mcp");
    }

    #[test]
    fn test_default_extensions_resolve_full_set() {
        // 默认清单 = 全装（v0.13 行为等价）：4 extension、3 个工具、1 hook
        let out = resolve(&crate::config::default_extensions());
        assert_eq!(out.len(), 4);
        let c = ctx();
        let n_tools: usize = out.iter().map(|e| e.tools(&c).len()).sum();
        let n_hooks: usize = out.iter().map(|e| e.hooks().len()).sum();
        assert_eq!(n_tools, 3);
        assert_eq!(n_hooks, 1);
    }
}
