//! Pool, diagnostics, statistics, API-key and alert commands.

use anyhow::{anyhow, Context, Result};
use clap::{Subcommand, ValueEnum};
use kproxy_core::paths::Paths;
use kproxy_ipc::protocol::method;
use kproxy_ipc::protocol::{
    ConfigPathResult, ConfigReloadResult, ConfigShowResult, LogFilesResult, LogTraceResult,
    ModelResolutionResult, ProxyServiceApiKeysResult, ProxyServiceCreateResult,
    ProxyServiceDeleteResult, ProxyServiceListResult,
};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

use crate::client::AdminClient;
use crate::output::{format_timestamp, print_json, render_table};
use crate::ModelMapCommand;

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// 列出 API 代理服务。
    #[command(after_help = "示例：\n  kproxy service list\n  kproxy --json service list")]
    List,
    /// 显示单个 API 代理服务详情。
    #[command(
        after_help = "示例：\n  kproxy service show main\n  kproxy --json service show svc_abcd"
    )]
    Show {
        /// 服务 ID 或名称。
        service: String,
    },
    /// 创建并启动服务，同时生成首个 API key。
    #[command(
        after_help = "示例：\n  kproxy service create --name main\n  kproxy service create --name team --host 127.0.0.1 --port 5581"
    )]
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        api_key_name: Option<String>,
        #[arg(long, default_value = "sk")]
        api_key_format: String,
    },
    /// 修改服务名称、监听地址、端口或绑定的 API key。
    #[command(
        after_help = "API key 参数接受 ID 或名称，可重复使用。\n\n示例：\n  kproxy service edit main --host 127.0.0.1 --port 5581\n  kproxy service edit main --add-api-key ci\n  kproxy service edit main --remove-api-key ak_ab12"
    )]
    Edit {
        /// 当前服务 ID 或名称。
        service: String,
        /// 新服务名称。
        #[arg(long)]
        rename: Option<String>,
        /// 新监听地址。
        #[arg(long)]
        host: Option<String>,
        /// 新监听端口。
        #[arg(long)]
        port: Option<u16>,
        /// 增加绑定的 API key ID 或名称，可重复或逗号分隔。
        #[arg(long, value_delimiter = ',', value_name = "KEY")]
        add_api_key: Vec<String>,
        /// 移除绑定的 API key ID 或名称，可重复或逗号分隔。
        #[arg(long, value_delimiter = ',', value_name = "KEY")]
        remove_api_key: Vec<String>,
    },
    /// 启动已停用的 API 代理服务。
    #[command(after_help = "示例：\n  kproxy service enable main")]
    Enable {
        /// 服务 ID 或名称。
        service: String,
    },
    /// 停止并停用 API 代理服务，但保留配置和 API key。
    #[command(after_help = "示例：\n  kproxy service disable main")]
    Disable {
        /// 服务 ID 或名称。
        service: String,
    },
    /// 删除并停止服务；一并删除未被其他服务共享的 API key。
    #[command(
        name = "delete",
        visible_alias = "rm",
        after_help = "示例：\n  kproxy service delete main\n  kproxy service rm svc_abcd\n\n执行前需输入 y 或 yes 确认。"
    )]
    Delete {
        /// 服务 ID 或名称。
        service: String,
    },
    /// 查看服务绑定的 API key；明文需要显式授权输出。
    #[command(
        name = "apikeys",
        after_help = "示例：\n  kproxy service apikeys main\n  kproxy service apikeys svc_abcd --show-secret"
    )]
    ApiKeys {
        /// 服务 ID 或名称。
        service: String,
        /// 输出 API key 明文。注意终端记录和 CI 日志泄露风险。
        #[arg(long)]
        show_secret: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ApiKeyCommand {
    /// 列出全部 API key；默认显示汇总，--detail 增加逐 key 用量。
    #[command(
        after_help = "示例：\n  kproxy apikey list\n  kproxy apikey list --detail\n  kproxy --json apikey list --detail"
    )]
    List {
        /// 展示每个 API key 的 token/credits 消耗明细。
        #[arg(long)]
        detail: bool,
    },
    /// 显示单个 API key 的配置和累计用量，不显示密钥明文。
    #[command(
        after_help = "参数接受 API key ID 或名称。\n\n示例：\n  kproxy apikey show ci\n  kproxy --json apikey show ak_ab12"
    )]
    Show { id: String },
    /// 创建 API key；明文只在创建结果中显示一次。
    #[command(
        after_help = "示例：\n  kproxy apikey add --name ci\n  kproxy apikey add --name team --credits-limit 100"
    )]
    Add {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "sk")]
        format: String,
        #[arg(long)]
        credits_limit: Option<f64>,
    },
    /// 删除 API key，执行前需输入 y 或 yes 确认。
    #[command(
        visible_alias = "delete",
        after_help = "参数接受 API key ID 或名称。\n\n示例：\n  kproxy apikey rm ak_ab12\n  kproxy apikey delete ci\n\n执行前需输入 y 或 yes 确认。"
    )]
    Rm { id: String },
    /// 启用 API key。
    #[command(
        after_help = "示例：\n  kproxy apikey enable ak_ab12\n  kproxy --json apikey enable ak_ab12"
    )]
    Enable { id: String },
    /// 停用 API key，但保留配置与历史用量。
    #[command(
        after_help = "示例：\n  kproxy apikey disable ak_ab12\n  kproxy --json apikey disable ak_ab12"
    )]
    Disable { id: String },
    /// 设置或清除 API key 的累计 Credits 上限。
    #[command(
        after_help = "参数接受 API key ID 或名称。`--clear` 恢复为不限；`--credits 0` 会阻止任何新消耗。\n\n示例：\n  kproxy apikey limit ci --credits 100\n  kproxy apikey limit ci --clear"
    )]
    Limit {
        id: String,
        #[arg(long, required_unless_present = "clear", conflicts_with = "clear")]
        credits: Option<f64>,
        /// 删除累计 credits 上限，恢复为不限。
        #[arg(long)]
        clear: bool,
    },
    /// 查看 API key 的聚合与分维度用量。
    #[command(
        after_help = "示例：\n  kproxy apikey usage ak_ab12\n  kproxy --json apikey usage ak_ab12"
    )]
    Usage { id: String },
    /// 查看 API key 的最近请求历史。
    #[command(
        after_help = "示例：\n  kproxy apikey history ak_ab12\n  kproxy apikey history ak_ab12 --tail 200"
    )]
    History {
        id: String,
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
    /// 清除 API key 的全部累计用量，执行前需确认。
    #[command(
        after_help = "示例：\n  kproxy apikey reset-usage ak_ab12\n\n执行前需输入 y 或 yes 确认。"
    )]
    ResetUsage { id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AlertEvent {
    /// 单个账号触发剩余额度保护并暂停调度。
    AccountCreditProtected,
    /// 单个账号额度完全耗尽。
    AccountQuotaExhausted,
    /// API 代理服务的全部启用账号额度完全耗尽。
    ServiceQuotaExhausted,
    /// 账号 Token 自动或请求触发刷新失败。
    TokenRefreshFailed,
}

impl AlertEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::AccountCreditProtected => "account-credit-protected",
            Self::AccountQuotaExhausted => "account-quota-exhausted",
            Self::ServiceQuotaExhausted => "service-quota-exhausted",
            Self::TokenRefreshFailed => "token-refresh-failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AlertPlatform {
    /// 钉钉群机器人 Webhook。
    Dingtalk,
    /// 企业微信群机器人 Webhook。
    #[value(name = "wechat-work", alias = "wechat")]
    WechatWork,
    /// 飞书群机器人 Webhook。
    Feishu,
    /// Telegram Bot API。
    Telegram,
    /// Discord Webhook。
    Discord,
    /// 自定义 Webhook，可配置消息模板。
    Custom,
}

impl AlertPlatform {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dingtalk => "dingtalk",
            Self::WechatWork => "wechat-work",
            Self::Feishu => "feishu",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AlertCommand {
    /// 查看一次性异常告警策略。
    #[command(after_help = "示例：\n  kproxy alert config")]
    Config,
    /// 列出可订阅事件及其触发条件。
    #[command(after_help = "示例：\n  kproxy alert events\n  kproxy --json alert events")]
    Events,
    /// 列出支持的通知平台及平台专用参数。
    #[command(after_help = "示例：\n  kproxy alert platforms\n  kproxy --json alert platforms")]
    Platforms,
    /// 列出全部告警通知目标。
    #[command(after_help = "示例：\n  kproxy alert list\n  kproxy --json alert list")]
    List,
    /// 添加告警目标。
    #[command(
        after_help = "`--platform` 表示接收 Webhook 的通知平台；先用 `kproxy alert platforms` 查看平台说明。\n用 `kproxy alert events` 查看事件说明。多选可重复传入 --event，也可使用逗号分隔。\n\n示例：\n  kproxy alert add --name alerts --platform dingtalk --webhook-url 'https://oapi.dingtalk.com/robot/send?access_token=replace-me' --dingtalk-sign 'SEC-replace-me' --event account-credit-protected --event account-quota-exhausted\n  kproxy alert add --name alerts --platform feishu --webhook-url https://example/hook --event account-credit-protected,account-quota-exhausted,service-quota-exhausted"
    )]
    Add {
        /// 告警目标的唯一名称。
        #[arg(long)]
        name: String,
        /// Webhook 接收平台。旧参数名 --kind 继续兼容。
        #[arg(long = "platform", visible_alias = "kind", value_name = "PLATFORM")]
        platform: AlertPlatform,
        /// Webhook 接收地址。
        #[arg(long = "webhook-url", visible_alias = "url", value_name = "URL")]
        webhook_url: String,
        /// 要订阅的异常事件；可重复传入或使用逗号分隔。
        #[arg(
            long = "event",
            value_delimiter = ',',
            required = true,
            value_name = "EVENT"
        )]
        events: Vec<AlertEvent>,
        /// 创建目标但暂不启用。
        #[arg(long)]
        disabled: bool,
        /// 钉钉机器人加签密钥；仅启用加签时需要。
        #[arg(long)]
        dingtalk_sign: Option<String>,
        /// Telegram 目标的 chat ID；platform=telegram 时必填。
        #[arg(long)]
        telegram_chat_id: Option<String>,
        /// 自定义 Webhook 消息模板；支持 {{event}}、{{title}}、{{message}}。
        #[arg(long)]
        custom_template: Option<String>,
    },
    /// 编辑告警目标。
    #[command(
        after_help = "目标名称既可写成位置参数，也可通过 --name 指定。\n`--event` 会整体替换原订阅；可重复传入或使用逗号分隔。\n\n示例：\n  kproxy alert edit alerts --webhook-url https://example/new-hook\n  kproxy alert edit alerts --dingtalk-sign 'SEC-replace-me'\n  kproxy alert edit --name alerts --event token-refresh-failed --event service-quota-exhausted\n  kproxy alert edit --name alerts --platform feishu"
    )]
    Edit {
        /// 当前名称；也可使用 --name。
        #[arg(value_name = "NAME", required_unless_present = "name")]
        target: Option<String>,
        /// 当前名称；与位置参数 NAME 二选一。
        #[arg(long, value_name = "NAME", conflicts_with = "target")]
        name: Option<String>,
        /// 修改目标名称。
        #[arg(long)]
        rename: Option<String>,
        /// 修改 Webhook 接收平台。旧参数名 --kind 继续兼容。
        #[arg(long = "platform", visible_alias = "kind", value_name = "PLATFORM")]
        platform: Option<AlertPlatform>,
        /// 修改 Webhook 接收地址。
        #[arg(long = "webhook-url", visible_alias = "url", value_name = "URL")]
        webhook_url: Option<String>,
        /// 整体替换要订阅的异常事件；可重复传入或使用逗号分隔。
        #[arg(
            long = "event",
            value_delimiter = ',',
            conflicts_with = "clear_events",
            value_name = "EVENT"
        )]
        events: Vec<AlertEvent>,
        /// 清空事件订阅；目标将不再接收告警。
        #[arg(long)]
        clear_events: bool,
        /// 启用目标。
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// 停用目标，但保留配置。
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        /// 设置钉钉机器人加签密钥。
        #[arg(long, conflicts_with = "clear_dingtalk_sign")]
        dingtalk_sign: Option<String>,
        /// 删除钉钉机器人加签密钥。
        #[arg(long)]
        clear_dingtalk_sign: bool,
        /// 设置 Telegram chat ID。
        #[arg(long, conflicts_with = "clear_telegram_chat_id")]
        telegram_chat_id: Option<String>,
        /// 删除 Telegram chat ID。
        #[arg(long)]
        clear_telegram_chat_id: bool,
        /// 设置自定义 Webhook 消息模板。
        #[arg(long, conflicts_with = "clear_custom_template")]
        custom_template: Option<String>,
        /// 删除自定义 Webhook 消息模板。
        #[arg(long)]
        clear_custom_template: bool,
    },
    /// 删除告警目标，执行前需输入 y 或 yes 确认。
    #[command(name = "delete", visible_alias = "rm")]
    Delete { name: String },
    /// 向一个或全部目标发送测试通知。
    #[command(after_help = "示例：\n  kproxy alert test alerts\n  kproxy alert test --all")]
    Test {
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// 查看最近的告警投递记录。
    #[command(after_help = "示例：\n  kproxy alert logs\n  kproxy alert logs --tail 200")]
    Logs {
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
}

pub async fn simple_rpc(
    client: &mut AdminClient,
    method_name: &str,
    params: serde_json::Value,
    json: bool,
) -> Result<()> {
    let value: serde_json::Value = client.call(method_name, params).await?;
    if json {
        print_json(&value)?;
    } else {
        print_human_value(&value);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PoolOutput {
    model: String,
    #[serde(default)]
    queued: usize,
    #[serde(default)]
    accounts: Vec<PoolAccountOutput>,
    #[serde(default)]
    scoring: Option<PoolScoringOutput>,
}

#[derive(Debug, Deserialize)]
struct PoolScoringOutput {
    weight_active: f64,
    weight_credit: f64,
    weight_idle: f64,
    max_concurrent_per_account: usize,
    idle_window_ms: u64,
}

#[derive(Debug, Deserialize)]
struct PoolAccountOutput {
    account_id: String,
    #[serde(default)]
    account_name: String,
    score: Option<f64>,
    #[serde(default)]
    active_factor: f64,
    #[serde(default)]
    credit_factor: f64,
    #[serde(default)]
    idle_factor: f64,
    #[serde(default)]
    eligible: bool,
    #[serde(default)]
    reason: String,
}

pub async fn show_pool(
    client: &mut AdminClient,
    model: &str,
    explain: bool,
    json: bool,
) -> Result<()> {
    let value: serde_json::Value = client
        .call(method::POOL, serde_json::json!({"model":model}))
        .await?;
    if json {
        return print_json(&value);
    }
    let output =
        serde_json::from_value::<PoolOutput>(value).context("daemon 返回的账号池评分无效")?;
    print!("{}", render_pool_output(&output, explain));
    Ok(())
}

fn render_pool_output(pool: &PoolOutput, explain: bool) -> String {
    let eligible = pool
        .accounts
        .iter()
        .filter(|account| account.eligible)
        .count();
    let unavailable = pool.accounts.len().saturating_sub(eligible);
    let mut output = format!(
        "模型 {}  排队 {}  可调度 {}/{}\n",
        pool.model,
        pool.queued,
        eligible,
        pool.accounts.len()
    );

    let mut unavailable_by_reason = BTreeMap::<&str, usize>::new();
    for account in pool.accounts.iter().filter(|account| !account.eligible) {
        *unavailable_by_reason
            .entry(account.reason.as_str())
            .or_default() += 1;
    }
    if !unavailable_by_reason.is_empty() {
        output.push_str("不可调度：");
        output.push_str(
            &unavailable_by_reason
                .into_iter()
                .map(|(reason, count)| format!("{} {count}", pool_reason_label(reason)))
                .collect::<Vec<_>>()
                .join("，"),
        );
        output.push('\n');
    }
    output.push('\n');

    let has_names = pool
        .accounts
        .iter()
        .any(|account| !account.account_name.trim().is_empty());
    if explain {
        let rows = pool
            .accounts
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let mut row = vec![
                    if account.eligible {
                        (index + 1).to_string()
                    } else {
                        "-".into()
                    },
                    account.account_id.clone(),
                ];
                if has_names {
                    row.push(short_pool_name(&account.account_name));
                }
                row.extend([
                    if account.eligible {
                        "可调度".into()
                    } else {
                        pool_reason_label(&account.reason).into()
                    },
                    format_pool_score(account.score),
                    format_pool_factor(account.active_factor, account.eligible),
                    format_pool_factor(account.credit_factor, account.eligible),
                    format_pool_factor(account.idle_factor, account.eligible),
                ]);
                row
            })
            .collect::<Vec<_>>();
        let mut headers = vec!["排名", "账号"];
        if has_names {
            headers.push("名称");
        }
        headers.extend(["状态", "评分", "并发", "额度", "近期使用"]);
        output.push_str(&render_table(&headers, &rows));
    } else {
        let rows = pool
            .accounts
            .iter()
            .filter(|account| account.eligible)
            .enumerate()
            .map(|(index, account)| {
                let mut row = vec![(index + 1).to_string(), account.account_id.clone()];
                if has_names {
                    row.push(short_pool_name(&account.account_name));
                }
                row.push(format_pool_score(account.score));
                row
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            output.push_str("暂无可调度账号。\n");
        } else {
            let mut headers = vec!["排名", "账号"];
            if has_names {
                headers.push("名称");
            }
            headers.push("评分");
            output.push_str(&render_table(&headers, &rows));
        }
        if unavailable > 0 {
            output.push_str(&format!(
                "提示：已折叠 {unavailable} 个不可调度账号；使用 --explain 查看详情。\n"
            ));
        }
    }
    output.push_str(&render_pool_score_help(pool.scoring.as_ref(), explain));
    output
}

fn render_pool_score_help(scoring: Option<&PoolScoringOutput>, explain: bool) -> String {
    if !explain {
        return "评分说明：越低越优；综合并发压力、额度使用率和近期使用情况计算。使用 --explain 查看公式。\n".into();
    }

    let Some(scoring) = scoring else {
        return "\n评分说明：越低越优；综合并发压力、额度使用率和近期使用情况计算。\n".into();
    };
    let concurrent_baseline = if scoring.max_concurrent_per_account == 0 {
        "10（未设置单账号上限时的归一化基准）".into()
    } else {
        scoring.max_concurrent_per_account.to_string()
    };
    format!(
        "\n评分说明：越低越优；评分 = 并发 × {} + 额度 × {} + 近期使用 × {}。\n\
         因子说明：并发 = 活跃请求数 ÷ {concurrent_baseline}；额度 = 已用额度 ÷ 总额度；近期使用 = 刚使用时 100%，空闲 {}后降为 0%。\n\
         调度说明：完全同分时会加入极小随机量打破平局。\n",
        format_pool_weight(scoring.weight_active),
        format_pool_weight(scoring.weight_credit),
        format_pool_weight(scoring.weight_idle),
        format_pool_duration(scoring.idle_window_ms),
    )
}

fn format_pool_weight(weight: f64) -> String {
    if weight.is_finite() {
        format!("{weight:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    } else {
        "-".into()
    }
}

fn format_pool_duration(duration_ms: u64) -> String {
    if duration_ms > 0 && duration_ms.is_multiple_of(60_000) {
        format!("{} 分钟", duration_ms / 60_000)
    } else if duration_ms > 0 && duration_ms.is_multiple_of(1_000) {
        format!("{} 秒", duration_ms / 1_000)
    } else {
        format!("{duration_ms} 毫秒")
    }
}

fn format_pool_score(score: Option<f64>) -> String {
    score
        .filter(|score| score.is_finite())
        .map(|score| format!("{score:.4}"))
        .unwrap_or_else(|| "-".into())
}

fn format_pool_factor(factor: f64, eligible: bool) -> String {
    if eligible && factor.is_finite() {
        format!("{:.1}%", factor * 100.0)
    } else {
        "-".into()
    }
}

fn pool_reason_label(reason: &str) -> &'static str {
    match reason {
        "disabled" => "已停用",
        "exhausted" => "额度耗尽",
        "low_credit" => "低额度保护",
        "cooling" => "冷却中",
        "banned" => "已封禁",
        "refreshing" => "刷新中",
        "model_unavailable" => "模型不支持",
        "available" | "" => "不可调度",
        _ => "其他",
    }
}

fn short_pool_name(name: &str) -> String {
    const MAX_CHARS: usize = 28;
    let mut characters = name.chars();
    let prefix = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else if prefix.is_empty() {
        "-".into()
    } else {
        prefix
    }
}

mod observability;

use observability::print_human_value;
#[cfg(test)]
use observability::{
    host_log_path, log_account, log_model_route, populate_host_log_paths, LogModelRoute,
};
pub use observability::{
    parse_duration, parse_timestamp, show_log_files, show_logs, show_stats, show_trace_logs,
};

mod alert;

pub use alert::{run_alert, show_alert_events, show_alert_platforms};

mod apikey;

use apikey::format_credits;
pub use apikey::run_apikey;
#[cfg(test)]
use apikey::{apikey_list_json, ApiKeyListEntry, ApiKeyListSummary};

pub async fn run_service(
    client: &mut AdminClient,
    command: ServiceCommand,
    json: bool,
) -> Result<()> {
    match command {
        ServiceCommand::List => {
            let result: ProxyServiceListResult = client
                .call(method::SERVICE_LIST, serde_json::json!({}))
                .await?;
            if json {
                print_json(&result)?;
            } else if result.services.is_empty() {
                println!("暂无 API 代理服务。使用 `kproxy service create --name <名称>` 创建。");
            } else {
                let rows = result
                    .services
                    .into_iter()
                    .map(|service| {
                        vec![
                            service.id,
                            service.name,
                            format!("{}:{}", service.host, service.port),
                            if service.running {
                                "running".into()
                            } else if service.enabled {
                                "error".into()
                            } else {
                                "disabled".into()
                            },
                            service.api_key_ids.len().to_string(),
                            service.error.unwrap_or_default(),
                        ]
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    render_table(&["ID", "名称", "监听", "状态", "API Keys", "错误"], &rows)
                );
            }
            Ok(())
        }
        ServiceCommand::Show { service } => show_service(client, &service, json).await,
        ServiceCommand::Create {
            name,
            host,
            port,
            api_key_name,
            api_key_format,
        } => {
            let result: ProxyServiceCreateResult = client
                .call(
                    method::SERVICE_CREATE,
                    serde_json::json!({
                        "name":name,
                        "host":host,
                        "port":port,
                        "api_key_name":api_key_name,
                        "api_key_format":api_key_format
                    }),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "已创建并启动 {} ({})，监听 {}:{}",
                    result.service.id,
                    result.service.name,
                    result.service.host,
                    result.service.port
                );
                println!(
                    "已创建默认 API key {} ({})：\n{}\n可用 `kproxy service apikeys {} --show-secret` 再次查看。",
                    result.api_key.id,
                    result.api_key.name,
                    result.api_key.key,
                    result.service.id
                );
            }
            Ok(())
        }
        ServiceCommand::Edit {
            service,
            rename,
            host,
            port,
            add_api_key,
            remove_api_key,
        } => {
            if rename.is_none()
                && host.is_none()
                && port.is_none()
                && add_api_key.is_empty()
                && remove_api_key.is_empty()
            {
                return Err(anyhow!(
                    "没有指定修改项；请使用 --rename、--host、--port、--add-api-key 或 --remove-api-key"
                ));
            }
            let add_api_key = resolve_api_key_ids(client, &add_api_key).await?;
            let remove_api_key = resolve_api_key_ids(client, &remove_api_key).await?;
            let result_selector = rename.clone().unwrap_or_else(|| service.clone());
            mutate_config_array(client, "proxy_service", |array| {
                let table = find_service_table_mut(array, &service)?;
                replace_optional_string(table, "name", rename.as_deref());
                replace_optional_string(table, "host", host.as_deref());
                if let Some(port) = port {
                    table.insert("port".into(), toml::Value::Integer(i64::from(port)));
                }
                let key_ids = table
                    .entry("api_key_ids")
                    .or_insert_with(|| toml::Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or_else(|| anyhow!("proxy service api_key_ids must be an array"))?;
                for key_id in add_api_key {
                    if !key_ids.iter().any(|value| value.as_str() == Some(&key_id)) {
                        key_ids.push(toml::Value::String(key_id));
                    }
                }
                key_ids.retain(|value| {
                    !value
                        .as_str()
                        .is_some_and(|id| remove_api_key.iter().any(|removed| removed == id))
                });
                Ok(())
            })
            .await?;
            if json {
                show_service(client, &result_selector, true).await
            } else {
                println!("已更新 API 代理服务 {result_selector}");
                Ok(())
            }
        }
        ServiceCommand::Enable { service } => {
            set_service_enabled(client, &service, true).await?;
            if json {
                show_service(client, &service, true).await
            } else {
                println!("已启用 API 代理服务 {service}");
                Ok(())
            }
        }
        ServiceCommand::Disable { service } => {
            set_service_enabled(client, &service, false).await?;
            if json {
                show_service(client, &service, true).await
            } else {
                println!("已停用 API 代理服务 {service}");
                Ok(())
            }
        }
        ServiceCommand::Delete { service } => {
            if !crate::commands::confirm(&format!(
                "确认删除 API 代理服务 {service} 及其专用 API key？"
            ))
            .await?
            {
                println!("已取消");
                return Ok(());
            }
            let result: ProxyServiceDeleteResult = client
                .call(
                    method::SERVICE_DELETE,
                    serde_json::json!({"service":service}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "已停止并删除 API 代理服务 {} ({})，同时删除 {} 个专用 API key。",
                    result.service_id,
                    result.service_name,
                    result.deleted_api_key_ids.len()
                );
                if !result.retained_api_key_ids.is_empty() {
                    println!(
                        "{} 个由其他服务共享的 API key 已保留：{}",
                        result.retained_api_key_ids.len(),
                        result.retained_api_key_ids.join(",")
                    );
                }
            }
            Ok(())
        }
        ServiceCommand::ApiKeys {
            service,
            show_secret,
        } => {
            let result: ProxyServiceApiKeysResult = client
                .call(
                    method::SERVICE_APIKEYS,
                    serde_json::json!({"service":service,"show_secret":show_secret}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else if result.api_keys.is_empty() {
                println!(
                    "API 代理服务 {} ({}) 未绑定 API key。",
                    result.service_id, result.service_name
                );
            } else {
                let rows = result
                    .api_keys
                    .into_iter()
                    .map(|key| {
                        vec![
                            key.id,
                            key.name,
                            key.format,
                            if key.enabled { "enabled" } else { "disabled" }.into(),
                            key.credits_limit
                                .map(format_credits)
                                .unwrap_or_else(|| "-".into()),
                            key.key.unwrap_or_else(|| "<hidden>".into()),
                        ]
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    render_table(
                        &["ID", "名称", "格式", "状态", "Credits 上限", "API Key"],
                        &rows
                    )
                );
                if !show_secret {
                    println!("使用 --show-secret 显示明文 API Key。");
                }
            }
            Ok(())
        }
    }
}

async fn show_service(client: &mut AdminClient, selector: &str, json: bool) -> Result<()> {
    let result: ProxyServiceListResult = client
        .call(method::SERVICE_LIST, serde_json::json!({}))
        .await?;
    let service = result
        .services
        .into_iter()
        .find(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| anyhow!("proxy service not found: {selector}"))?;
    if json {
        return print_json(&service);
    }
    println!("ID        {}", service.id);
    println!("名称      {}", service.name);
    println!("监听      {}:{}", service.host, service.port);
    println!(
        "配置状态  {}",
        if service.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "运行状态  {}",
        if service.running {
            "running"
        } else {
            "stopped"
        }
    );
    println!(
        "API Keys  {}",
        if service.api_key_ids.is_empty() {
            "-".into()
        } else {
            service.api_key_ids.join(",")
        }
    );
    if let Some(error) = service.error.filter(|error| !error.is_empty()) {
        println!("错误      {error}");
    }
    Ok(())
}

async fn set_service_enabled(
    client: &mut AdminClient,
    selector: &str,
    enabled: bool,
) -> Result<()> {
    mutate_config_array(client, "proxy_service", |array| {
        find_service_table_mut(array, selector)?
            .insert("enabled".into(), toml::Value::Boolean(enabled));
        Ok(())
    })
    .await
}

fn find_service_table_mut<'a>(
    array: &'a mut [toml::Value],
    selector: &str,
) -> Result<&'a mut toml::map::Map<String, toml::Value>> {
    array
        .iter_mut()
        .find(|value| {
            value.as_table().is_some_and(|table| {
                table.get("id").and_then(toml::Value::as_str) == Some(selector)
                    || table.get("name").and_then(toml::Value::as_str) == Some(selector)
            })
        })
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow!("proxy service not found: {selector}"))
}

async fn resolve_api_key_ids(
    client: &mut AdminClient,
    selectors: &[String],
) -> Result<Vec<String>> {
    if selectors.is_empty() {
        return Ok(Vec::new());
    }
    let config = effective_config(client).await?;
    let mut ids = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let key = config
            .api_key
            .iter()
            .find(|key| key.id.as_deref() == Some(selector) || key.name == *selector)
            .ok_or_else(|| anyhow!("API key not found: {selector}"))?;
        let id = key
            .id
            .clone()
            .ok_or_else(|| anyhow!("API key {selector} does not have a stable ID"))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

async fn show_keys(
    client: &mut AdminClient,
    selected: Option<&str>,
    tail: Option<usize>,
    json: bool,
) -> Result<()> {
    let mut value: serde_json::Value = client
        .call(method::APIKEY_LIST, serde_json::json!({}))
        .await?;
    if let Some(id) = selected {
        let entry = value
            .as_array_mut()
            .and_then(|array| {
                array
                    .iter()
                    .position(|item| item["id"] == id || item["name"] == id)
                    .map(|index| array.remove(index))
            })
            .ok_or_else(|| anyhow!("API key not found: {id}"))?;
        value = if let Some(tail) = tail {
            let history = entry["usage"]["history"]
                .as_array()
                .map(|items| items.iter().rev().take(tail).cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            serde_json::json!({"id":entry["id"],"name":entry["name"],"history":history})
        } else {
            entry
        };
    }
    if json {
        print_json(&value)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

async fn mutate_key_and_reload(
    client: &mut AdminClient,
    id: &str,
    field: &str,
    value: toml::Value,
) -> Result<()> {
    mutate_config_array(client, "api_key", |array| {
        let table = array
            .iter_mut()
            .find(|item| matches_key(item, id))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| anyhow!("API key not found: {id}"))?;
        table.insert(field.into(), value);
        Ok(())
    })
    .await
}

async fn clear_key_field_and_reload(client: &mut AdminClient, id: &str, field: &str) -> Result<()> {
    mutate_config_array(client, "api_key", |array| {
        let table = array
            .iter_mut()
            .find(|item| matches_key(item, id))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| anyhow!("API key not found: {id}"))?;
        table.remove(field);
        Ok(())
    })
    .await
}

async fn mutate_config_array(
    client: &mut AdminClient,
    section: &str,
    mutate: impl FnOnce(&mut Vec<toml::Value>) -> Result<()>,
) -> Result<()> {
    mutate_config(client, |table| {
        let array = table
            .entry(section)
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow!("{section} must be an array of tables"))?;
        mutate(array)
    })
    .await
}

async fn mutate_config(
    client: &mut AdminClient,
    mutate: impl FnOnce(&mut toml::map::Map<String, toml::Value>) -> Result<()>,
) -> Result<()> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = std::path::PathBuf::from(paths.config_file);
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let before = raw.parse::<toml::Value>().context("配置文件 TOML 无效")?;
    let mut root = before.clone();
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    mutate(table)?;
    let output =
        kproxy_store::config_update::render_update_preserving_comments(&raw, &before, &root)
            .context("更新配置失败")?;
    let config: kproxy_core::config::Config =
        toml::from_str(&output).context("修改后的配置无法解析")?;
    config.validate().context("修改后的配置校验失败")?;
    kproxy_store::atomic::write_bytes_atomically(&path, output.as_bytes(), Some(0o600)).await?;
    let result: ConfigReloadResult = client
        .call(method::CONFIG_RELOAD, serde_json::json!({}))
        .await?;
    if result.applied {
        return Ok(());
    }
    kproxy_store::atomic::write_bytes_atomically(&path, raw.as_bytes(), Some(0o600)).await?;
    let rollback: ConfigReloadResult = client
        .call(method::CONFIG_RELOAD, serde_json::json!({}))
        .await?;
    if !rollback.applied {
        return Err(anyhow!(
            "配置重载失败且回滚后的配置也无法重载: {}",
            rollback.error.unwrap_or_else(|| "unknown".into())
        ));
    }
    Err(anyhow!(
        "配置重载失败，磁盘文件已回滚: {}",
        result.error.unwrap_or_else(|| "unknown".into())
    ))
}

fn string_array_value(values: &[String]) -> toml::Value {
    toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect())
}

fn alert_event_array_value(events: &[AlertEvent]) -> toml::Value {
    toml::Value::Array(
        events
            .iter()
            .map(|event| toml::Value::String(event.as_str().into()))
            .collect(),
    )
}

fn integer_array_value(values: &[u32]) -> toml::Value {
    toml::Value::Array(
        values
            .iter()
            .map(|value| toml::Value::Integer(i64::from(*value)))
            .collect(),
    )
}

fn insert_optional_string(
    table: &mut toml::map::Map<String, toml::Value>,
    field: &str,
    value: Option<&str>,
) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        table.insert(field.into(), toml::Value::String(value.into()));
    }
}

fn replace_optional_string(
    table: &mut toml::map::Map<String, toml::Value>,
    field: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        table.insert(field.into(), toml::Value::String(value.into()));
    }
}

fn replace_or_clear_optional_string(
    table: &mut toml::map::Map<String, toml::Value>,
    field: &str,
    value: Option<&str>,
    clear: bool,
) {
    if clear {
        table.remove(field);
    } else {
        replace_optional_string(table, field, value);
    }
}

fn named_value_matches(value: &toml::Value, name: &str) -> bool {
    value
        .as_table()
        .and_then(|table| table.get("name"))
        .and_then(toml::Value::as_str)
        == Some(name)
}

fn find_named_table_mut<'a>(
    array: &'a mut [toml::Value],
    name: &str,
    kind: &str,
) -> Result<&'a mut toml::map::Map<String, toml::Value>> {
    array
        .iter_mut()
        .find(|value| named_value_matches(value, name))
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow!("{kind} not found: {name}"))
}

fn remove_named_value(array: &mut Vec<toml::Value>, name: &str, kind: &str) -> Result<()> {
    let before = array.len();
    array.retain(|value| !named_value_matches(value, name));
    if array.len() == before {
        return Err(anyhow!("{kind} not found: {name}"));
    }
    Ok(())
}

fn matches_key(value: &toml::Value, id: &str) -> bool {
    value.as_table().is_some_and(|table| {
        table.get("id").and_then(toml::Value::as_str) == Some(id)
            || table.get("name").and_then(toml::Value::as_str) == Some(id)
    })
}

fn generate_key(format: &str) -> Result<String> {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    use std::fmt::Write as _;
    let random = bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    });
    match format {
        "sk" => Ok(format!("sk-{random}")),
        "token" => Ok(format!("token_{random}")),
        "simple" => Ok(random),
        other => Err(anyhow!("unsupported format: {other}")),
    }
}

fn key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .fold(String::from("ak_"), |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        })
}

#[derive(Debug, Clone, Copy)]
struct ConfigModule {
    name: &'static str,
    key: &'static str,
    category: &'static str,
    description: &'static str,
    preferred_command: &'static str,
    aliases: &'static [&'static str],
    is_array: bool,
}

impl ConfigModule {
    fn resettable(&self) -> bool {
        !matches!(self.key, "api_key" | "proxy_service")
    }
}

const CONFIG_MODULES: &[ConfigModule] = &[
    ConfigModule {
        name: "server",
        key: "server",
        category: "通用",
        description: "API 服务默认监听、准入、连接与 TLS",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "upstream",
        key: "upstream",
        category: "通用",
        description: "Kiro 上游请求、重试、超时与连接池",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "pool",
        key: "pool",
        category: "通用",
        description: "账号池并发、排队、额度保护与选号",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "features",
        key: "features",
        category: "通用",
        description: "协议转换、工具、缓存与 thinking 开关",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "models",
        key: "models",
        category: "通用",
        description: "动态模型发现与缓存",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "tasks",
        key: "tasks",
        category: "通用",
        description: "后台周期任务间隔",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "context",
        key: "context",
        category: "通用",
        description: "上下文限制、压缩与工具保护",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "storage",
        key: "storage",
        category: "通用",
        description: "账号与状态持久化参数",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "notify",
        key: "notify",
        category: "告警",
        description: "告警阈值、抑制与投递策略",
        preferred_command: "kproxy alert config",
        aliases: &["alert", "alerts"],
        is_array: false,
    },
    ConfigModule {
        name: "log",
        key: "log",
        category: "通用",
        description: "日志级别、格式、路径与保留策略",
        preferred_command: "-",
        aliases: &["logging"],
        is_array: false,
    },
    ConfigModule {
        name: "admin",
        key: "admin",
        category: "通用",
        description: "本地管理面 socket",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "sso",
        key: "sso",
        category: "通用",
        description: "企业 SSO 默认入口与区域",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "model-mapping",
        key: "model_mapping",
        category: "规则",
        description: "客户端模型到 Kiro 模型的映射规则",
        preferred_command: "kproxy model-map",
        aliases: &["model-map"],
        is_array: true,
    },
    ConfigModule {
        name: "model-thinking-mode",
        key: "model_thinking_mode",
        category: "规则",
        description: "模型级 thinking 默认开关",
        preferred_command: "-",
        aliases: &["thinking"],
        is_array: false,
    },
    ConfigModule {
        name: "webhook",
        key: "webhook",
        category: "告警",
        description: "告警投递目标",
        preferred_command: "kproxy alert",
        aliases: &["webhooks"],
        is_array: true,
    },
    ConfigModule {
        name: "api-key",
        key: "api_key",
        category: "基础服务",
        description: "客户端访问凭据与额度限制",
        preferred_command: "kproxy apikey",
        aliases: &["apikey"],
        is_array: true,
    },
    ConfigModule {
        name: "proxy-service",
        key: "proxy_service",
        category: "基础服务",
        description: "代理监听实例及 API key 绑定",
        preferred_command: "kproxy service",
        aliases: &["service"],
        is_array: true,
    },
];

pub fn list_config_modules(json: bool) -> Result<()> {
    if json {
        let modules = CONFIG_MODULES
            .iter()
            .map(|module| {
                serde_json::json!({
                    "name":module.name,
                    "toml_key":module.key,
                    "category":module.category,
                    "description":module.description,
                    "preferred_command":(module.preferred_command != "-")
                        .then_some(module.preferred_command),
                    "aliases":module.aliases,
                    "resettable":module.resettable(),
                })
            })
            .collect::<Vec<_>>();
        return print_json(&modules);
    }

    let rows = CONFIG_MODULES
        .iter()
        .map(|module| {
            vec![
                module.name.to_string(),
                module.category.to_string(),
                module.description.to_string(),
                if module.resettable() { "是" } else { "否" }.to_string(),
                module.preferred_command.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print!(
        "{}",
        render_table(&["模块", "类型", "说明", "可重置", "推荐管理命令"], &rows)
    );
    Ok(())
}

pub async fn show_config(
    client: &mut AdminClient,
    module: Option<&str>,
    effective: bool,
    json: bool,
) -> Result<()> {
    let module = module.map(resolve_config_module).transpose()?;
    let show: ConfigShowResult = client
        .call(method::CONFIG_SHOW, serde_json::json!({}))
        .await?;
    let Some(module) = module else {
        if json {
            return print_json(&show);
        }
        if effective {
            println!("{}", serde_json::to_string_pretty(&show.effective_json)?);
        } else {
            print!("{}", show.raw);
        }
        return Ok(());
    };

    if effective {
        let value = show
            .effective_json
            .get(module.key)
            .cloned()
            .ok_or_else(|| anyhow!("生效配置缺少模块 {}", module.name))?;
        if json {
            print_json(&serde_json::json!({
                "path":show.path,
                "module":module.name,
                "toml_key":module.key,
                "effective":value,
            }))?;
        } else {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        return Ok(());
    }

    let raw = render_config_module_document(&show.raw, module)?;
    if json {
        print_json(&serde_json::json!({
            "path":show.path,
            "module":module.name,
            "toml_key":module.key,
            "raw":raw,
        }))?;
    } else {
        print!("{raw}");
    }
    Ok(())
}

fn resolve_config_module(name: &str) -> Result<&'static ConfigModule> {
    let normalized = name.trim().to_ascii_lowercase().replace('-', "_");
    CONFIG_MODULES
        .iter()
        .find(|module| {
            module.key == normalized
                || module.name.replace('-', "_") == normalized
                || module
                    .aliases
                    .iter()
                    .any(|alias| alias.replace('-', "_") == normalized)
        })
        .ok_or_else(|| anyhow!("未知配置模块 {name}；使用 `kproxy config list` 查看可用模块"))
}

fn render_config_module_document(raw: &str, module: &ConfigModule) -> Result<String> {
    let source = raw.parse::<DocumentMut>().context("配置文件 TOML 无效")?;
    let skeleton = if module.is_array {
        format!("{} = []\n", module.key)
    } else {
        format!("[{}]\n", module.key)
    };
    let mut output = skeleton
        .parse::<DocumentMut>()
        .context("无法生成配置模块模板")?;
    if let Some(item) = source.as_table().get(module.key) {
        output.as_table_mut().insert(module.key, item.clone());
    }
    Ok(output.to_string())
}

fn render_config_module_editor(raw: &str, module: &ConfigModule) -> Result<String> {
    let body = render_config_module_document(raw, module)?;
    Ok(format!(
        "# KProxy 配置模块：{}（TOML key: {}）\n\
         # 只允许编辑本模块；保存后会合并回完整配置并进行整体校验。\n\
         # 删除整个模块不会保存；要恢复默认值，请保留模块 TOML key 并清空字段或数组。\n\n\
         {body}",
        module.name, module.key
    ))
}

fn merge_edited_config_module(
    original: &str,
    module: &ConfigModule,
    edited: &str,
) -> Result<String> {
    let before = original
        .parse::<toml::Value>()
        .context("原配置文件 TOML 无效")?;
    let edited = edited
        .parse::<toml::Value>()
        .with_context(|| format!("配置模块 {} 的 TOML 无效", module.name))?;
    let edited_table = edited
        .as_table()
        .ok_or_else(|| anyhow!("配置模块文件必须是 TOML table"))?;
    let extra = edited_table
        .keys()
        .filter(|key| key.as_str() != module.key)
        .cloned()
        .collect::<Vec<_>>();
    if !extra.is_empty() {
        return Err(anyhow!(
            "配置模块 {} 不能包含其他顶层模块：{}",
            module.name,
            extra.join(", ")
        ));
    }
    let value = edited_table.get(module.key).cloned().ok_or_else(|| {
        anyhow!(
            "配置模块 {} 缺少 TOML key `{}`；请保留该 key",
            module.name,
            module.key
        )
    })?;
    let mut after = before.clone();
    after
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?
        .insert(module.key.into(), value);
    let output =
        kproxy_store::config_update::render_update_preserving_comments(original, &before, &after)
            .context("合并配置模块失败")?;
    let config: kproxy_core::config::Config =
        toml::from_str(&output).context("合并后的配置无法解析")?;
    config.validate().context("合并后的配置校验失败")?;
    Ok(output)
}

/// Reloads the configuration while excluding concurrent `kproxy` and daemon
/// mutations of the same file.
pub async fn reload_config(client: &mut AdminClient) -> Result<ConfigReloadResult> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = PathBuf::from(paths.config_file);
    let _config_lock = kproxy_store::atomic::lock_file_exclusive(&path)
        .await
        .with_context(|| format!("锁定配置文件 {} 失败", path.display()))?;
    reload_config_while_locked(client).await
}

async fn reload_config_while_locked(client: &mut AdminClient) -> Result<ConfigReloadResult> {
    client
        .call(method::CONFIG_RELOAD, serde_json::json!({}))
        .await
}

pub async fn validate_config(file: Option<&str>) -> Result<()> {
    let path = file
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Paths::from_env().config_file);
    let config = kproxy_store::config_loader::load_config(&path)
        .await
        .with_context(|| format!("读取或解析 {} 失败", path.display()))?;
    config
        .validate()
        .with_context(|| format!("{} 配置校验失败", path.display()))?;
    println!("配置有效：{}", path.display());
    Ok(())
}

pub async fn edit_config(client: &mut AdminClient, module: Option<&str>) -> Result<()> {
    if let Some(module) = module {
        return edit_config_module(client, resolve_config_module(module)?).await;
    }
    edit_full_config(client).await
}

async fn edit_full_config(client: &mut AdminClient) -> Result<()> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = std::path::PathBuf::from(paths.config_file);
    let original = tokio::fs::read(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    run_editor(&path).await?;
    if let Err(error) = validate_config(Some(path.to_string_lossy().as_ref())).await {
        kproxy_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
        return Err(error.context("配置无效，磁盘文件已回滚"));
    }
    let result = reload_edited_config(client, &path, &original).await?;
    println!("配置已保存并重载");
    print_restart_required(&result.needs_restart);
    Ok(())
}

async fn edit_config_module(client: &mut AdminClient, module: &ConfigModule) -> Result<()> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = PathBuf::from(paths.config_file);
    let _config_lock = kproxy_store::atomic::lock_file_exclusive(&path)
        .await
        .with_context(|| format!("锁定配置文件 {} 失败", path.display()))?;
    let original = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let editor_content = render_config_module_editor(&original, module)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!("kproxy-config-{}-", module.name))
        .suffix(".toml")
        .tempfile()
        .context("创建配置模块临时文件失败")?;
    temporary
        .write_all(editor_content.as_bytes())
        .context("写入配置模块临时文件失败")?;
    temporary.flush().context("刷新配置模块临时文件失败")?;
    run_editor(temporary.path()).await?;
    let edited = tokio::fs::read_to_string(temporary.path())
        .await
        .context("读取编辑后的配置模块失败")?;
    let output = merge_edited_config_module(&original, module, &edited)?;
    if output == original {
        println!("配置模块 {} 未修改", module.name);
        return Ok(());
    }

    kproxy_store::atomic::write_bytes_atomically(&path, output.as_bytes(), Some(0o600)).await?;
    let result = reload_edited_config(client, &path, original.as_bytes()).await?;
    println!("配置模块 {} 已保存并重载", module.name);
    print_restart_required(&result.needs_restart);
    Ok(())
}

async fn run_editor(path: &Path) -> Result<()> {
    let editor = resolve_editor()?;
    let mut command = tokio::process::Command::new(&editor.program);
    command.args(&editor.args).arg(path);
    ensure_utf8_editor_locale(&mut command);
    let status = command.status().await.with_context(|| {
        format!(
            "启动编辑器 {} 失败；请确认命令已安装，或通过 $VISUAL/$EDITOR 指定编辑器",
            editor.program.display()
        )
    })?;
    if !status.success() {
        return Err(anyhow!("编辑器退出状态为 {status}"));
    }
    Ok(())
}

async fn reload_edited_config(
    client: &mut AdminClient,
    path: &Path,
    original: &[u8],
) -> Result<ConfigReloadResult> {
    let reload = reload_config_while_locked(client).await;
    if matches!(&reload, Ok(result) if result.applied) {
        return reload;
    }

    kproxy_store::atomic::write_bytes_atomically(path, original, Some(0o600)).await?;
    let rollback = reload_config_while_locked(client).await;
    match (reload, rollback) {
        (Ok(result), Ok(rollback)) if rollback.applied => Err(anyhow!(
            "配置重载失败，磁盘文件已回滚：{}",
            result.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Err(error), Ok(rollback)) if rollback.applied => {
            Err(error.context("配置重载请求失败，磁盘文件已回滚"))
        }
        (Ok(result), Ok(rollback)) => Err(anyhow!(
            "配置重载失败，且回滚配置也无法重载：{}；回滚错误：{}",
            result.error.unwrap_or_else(|| "未知错误".into()),
            rollback.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Err(error), Ok(rollback)) => Err(anyhow!(
            "配置重载请求失败，且回滚配置也无法重载：{error}；回滚错误：{}",
            rollback.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Ok(result), Err(rollback_error)) => Err(anyhow!(
            "配置重载失败，且无法确认回滚配置已生效：{}；回滚错误：{rollback_error}",
            result.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Err(error), Err(rollback_error)) => Err(anyhow!(
            "配置重载请求失败，且无法确认回滚配置已生效：{error}；回滚错误：{rollback_error}"
        )),
    }
}

fn print_restart_required(fields: &[String]) {
    for field in fields {
        println!("注意：{field} 需重启 kproxyd 才能生效");
    }
}

/// Successful `config reset` result.
pub struct ConfigResetResult {
    pub config_file: PathBuf,
    pub backup_file: PathBuf,
    pub module: Option<String>,
    pub needs_restart: Vec<String>,
}

/// Back up the current configuration, reset one module or all general settings, and reload it.
pub async fn reset_config(
    client: &mut AdminClient,
    module: Option<&str>,
) -> Result<Option<ConfigResetResult>> {
    let module = module.map(resolve_config_module).transpose()?;
    if let Some(module) = module {
        if !module.resettable() {
            return Err(anyhow!(
                "配置模块 {} 属于基础服务资源，不能通过 config reset 清空；请使用 `{}` 显式管理，或执行完整 `kproxy uninstall`",
                module.name,
                module.preferred_command
            ));
        }
    }
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = PathBuf::from(paths.config_file);
    let prompt = module.map_or_else(
        || {
            "确认将通用配置恢复为默认设置？API key 和代理服务会保留，模型映射和告警目标会被清除"
                .to_string()
        },
        |module| {
            format!(
                "确认将配置模块 {} 恢复为默认设置？其他配置不会改动",
                module.name
            )
        },
    );
    if !crate::commands::confirm(&prompt).await? {
        return Ok(None);
    }

    let original = tokio::fs::read(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let raw = std::str::from_utf8(&original).context("当前配置不是有效的 UTF-8")?;
    let reset = match module {
        Some(module) => render_config_module_reset(raw, module)?,
        None => render_reset_config_preserving_services(raw)?,
    };

    let backup_file = write_config_backup(&path, &original).await?;
    kproxy_store::atomic::write_bytes_atomically(&path, reset.as_bytes(), Some(0o600)).await?;
    let result: ConfigReloadResult = match reload_config_while_locked(client).await {
        Ok(result) => result,
        Err(reload_error) => {
            kproxy_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
            let rollback = reload_config_while_locked(client).await;
            return match rollback {
                Ok(rollback) if rollback.applied => Err(reload_error.context(format!(
                    "默认配置重载请求失败，磁盘文件已回滚；原配置备份位于 {}",
                    backup_file.display()
                ))),
                Ok(rollback) => Err(anyhow!(
                    "默认配置重载请求失败，且回滚后的配置也无法重载；原配置备份位于 {}：{}；回滚错误：{}",
                    backup_file.display(),
                    reload_error,
                    rollback.error.unwrap_or_else(|| "未知错误".into())
                )),
                Err(rollback_error) => Err(anyhow!(
                    "默认配置重载请求失败，且无法确认回滚配置已生效；原配置备份位于 {}：{}；回滚错误：{}",
                    backup_file.display(),
                    reload_error,
                    rollback_error
                )),
            };
        }
    };
    if result.applied {
        return Ok(Some(ConfigResetResult {
            config_file: path,
            backup_file,
            module: module.map(|module| module.name.to_string()),
            needs_restart: result.needs_restart,
        }));
    }

    kproxy_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
    let rollback = reload_config_while_locked(client).await?;
    if !rollback.applied {
        return Err(anyhow!(
            "默认配置重载失败，且回滚后的配置也无法重载；原配置备份位于 {}：{}",
            backup_file.display(),
            rollback.error.unwrap_or_else(|| "未知错误".into())
        ));
    }
    Err(anyhow!(
        "默认配置重载失败，磁盘文件已回滚；原配置备份位于 {}：{}",
        backup_file.display(),
        result.error.unwrap_or_else(|| "未知错误".into())
    ))
}

fn render_config_module_reset(raw: &str, module: &ConfigModule) -> Result<String> {
    if !module.resettable() {
        return Err(anyhow!(
            "配置模块 {} 属于基础服务资源，不能通过 config reset 清空",
            module.name
        ));
    }
    let before = raw.parse::<toml::Value>().context("当前配置 TOML 无效")?;
    let defaults = toml::Value::try_from(kproxy_core::config::Config::default())
        .context("内置默认配置无法序列化")?;
    let default_value = defaults
        .as_table()
        .and_then(|table| table.get(module.key))
        .cloned()
        .ok_or_else(|| anyhow!("内置默认配置缺少模块 {}", module.name))?;
    let mut after = before.clone();
    after
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?
        .insert(module.key.into(), default_value);
    let output =
        kproxy_store::config_update::render_update_preserving_comments(raw, &before, &after)
            .context("生成模块重置配置失败")?;
    let config: kproxy_core::config::Config =
        toml::from_str(&output).context("模块重置后的配置无法解析")?;
    config.validate().context("模块重置后的配置校验失败")?;
    Ok(output)
}

/// Renders defaults while retaining the two resource sections managed by
/// `kproxy apikey` and `kproxy service`.
fn render_reset_config_preserving_services(raw: &str) -> Result<String> {
    const PRESERVED_SECTIONS: [&str; 2] = ["api_key", "proxy_service"];

    let current = raw.parse::<toml::Value>().context("当前配置 TOML 无效")?;
    let current_table = current
        .as_table()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    let defaults = kproxy_store::bootstrap::render_default_config(
        &kproxy_core::config::Config::default().admin.socket,
    );
    let before = defaults
        .parse::<toml::Value>()
        .context("内置默认配置无法解析")?;
    let mut after = before.clone();
    let after_table = after
        .as_table_mut()
        .ok_or_else(|| anyhow!("default config root must be a TOML table"))?;
    for section in PRESERVED_SECTIONS {
        if let Some(value) = current_table.get(section) {
            after_table.insert(section.into(), value.clone());
        }
    }

    let output =
        kproxy_store::config_update::render_update_preserving_comments(&defaults, &before, &after)
            .context("生成重置配置失败")?;
    let config: kproxy_core::config::Config =
        toml::from_str(&output).context("重置后的配置无法解析")?;
    config.validate().context("重置后的配置校验失败")?;
    Ok(output)
}

async fn write_config_backup(path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("配置路径缺少文件名：{}", path.display()))?
        .to_string_lossy();
    for index in 0..10_000usize {
        let suffix = if index == 0 {
            ".bak".to_owned()
        } else {
            format!(".bak.{index}")
        };
        let candidate = path.with_file_name(format!("{file_name}{suffix}"));
        if kproxy_store::atomic::write_bytes_if_absent_atomically(&candidate, contents, Some(0o600))
            .await?
        {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "无法为 {} 分配备份文件名：已有备份数量过多",
        path.display()
    ))
}

const DEFAULT_EDITORS: [&str; 3] = ["vim", "vi", "nano"];
const UTF8_EDITOR_LOCALE: &str = "C.UTF-8";

#[derive(Debug, PartialEq, Eq)]
struct EditorCommand {
    program: PathBuf,
    args: Vec<String>,
}

fn resolve_editor() -> Result<EditorCommand> {
    if let Some(configured) = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
    {
        return parse_editor(&configured);
    }

    let path = std::env::var_os("PATH");
    let program = find_default_editor(path.as_deref()).ok_or_else(|| {
        anyhow!(
            "未找到可用编辑器（已尝试 {}）；请安装编辑器，或设置 $VISUAL/$EDITOR，例如 EDITOR=vim kproxy config edit",
            DEFAULT_EDITORS.join("、")
        )
    })?;
    Ok(EditorCommand {
        program,
        args: Vec::new(),
    })
}

fn parse_editor(configured: &str) -> Result<EditorCommand> {
    let mut parts = shlex::split(configured)
        .ok_or_else(|| anyhow!("$VISUAL/$EDITOR 存在未闭合的引号"))?
        .into_iter();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("$VISUAL/$EDITOR 不能为空"))?;
    Ok(EditorCommand {
        program: PathBuf::from(program),
        args: parts.collect(),
    })
}

fn ensure_utf8_editor_locale(command: &mut tokio::process::Command) {
    if editor_needs_utf8_locale(
        std::env::var_os("LC_ALL").as_deref(),
        std::env::var_os("LC_CTYPE").as_deref(),
        std::env::var_os("LANG").as_deref(),
    ) {
        command
            .env("LANG", UTF8_EDITOR_LOCALE)
            .env("LC_ALL", UTF8_EDITOR_LOCALE);
    }
}

fn editor_needs_utf8_locale(
    lc_all: Option<&std::ffi::OsStr>,
    lc_ctype: Option<&std::ffi::OsStr>,
    lang: Option<&std::ffi::OsStr>,
) -> bool {
    let effective = [lc_all, lc_ctype, lang]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty());
    effective.is_none_or(|value| {
        !value
            .to_string_lossy()
            .bytes()
            .filter(u8::is_ascii_alphanumeric)
            .map(|byte| byte.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .windows(4)
            .any(|window| window == b"utf8")
    })
}

fn find_default_editor(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path?;
    DEFAULT_EDITORS.iter().find_map(|editor| {
        std::env::split_paths(path)
            .map(|directory| directory.join(editor))
            .find(|candidate| is_executable(candidate))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub async fn show_models(client: &mut AdminClient, mapped: bool, json: bool) -> Result<()> {
    let models: serde_json::Value = client.call(method::MODELS, serde_json::json!({})).await?;
    if !mapped {
        if json {
            return print_json(&models);
        }
        for model in models.as_array().into_iter().flatten() {
            println!(
                "{:<34} {}",
                model["modelId"].as_str().unwrap_or("-"),
                model["modelName"].as_str().unwrap_or("")
            );
        }
        return Ok(());
    }
    let config = effective_config(client).await?;
    let routes = models
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["modelId"].as_str())
        .map(|model| {
            let route = kproxy_translate::model::map_model(
                model,
                &config.model_mapping,
                None,
                None,
                &config.features.default_model_id,
            );
            serde_json::json!({"input":model,"mapped":route.mapped,"rule":route.rule})
        })
        .collect::<Vec<_>>();
    if json {
        print_json(&routes)
    } else {
        for route in routes {
            println!(
                "{:<34} -> {:<34} {}",
                route["input"].as_str().unwrap_or("-"),
                route["mapped"].as_str().unwrap_or("-"),
                route["rule"].as_str().unwrap_or("(无规则)")
            );
        }
        Ok(())
    }
}

pub async fn show_model_resolution(
    client: &mut AdminClient,
    model: &str,
    api_key: Option<&str>,
    json: bool,
) -> Result<()> {
    let result: ModelResolutionResult = client
        .call(
            method::MODEL_RESOLVE,
            serde_json::json!({"model":model,"api_key":api_key}),
        )
        .await?;
    if json {
        return print_json(&result);
    }

    println!("输入模型  {}", result.input_model);
    println!("显式映射  {}", result.mapped_model);
    println!(
        "映射规则  {}",
        result.mapping_rule.as_deref().unwrap_or("(无规则)")
    );
    if let Some(resolved) = &result.resolved_model {
        println!("最终模型  {resolved}");
    } else if result.possible_models.is_empty() {
        println!("最终模型  (无法解析)");
    } else {
        println!("候选模型  {}", result.possible_models.join(", "));
    }
    println!(
        "匹配账号  {}/{}",
        result.matched_accounts, result.total_accounts
    );
    let rows = result
        .accounts
        .iter()
        .map(|account| {
            vec![
                format!("{} ({})", account.account_name, account.account_id),
                account.health.clone(),
                account.mapped_model.clone(),
                account
                    .resolved_model
                    .clone()
                    .or_else(|| account.error.clone())
                    .unwrap_or_else(|| "-".into()),
                account.mapping_rule.clone().unwrap_or_else(|| "-".into()),
                if account.used_default {
                    format!("{}+default", account.model_source)
                } else {
                    account.model_source.clone()
                },
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "\n{}",
        render_table(
            &[
                "账号",
                "状态",
                "映射模型",
                "最终模型 / 原因",
                "规则",
                "来源",
            ],
            &rows
        )
    );
    Ok(())
}

pub async fn run_model_map(
    client: &mut AdminClient,
    command: ModelMapCommand,
    json: bool,
) -> Result<()> {
    match command {
        ModelMapCommand::List => {
            let config = effective_config(client).await?;
            let mut rules = config.model_mapping;
            rules.sort_by_key(|rule| rule.priority);
            if json {
                print_json(&rules)
            } else {
                for rule in rules {
                    let credits = rule
                        .max_remaining_credit_percent
                        .map(|value| format!("剩余<{value}%"))
                        .unwrap_or_else(|| "无额度条件".into());
                    let schedule = rule
                        .schedule
                        .as_ref()
                        .map(|schedule| schedule.mode.as_str())
                        .unwrap_or("全天");
                    println!(
                        "[{:>3}] {:<24} {:<11} {} -> {}  {}  {}{}",
                        rule.priority,
                        rule.name,
                        rule.kind,
                        rule.source_models.join(","),
                        rule.target_models.join(","),
                        credits,
                        schedule,
                        if rule.enabled { "" } else { " [disabled]" }
                    );
                }
                Ok(())
            }
        }
        ModelMapCommand::Add {
            name,
            kind,
            source_models,
            target_models,
            priority,
            weights,
            below_credits_percent,
            api_key_ids,
            disabled,
        } => {
            mutate_config_array(client, "model_mapping", |array| {
                if array.iter().any(|value| named_value_matches(value, &name)) {
                    return Err(anyhow!("model mapping already exists: {name}"));
                }
                let mut table = toml::map::Map::new();
                table.insert("name".into(), toml::Value::String(name.clone()));
                table.insert("enabled".into(), toml::Value::Boolean(!disabled));
                table.insert("type".into(), toml::Value::String(kind.clone()));
                table.insert("source_models".into(), string_array_value(&source_models));
                table.insert("target_models".into(), string_array_value(&target_models));
                table.insert("priority".into(), toml::Value::Integer(i64::from(priority)));
                if !weights.is_empty() {
                    table.insert("weights".into(), integer_array_value(&weights));
                }
                if let Some(percent) = below_credits_percent {
                    table.insert(
                        "max_remaining_credit_percent".into(),
                        toml::Value::Float(percent),
                    );
                }
                if !api_key_ids.is_empty() {
                    table.insert("api_key_ids".into(), string_array_value(&api_key_ids));
                }
                // Missing schedule means always active. A credits threshold
                // naturally stops matching after the upstream monthly quota
                // refresh raises the remaining percentage again.
                array.push(toml::Value::Table(table));
                Ok(())
            })
            .await?;
            println!("已添加模型映射规则 {name}");
            Ok(())
        }
        ModelMapCommand::Edit {
            name,
            rename,
            kind,
            source_models,
            target_models,
            priority,
            weights,
            clear_weights,
            below_credits_percent,
            clear_credits_threshold,
            api_key_ids,
            clear_api_keys,
            enable,
            disable,
        } => {
            mutate_config_array(client, "model_mapping", |array| {
                let table = find_named_table_mut(array, &name, "model mapping")?;
                replace_optional_string(table, "name", rename.as_deref());
                replace_optional_string(table, "type", kind.as_deref());
                if !source_models.is_empty() {
                    table.insert("source_models".into(), string_array_value(&source_models));
                }
                if !target_models.is_empty() {
                    table.insert("target_models".into(), string_array_value(&target_models));
                }
                if let Some(priority) = priority {
                    table.insert("priority".into(), toml::Value::Integer(i64::from(priority)));
                }
                if clear_weights {
                    table.remove("weights");
                } else if !weights.is_empty() {
                    table.insert("weights".into(), integer_array_value(&weights));
                }
                if clear_credits_threshold {
                    table.remove("max_remaining_credit_percent");
                } else if let Some(percent) = below_credits_percent {
                    table.insert(
                        "max_remaining_credit_percent".into(),
                        toml::Value::Float(percent),
                    );
                }
                if clear_api_keys {
                    table.remove("api_key_ids");
                } else if !api_key_ids.is_empty() {
                    table.insert("api_key_ids".into(), string_array_value(&api_key_ids));
                }
                if enable || disable {
                    table.insert("enabled".into(), toml::Value::Boolean(enable));
                }
                Ok(())
            })
            .await?;
            println!("已更新模型映射规则 {name}");
            Ok(())
        }
        ModelMapCommand::Delete { name } => {
            if !crate::commands::confirm(&format!("确认删除模型映射规则 {name}？")).await?
            {
                println!("已取消");
                return Ok(());
            }
            mutate_config_array(client, "model_mapping", |array| {
                remove_named_value(array, &name, "model mapping")
            })
            .await?;
            println!("已删除模型映射规则 {name}");
            Ok(())
        }
        ModelMapCommand::Test {
            model,
            remaining_credits_percent,
            api_key,
        } => {
            let config = effective_config(client).await?;
            let route = kproxy_translate::model::map_model(
                &model,
                &config.model_mapping,
                api_key.as_deref(),
                remaining_credits_percent,
                &config.features.default_model_id,
            );
            if json {
                print_json(&serde_json::json!({
                    "input":route.original,"matched_rule":route.rule,"result":route.mapped
                }))
            } else {
                println!("输入      {}", route.original);
                println!("命中      {}", route.rule.as_deref().unwrap_or("(无规则)"));
                println!("结果      {}", route.mapped);
                Ok(())
            }
        }
    }
}

async fn effective_config(client: &mut AdminClient) -> Result<kproxy_core::config::Config> {
    let show: ConfigShowResult = client
        .call(method::CONFIG_SHOW, serde_json::json!({}))
        .await?;
    serde_json::from_value(show.effective_json).context("daemon 返回的生效配置无效")
}

const HELP_TOPICS: &[&str] = &[
    "status",
    "health",
    "version",
    "account",
    "sso",
    "service",
    "apikey",
    "pool",
    "balance",
    "diagnose",
    "subscriptions",
    "tasks",
    "stats",
    "logs",
    "alert",
    "models",
    "model-map",
    "config",
    "docker",
];

pub fn print_topic(topic: Option<&str>) -> Result<()> {
    let Some(topic) = topic else {
        println!("可用帮助主题：\n  {}", HELP_TOPICS.join("\n  "));
        println!("\n使用 `kproxy help <topic>` 查看详情，子命令可用 `--help`。");
        return Ok(());
    };
    let text = match topic {
        "status" => {
            "`kproxy status` 展示 daemon 版本、代理服务、账号池和本次启动后的请求统计；支持 `--since` 或 `--start/--end`，`--watch` 每 2 秒刷新。"
        }
        "health" => {
            "`kproxy health` 检查 daemon 管理面是否可用，供容器和 systemd 健康检查使用；账号或代理服务为空不会让该检查失败。"
        }
        "version" => {
            "`kproxy version` 显示 CLI 版本、Rust MSRV 和默认 Kiro 上游端点。"
        }
        "account" => {
            "账号命令包括 list/show/import/export/add-sso/rm/enable/disable/tag/refresh/probe。删除操作会要求输入 y/yes 二次确认。"
        }
        "balance" => {
            "账号池评分 = active_ratio×weight_active + used_credit_ratio×weight_credit + recent_idle_penalty×weight_idle。\n分数越低越优先；随后加入小幅随机抖动，避免并发请求集中到同一账号。\n用 `kproxy pool --watch --explain` 查看实时评分明细。"
        }
        "pool" => {
            "`kproxy pool --model <model>` 查看当前账号池可用性；`--explain` 展示调度评分，`--watch` 持续刷新。评分原理见 `kproxy help balance`。"
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
            "配置默认位于 $KPROXY_HOME/config.toml，修改后热重载；server.host/port、admin.socket 和 TLS 监听变更需要重启。\n`kproxy config list` 列出全部顶层模块及是否允许重置；`show [模块]` 可查看完整配置或单个模块，增加 `--effective` 查看合并默认值后的结果；`edit [模块]` 可编辑完整配置或单个模块，保存时会合并、整体校验并重载。`reset [模块]` 只恢复指定模块，其他配置不变；不指定模块时恢复全部通用配置。API key 和代理服务属于基础服务资源，不会被 config reset 清除。`validate [file]` 只校验，不应用。"
        }
        "apikey" => {
            "API key 限额采用在途预留：请求进入时预留估算 credits，结束后按上游实际用量结算，避免并发突破限额。\n`kproxy apikey show <ID|名称>` 查看单项，`list --detail` 查看 token/credits 消耗；`limit <ID|名称> --clear` 可恢复不限，`rm`/`delete` 均可删除。日维度、模型、路径和历史可用 `usage` 与 `history` 查询。"
        }
        "diagnose" => {
            "`kproxy diagnose endpoints` 检查 CodeWhisperer/AmazonQ/OIDC 端点；`kproxy diagnose account <id>` 或 `--all` 拉取模型并发起真实推理。"
        }
        "subscriptions" => {
            "`kproxy subscriptions [account]` 查询上游当前可用的企业订阅计划；省略账号时使用可调度账号。"
        }
        "tasks" => {
            "`kproxy tasks` 查看 token 刷新、状态探测、统计持久化、模型缓存等周期任务；`kproxy tasks run <name>` 立即执行。"
        }
        "stats" => {
            "`kproxy stats` 默认显示跨 daemon 重启的持久化累计统计；可用 `--since 1h` 或带时区的 `--start/--end` 查询时间段。`--detail` 显示最近请求，并可用 `--by model|account|endpoint` 分组。"
        }
        "logs" => {
            "`kproxy logs show` 查看内存中的结构化请求日志，`follow` 持续跟踪；支持 `--level`、`--account` 和 `--tail`。`kproxy logs trace <TRACE_ID>` 默认跨日期和全部精确级别分片查询完整链路，可用 `--level error` 限定级别。info 文件只包含 INFO，WARN/ERROR 分别写入 warn/error 文件。`kproxy logs files [--level error]` 列出实际日志文件，`logs path` 显示目录、基础路径和当前日志配置。旧的 `kproxy logs --tail ...` 与 `kproxy logs -f` 继续兼容。"
        }
        "alert" => {
            "`kproxy alert events` 列出四类异常事件和触发条件，`kproxy alert platforms` 说明 --platform 支持的通知平台和平台专用参数；`kproxy alert config` 查看一次性告警策略。同类型的多账号事件会聚合为一条 Markdown 告警；每个账号或服务恢复后才允许再次告警。`kproxy alert add/edit/delete/list/test/logs` 管理告警目标。"
        }
        "models" => {
            "`kproxy models` 显示账号自动探测到的 Kiro 模型；`--refresh` 先立即刷新缓存，`--mapped` 同时显示显式映射结果。`kproxy models resolve <MODEL_ID>` 使用当前配置、账号额度和账号模型缓存，显示显式映射与最终 Kiro 模型；可配合 `--api-key` 和 `--refresh`。"
        }
        "docker" => {
            "默认 `docker compose up -d --build` 构建 runtime-full，启用全部 feature 并包含 Chromium SSO 运行时。数据保存在 kproxy-data 命名卷。"
        }
        _ => {
            return Err(anyhow!(
                "未知帮助主题 {topic}；可用主题：{}",
                HELP_TOPICS.join(", ")
            ))
        }
    };
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests;
