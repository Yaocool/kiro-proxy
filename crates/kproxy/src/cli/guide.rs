//! Long-form operational guides kept separate from generated command help.

use anyhow::{anyhow, Result};
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Topic {
    Status,
    Health,
    Version,
    Account,
    Sso,
    Service,
    #[value(name = "apikey")]
    ApiKey,
    Pool,
    Balance,
    Diagnose,
    Subscriptions,
    Tasks,
    Stats,
    Logs,
    Alert,
    Models,
    ModelMap,
    Config,
    Docker,
}

impl Topic {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Health => "health",
            Self::Version => "version",
            Self::Account => "account",
            Self::Sso => "sso",
            Self::Service => "service",
            Self::ApiKey => "apikey",
            Self::Pool => "pool",
            Self::Balance => "balance",
            Self::Diagnose => "diagnose",
            Self::Subscriptions => "subscriptions",
            Self::Tasks => "tasks",
            Self::Stats => "stats",
            Self::Logs => "logs",
            Self::Alert => "alert",
            Self::Models => "models",
            Self::ModelMap => "model-map",
            Self::Config => "config",
            Self::Docker => "docker",
        }
    }
}

const TOPICS: &[(&str, &str)] = &[
    ("status", "服务状态与时间范围"),
    ("health", "健康检查与业务就绪"),
    ("version", "版本和默认上游端点"),
    ("account", "账号管理与确认规则"),
    ("sso", "企业 SSO 单账号与批量导入"),
    ("service", "代理服务生命周期"),
    ("apikey", "API key、限额与用量"),
    ("pool", "账号池状态与评分明细"),
    ("balance", "账号池调度评分原理"),
    ("diagnose", "网络和账号真实推理诊断"),
    ("subscriptions", "企业订阅计划查询"),
    ("tasks", "周期任务状态与手动运行"),
    ("stats", "持久化统计与时间范围"),
    ("logs", "结构化日志、文件和 Trace ID"),
    ("alert", "异常事件和通知目标"),
    ("models", "动态模型发现与解析"),
    ("model-map", "模型映射匹配规则"),
    ("config", "配置编辑、校验和热重载"),
    ("docker", "Docker 运行方式和数据卷"),
];

pub fn print(topic: Option<&str>) -> Result<()> {
    let Some(topic) = topic else {
        println!("可用指南主题：");
        for (name, description) in TOPICS {
            println!("  {name:<16}{description}");
        }
        println!("\n使用 `kproxy guide <topic>` 查看详情，命令参数可用 `kproxy help <命令路径>`。");
        return Ok(());
    };
    let text = match topic {
        "status" => {
            "`kproxy status` 展示 daemon 版本、代理服务、账号池和本次启动后的请求统计；支持 `--since` 或 `--start/--end`，`--watch` 每 2 秒刷新。"
        }
        "health" => {
            "`kproxy health` 检查 daemon 管理面是否可用，供容器和 systemd 健康检查使用；账号或代理服务为空不会让该检查失败。`kproxy ready` 进一步检查业务代理是否已经具备接收请求的条件。"
        }
        "version" => "`kproxy version` 显示 CLI 版本、Rust MSRV 和默认 Kiro 上游端点。",
        "account" => {
            "账号命令包括 list/show/import/add-api-key/export/add-sso/rm/enable/disable/tag/regen-machine-id/refresh/probe/reset-health。删除操作会要求输入 y/yes 二次确认。"
        }
        "balance" => {
            "账号池评分 = active_ratio×weight_active + used_credit_ratio×weight_credit + recent_idle_penalty×weight_idle。\n分数越低越优先；随后加入小幅随机抖动，避免并发请求集中到同一账号。\n用 `kproxy pool --watch --explain` 查看实时评分明细。"
        }
        "pool" => {
            "`kproxy pool --model <model>` 查看当前账号池可用性；`--explain` 展示调度评分，`--watch` 持续刷新。评分原理见 `kproxy guide balance`。"
        }
        "model-map" => {
            "模型映射按 priority 从小到大匹配。source_models 支持 `*`；replace/alias 选首个目标，loadbalance 按 weights 随机。\n用 add/edit/delete 管理规则；`--below-credits-percent` 让规则仅在账号剩余额度低于阈值时全天生效，额度恢复后自动停止命中。"
        }
        "sso" => {
            r#"先在配置中设置 `[sso] start_url = "https://..."`。单账号：`printf '%s\n' "$PASSWORD" | kproxy account add-sso --email user@example.com --password-stdin`。
批量：CSV 仅含 email,password 两列，运行 `kproxy account add-sso --batch accounts.csv -c 1`；也可用 `--batch - < accounts.csv` 从 stdin 读取。`--start-url` 可覆盖全局值，`--headful` 可手工完成额外验证。默认/full 构建包含 SSO。"#
        }
        "service" => {
            "`kproxy service list/show/create/edit/enable/disable/apikeys/delete` 管理独立代理监听。edit 可修改监听并按 API key ID 或名称增删绑定；disable 会保留配置和 key；删除时仅级联删除未共享 key，并要求 y/yes 确认。"
        }
        "config" => {
            "配置默认位于 $KPROXY_HOME/config.toml，修改后热重载；server.host/port、admin.socket 和 TLS 监听变更需要重启。\n`kproxy config list` 列出全部顶层模块及是否允许重置；`show [模块]` 可查看完整配置或单个模块，增加 `--effective` 查看合并默认值后的结果；`edit [模块]` 可编辑完整配置或单个模块，保存时会合并、整体校验并重载。`reset [模块]` 只恢复指定模块，其他配置不变；不指定模块时恢复全部通用配置，并保留 API key、代理服务和告警配置。`validate [file]` 只校验，不应用。"
        }
        "apikey" => {
            "API key 限额采用在途预留：请求进入时预留估算 credits，结束后按上游实际用量结算，避免并发突破限额。\n`kproxy apikey show <ID|名称>` 查看单项，`list --detail` 查看 token/credits 消耗；`limit <ID|名称> --clear` 可恢复不限，`rm`/`delete` 均可删除。日维度、模型、路径和历史可用 `usage` 与 `history` 查询。"
        }
        "diagnose" => {
            "`kproxy diagnose all` 依次检查 CodeWhisperer/AmazonQ/OIDC 端点，并对全部账号发起真实推理；也可分别使用 `kproxy diagnose endpoints` 和 `kproxy diagnose account <id|--all>`。"
        }
        "subscriptions" => {
            "`kproxy subscriptions [account]` 查询上游当前可用的企业订阅计划；省略账号时使用可调度账号。"
        }
        "tasks" => {
            "`kproxy tasks list` 查看 token 刷新、状态探测、统计持久化、模型缓存等周期任务；`kproxy tasks run <name>` 立即执行。"
        }
        "stats" => {
            "`kproxy stats` 默认显示跨 daemon 重启的持久化累计统计；可用 `--since 1h` 或带时区的 `--start/--end` 查询时间段。`--detail` 显示最近请求，并可用 `--by model|account|endpoint` 分组。"
        }
        "logs" => {
            "`kproxy logs show` 查看内存中的结构化请求日志，`follow` 持续跟踪；支持 `--level`、`--account` 和 `--tail`。`kproxy logs trace <TRACE_ID>` 默认跨日期和全部精确级别分片查询完整链路，可用 `--level error` 限定级别。info 文件只包含 INFO，WARN/ERROR 分别写入 warn/error 文件。`kproxy logs files [--level error]` 列出实际日志文件，`logs path` 显示目录、基础路径和当前日志配置。"
        }
        "alert" => {
            "`kproxy alert events` 列出四类异常事件和触发条件，`kproxy alert platforms` 说明 --platform 支持的通知平台和平台专用参数；`kproxy alert config` 查看一次性告警策略。同类型的多账号事件会聚合为一条 Markdown 告警；每个账号或服务恢复后才允许再次告警。`kproxy alert add/edit/delete/list/test/logs` 管理告警目标。"
        }
        "models" => {
            "`kproxy models list` 显示账号自动探测到的 Kiro 模型；`--refresh` 先立即刷新缓存，`--mapped` 同时显示显式映射结果。`kproxy models resolve <MODEL_ID>` 使用当前配置、账号额度和账号模型缓存，显示显式映射与最终 Kiro 模型；可配合 `--api-key` 和 `--refresh`。"
        }
        "docker" => {
            "默认 `docker compose up -d --build` 构建 runtime-full，启用全部 feature 并包含 Chromium SSO 运行时。数据保存在 kproxy-data 命名卷。"
        }
        _ => {
            return Err(anyhow!(
                "未知指南主题 {topic}；可用主题：{}",
                TOPICS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    };
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_topics_match_clap_values() {
        let values = Topic::value_variants()
            .iter()
            .map(|topic| topic.as_str())
            .collect::<Vec<_>>();
        let documented = TOPICS.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        assert_eq!(values, documented);
    }
}
