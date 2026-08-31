//! Pool, diagnostics, statistics, API-key and alert commands.

use anyhow::{anyhow, Context, Result};
use clap::{Subcommand, ValueEnum};
use kproxy_core::paths::Paths;
use kproxy_ipc::protocol::method;
use kproxy_ipc::protocol::{
    ConfigPathResult, ConfigReloadResult, ConfigShowResult, LogFilesResult, ModelResolutionResult,
    ProxyServiceApiKeysResult, ProxyServiceCreateResult, ProxyServiceDeleteResult,
    ProxyServiceListResult,
};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

pub async fn show_stats(
    client: &mut AdminClient,
    detail: bool,
    recent: Option<usize>,
    range: (Option<u64>, Option<i64>, Option<i64>),
    by: Option<&str>,
    json: bool,
) -> Result<()> {
    let (since_secs, start_secs, end_secs) = range;
    let effective_recent = detail.then_some(recent.unwrap_or(20));
    let value: serde_json::Value = client
        .call(
            method::STATS,
            serde_json::json!({
                "detail":detail,
                "recent":effective_recent,
                "since_secs":since_secs,
                "start_secs":start_secs,
                "end_secs":end_secs,
                "by":by,
            }),
        )
        .await?;
    if json {
        return print_json(&value);
    }
    if value.get("by_apikey").is_some() {
        return print_stats_by_apikey(&value["by_apikey"]);
    }

    print_stats_range(&value);

    let summary = if detail {
        &value["stats"]["total"]
    } else {
        &value["summary"]
    };
    print_stats_summary(summary, &value["latency"]);
    if !detail {
        println!("使用 --detail 查看分组统计和最近请求。");
        return Ok(());
    }
    if let Some(dimension) = by {
        print_stats_group(dimension, &value["grouped"])?;
    }
    print_recent_stats_requests(&value["stats"]["recent_requests"])
}

fn print_stats_range(value: &serde_json::Value) {
    let range = &value["range"];
    let start = range["start"].as_i64();
    let end = range["end"].as_i64();
    if start.is_none() && end.is_none() {
        println!("范围    持久化累计（跨 daemon 重启）");
    } else {
        println!(
            "范围    持久化 {} ～ {}（分钟级聚合）",
            start.map_or_else(|| "最早可用".into(), format_timestamp),
            end.map_or_else(|| "现在".into(), format_timestamp)
        );
    }
    if range["truncated"].as_bool().unwrap_or(false) {
        if range["prefix_truncated"].as_bool().unwrap_or(false) {
            let available = range["available_start"]
                .as_i64()
                .map_or_else(|| "未知".into(), format_timestamp);
            println!("提示    指定范围早于可用时间序列，结果从 {available} 起统计");
        }
        if let Some(gaps) = range["missing_ranges"].as_array() {
            for gap in gaps.iter().take(3) {
                let start = gap["start"]
                    .as_i64()
                    .map_or_else(|| "未知".into(), format_timestamp);
                let end = gap["end"]
                    .as_i64()
                    .map_or_else(|| "未知".into(), format_timestamp);
                println!("提示    {start} ～ {end} 的历史分片不可用，结果不包含该时段");
            }
            if gaps.len() > 3 {
                println!("提示    另有 {} 个历史缺口未逐项展示", gaps.len() - 3);
            }
        }
    }
}

fn print_stats_summary(summary: &serde_json::Value, latency: &serde_json::Value) {
    let requests = summary["requests"].as_u64().unwrap_or(0);
    let successes = summary["successes"].as_u64().unwrap_or(0);
    let success_rate = if requests == 0 {
        0.0
    } else {
        successes as f64 / requests as f64 * 100.0
    };
    let rows = vec![
        vec!["请求总数".into(), requests.to_string()],
        vec!["成功".into(), successes.to_string()],
        vec![
            "失败".into(),
            summary["failures"].as_u64().unwrap_or(0).to_string(),
        ],
        vec!["成功率".into(), format!("{success_rate:.1}%")],
        vec![
            "输入 Tokens".into(),
            summary["input_tokens"].as_u64().unwrap_or(0).to_string(),
        ],
        vec![
            "输出 Tokens".into(),
            summary["output_tokens"].as_u64().unwrap_or(0).to_string(),
        ],
        vec![
            "Credits".into(),
            format_credits(summary["credits"].as_f64().unwrap_or(0.0)),
        ],
        vec![
            "平均延迟".into(),
            format!("{} ms", latency["average_ms"].as_u64().unwrap_or(0)),
        ],
        vec![
            "延迟 P50/P95/P99".into(),
            format!(
                "{}/{}/{} ms",
                latency["p50_ms"].as_u64().unwrap_or(0),
                latency["p95_ms"].as_u64().unwrap_or(0),
                latency["p99_ms"].as_u64().unwrap_or(0)
            ),
        ],
    ];
    println!("{}", render_table(&["指标", "值"], &rows));
}

fn print_stats_group(dimension: &str, grouped: &serde_json::Value) -> Result<()> {
    let Some(object) = grouped.as_object() else {
        return Err(anyhow!("daemon 返回的 stats 分组数据无效"));
    };
    let mut entries = object.iter().collect::<Vec<_>>();
    entries.sort_by(|(left_name, left), (right_name, right)| {
        right["requests"]
            .as_u64()
            .cmp(&left["requests"].as_u64())
            .then_with(|| left_name.cmp(right_name))
    });
    let rows = entries
        .into_iter()
        .map(|(name, counter)| {
            vec![
                name.clone(),
                counter["requests"].as_u64().unwrap_or(0).to_string(),
                counter["successes"].as_u64().unwrap_or(0).to_string(),
                counter["failures"].as_u64().unwrap_or(0).to_string(),
                counter["input_tokens"].as_u64().unwrap_or(0).to_string(),
                counter["output_tokens"].as_u64().unwrap_or(0).to_string(),
                format_credits(counter["credits"].as_f64().unwrap_or(0.0)),
            ]
        })
        .collect::<Vec<_>>();
    println!("\n按 {dimension} 分组：");
    println!(
        "{}",
        render_table(
            &[
                dimension,
                "请求",
                "成功",
                "失败",
                "输入 Tokens",
                "输出 Tokens",
                "Credits",
            ],
            &rows,
        )
    );
    Ok(())
}

fn print_recent_stats_requests(requests: &serde_json::Value) -> Result<()> {
    let Some(requests) = requests.as_array() else {
        return Err(anyhow!("daemon 返回的最近请求数据无效"));
    };
    if requests.is_empty() {
        return Ok(());
    }
    let rows = requests
        .iter()
        .map(|request| {
            let account = request["account_name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .or_else(|| request["account_id"].as_str())
                .unwrap_or("-");
            vec![
                format_timestamp(request["timestamp"].as_i64().unwrap_or(0)),
                request["status"].as_u64().unwrap_or(0).to_string(),
                request["model"].as_str().unwrap_or("-").into(),
                account.into(),
                request["duration_ms"].as_u64().unwrap_or(0).to_string(),
                request["input_tokens"].as_u64().unwrap_or(0).to_string(),
                request["output_tokens"].as_u64().unwrap_or(0).to_string(),
                format_credits(request["credits"].as_f64().unwrap_or(0.0)),
                request["error"].as_str().unwrap_or("").into(),
            ]
        })
        .collect::<Vec<_>>();
    println!("\n最近请求：");
    println!(
        "{}",
        render_table(
            &[
                "时间",
                "状态",
                "模型",
                "账号",
                "耗时(ms)",
                "输入",
                "输出",
                "Credits",
                "错误",
            ],
            &rows,
        )
    );
    Ok(())
}

fn print_stats_by_apikey(value: &serde_json::Value) -> Result<()> {
    let Some(entries) = value.as_array() else {
        return Err(anyhow!("daemon 返回的 API key stats 无效"));
    };
    let rows = entries
        .iter()
        .map(|entry| {
            vec![
                entry["id"].as_str().unwrap_or("-").into(),
                entry["name"].as_str().unwrap_or("-").into(),
                entry["usage"]["total_requests"]
                    .as_u64()
                    .unwrap_or(0)
                    .to_string(),
                entry["usage"]["total_input_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .to_string(),
                entry["usage"]["total_output_tokens"]
                    .as_u64()
                    .unwrap_or(0)
                    .to_string(),
                format_credits(entry["usage"]["total_credits"].as_f64().unwrap_or(0.0)),
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        render_table(
            &[
                "ID",
                "名称",
                "请求",
                "输入 Tokens",
                "输出 Tokens",
                "Credits"
            ],
            &rows,
        )
    );
    Ok(())
}

fn print_human_value(value: &serde_json::Value) {
    match value {
        serde_json::Value::Array(values)
            if values.iter().all(serde_json::Value::is_object) && !values.is_empty() =>
        {
            let mut headers = Vec::<String>::new();
            for object in values.iter().filter_map(serde_json::Value::as_object) {
                for key in object.keys() {
                    if !headers.contains(key) && headers.len() < 10 {
                        headers.push(key.clone());
                    }
                }
            }
            let rows = values
                .iter()
                .filter_map(serde_json::Value::as_object)
                .map(|object| {
                    headers
                        .iter()
                        .map(|key| {
                            compact_value(object.get(key).unwrap_or(&serde_json::Value::Null))
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let labels = headers.iter().map(String::as_str).collect::<Vec<_>>();
            println!("{}", render_table(&labels, &rows));
        }
        serde_json::Value::Object(object) => {
            let rows = object
                .iter()
                .map(|(key, value)| vec![key.clone(), compact_value(value)])
                .collect::<Vec<_>>();
            println!("{}", render_table(&["字段", "值"], &rows));
        }
        _ => println!("{}", compact_value(value)),
    }
}

fn compact_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "-".into(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Array(values)
            if values
                .iter()
                .all(|value| value.is_string() || value.is_number()) =>
        {
            values
                .iter()
                .map(compact_value)
                .collect::<Vec<_>>()
                .join(", ")
        }
        _ => serde_json::to_string(value).unwrap_or_else(|_| "<invalid>".into()),
    }
}

pub fn parse_duration(value: &str) -> Result<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let amount = value[..split]
        .parse::<u64>()
        .with_context(|| format!("无效时间窗口: {value}"))?;
    let multiplier = match &value[split..] {
        "s" | "" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        unit => return Err(anyhow!("不支持的时间单位: {unit}")),
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("时间窗口过大: {value}"))
}

/// Parse Unix seconds or an RFC 3339 timestamp with an explicit timezone.
pub fn parse_timestamp(value: &str) -> Result<i64> {
    let value = value.trim();
    if let Ok(timestamp) = value.parse::<i64>() {
        return Ok(timestamp);
    }
    let (date, time_and_zone) = value
        .split_once('T')
        .or_else(|| value.split_once(' '))
        .ok_or_else(|| {
            anyhow!(
                "无效时间 {value}；请使用 Unix 秒或带时区的 RFC 3339，例如 2026-08-27T10:00:00+08:00"
            )
        })?;
    let (time, offset_seconds) = if let Some(time) = time_and_zone
        .strip_suffix('Z')
        .or_else(|| time_and_zone.strip_suffix('z'))
    {
        (time, 0i64)
    } else {
        let offset_index = time_and_zone
            .char_indices()
            .rfind(|(_, character)| matches!(character, '+' | '-'))
            .map(|(index, _)| index)
            .ok_or_else(|| anyhow!("时间必须包含时区：{value}；例如使用 Z 或 +08:00"))?;
        let (time, offset) = time_and_zone.split_at(offset_index);
        (time, parse_timezone_offset(offset, value)?)
    };
    let (year, month, day) = parse_date(date, value)?;
    let (hour, minute, second) = parse_clock(time, value)?;
    let days = days_from_civil(year, month, day);
    days.checked_mul(86_400)
        .and_then(|timestamp| timestamp.checked_add(i64::from(hour) * 3_600))
        .and_then(|timestamp| timestamp.checked_add(i64::from(minute) * 60))
        .and_then(|timestamp| timestamp.checked_add(i64::from(second)))
        .and_then(|timestamp| timestamp.checked_sub(offset_seconds))
        .ok_or_else(|| anyhow!("时间超出支持范围: {value}"))
}

fn parse_date(value: &str, original: &str) -> Result<(i64, u32, u32)> {
    let mut parts = value.split('-');
    let year = parts.next().and_then(|value| value.parse::<i64>().ok());
    let month = parts.next().and_then(|value| value.parse::<u32>().ok());
    let day = parts.next().and_then(|value| value.parse::<u32>().ok());
    if parts.next().is_some() {
        return Err(anyhow!("无效日期: {original}"));
    }
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Err(anyhow!("无效日期: {original}"));
    };
    let maximum_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return Err(anyhow!("无效月份: {original}")),
    };
    if day == 0 || day > maximum_day {
        return Err(anyhow!("无效日期: {original}"));
    }
    Ok((year, month, day))
}

fn parse_clock(value: &str, original: &str) -> Result<(u32, u32, u32)> {
    let value = value.split('.').next().unwrap_or(value);
    let mut parts = value.split(':');
    let hour = parts.next().and_then(|value| value.parse::<u32>().ok());
    let minute = parts.next().and_then(|value| value.parse::<u32>().ok());
    let second = parts.next().and_then(|value| value.parse::<u32>().ok());
    if parts.next().is_some() {
        return Err(anyhow!("无效时间: {original}"));
    }
    let (Some(hour), Some(minute), Some(second)) = (hour, minute, second) else {
        return Err(anyhow!("无效时间: {original}"));
    };
    if hour > 23 || minute > 59 || second > 59 {
        return Err(anyhow!("无效时间: {original}"));
    }
    Ok((hour, minute, second))
}

fn parse_timezone_offset(value: &str, original: &str) -> Result<i64> {
    let sign = match value.as_bytes().first().copied() {
        Some(b'+') => 1i64,
        Some(b'-') => -1i64,
        _ => return Err(anyhow!("无效时间时区: {original}")),
    };
    let Some((hours, minutes)) = value[1..].split_once(':') else {
        return Err(anyhow!("无效时间时区: {original}"));
    };
    let hours = hours
        .parse::<u32>()
        .map_err(|_| anyhow!("无效时间时区: {original}"))?;
    let minutes = minutes
        .parse::<u32>()
        .map_err(|_| anyhow!("无效时间时区: {original}"))?;
    if hours > 23 || minutes > 59 {
        return Err(anyhow!("无效时间时区: {original}"));
    }
    Ok(sign * (i64::from(hours) * 3_600 + i64::from(minutes) * 60))
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_prime = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub async fn show_logs(
    client: &mut AdminClient,
    tail: usize,
    follow: bool,
    level: Option<&str>,
    account: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut after_request_id: Option<String> = None;
    loop {
        let value: serde_json::Value = client
            .call(
                method::LOGS,
                serde_json::json!({
                    "after_request_id":after_request_id,
                    "tail":tail,
                    "wait_ms":if follow {30_000} else {0},
                    "level":level,
                    "account":account
                }),
            )
            .await?;
        for request in value["entries"].as_array().into_iter().flatten() {
            let request_id = request["request_id"].as_str().unwrap_or_default();
            if !request_id.is_empty() {
                after_request_id = Some(request_id.to_string());
            }
            if json {
                println!("{}", serde_json::to_string(request)?);
            } else {
                let account = log_account(request);
                let models = log_model_route(request);
                println!(
                    "{} {:>3} {:>6}ms account={} model={}",
                    format_timestamp(request["timestamp"].as_i64().unwrap_or_default()),
                    request["status"].as_u64().unwrap_or_default(),
                    request["duration_ms"].as_u64().unwrap_or_default(),
                    account,
                    models.original,
                );
                if let Some(rule) = models.mapping_rule {
                    println!(
                        "  mapping rule={} path={} -> {}",
                        rule, models.original, models.routed
                    );
                } else if models.original != models.routed {
                    println!(
                        "  fallback_routing path={} -> {}",
                        models.original, models.routed
                    );
                }
                if models.routed != models.resolved {
                    println!(
                        "  auto_resolution path={} -> {}",
                        models.routed, models.resolved
                    );
                }
                println!(
                    "  request path={} endpoint={} trace={} request_id={}",
                    request["path"].as_str().unwrap_or("-"),
                    request["endpoint"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("-"),
                    request["trace_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("-"),
                    request["request_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("-"),
                );
                if let Some(error) = request["error"].as_str().filter(|value| !value.is_empty()) {
                    println!("  error: {error}");
                }
                let diagnostics = &request["diagnostics"];
                let error_code = diagnostics["error_code"]
                    .as_str()
                    .filter(|value| !value.is_empty());
                let error_stage = diagnostics["error_stage"]
                    .as_str()
                    .filter(|value| !value.is_empty());
                if error_code.is_some() || error_stage.is_some() {
                    let upstream_status = diagnostics["upstream_status"]
                        .as_u64()
                        .map_or_else(|| "-".into(), |status| status.to_string());
                    println!(
                        "  diagnostics code={} stage={} client_status={} upstream_status={} account_error={}",
                        error_code.unwrap_or("-"),
                        error_stage.unwrap_or("-"),
                        diagnostics["client_status"]
                            .as_u64()
                            .unwrap_or_else(|| request["status"].as_u64().unwrap_or_default()),
                        upstream_status,
                        diagnostics["account_error"].as_bool().unwrap_or(false),
                    );
                    if error_code == Some("model_not_available") {
                        println!("  hint: kproxy models resolve {}", models.original);
                    }
                }
                for attempt in request["attempts"].as_array().into_iter().flatten() {
                    let status = attempt["status"]
                        .as_u64()
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "-".into());
                    let available_models = attempt["available_models"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",");
                    let available_models = if available_models.is_empty() {
                        String::new()
                    } else {
                        format!(" available_models=[{available_models}]")
                    };
                    println!(
                        "  attempt={} account={} model={} endpoint={} upstream_status={} error={}{}",
                        attempt["attempt"].as_u64().unwrap_or_default(),
                        log_account(attempt),
                        attempt["model"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .unwrap_or("-"),
                        attempt["endpoint"]
                            .as_str()
                            .filter(|value| !value.is_empty())
                            .unwrap_or("-"),
                        status,
                        attempt["error"].as_str().unwrap_or(""),
                        available_models,
                    );
                }
            }
        }
        if !follow {
            return Ok(());
        }
    }
}

pub async fn show_log_files(
    client: &mut AdminClient,
    level: Option<&str>,
    paths_only: bool,
    json: bool,
) -> Result<()> {
    let mut result: LogFilesResult = client
        .call(method::LOG_FILES, serde_json::json!({}))
        .await?;
    let host_data_dir = std::env::var_os(WRAPPER_HOST_DATA_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    populate_host_log_paths(
        &mut result,
        host_data_dir.as_deref(),
        &Paths::from_env().data_dir,
    );
    if let Some(level) = level {
        result.files.retain(|file| file.level == level);
    }
    if json {
        if paths_only {
            let mut output = serde_json::to_value(&result)?;
            output
                .as_object_mut()
                .expect("LogFilesResult serializes as an object")
                .remove("files");
            return print_json(&output);
        }
        return print_json(&result);
    }
    println!("日志目录    {}", result.directory);
    if let Some(host_directory) = &result.host_directory {
        println!("宿主机目录  {host_directory}");
    }
    println!("基础路径    {}", result.base_path);
    if let Some(host_base_path) = &result.host_base_path {
        println!("宿主机基础路径 {host_base_path}");
    }
    println!("格式/过滤   {} / {}", result.format, result.level_filter);
    if paths_only {
        return Ok(());
    }
    if result.files.is_empty() {
        println!("暂无匹配的日志文件；对应级别产生日志后会自动创建。");
        return Ok(());
    }
    let has_host_paths = result.files.iter().any(|file| file.host_path.is_some());
    let rows = result
        .files
        .into_iter()
        .map(|file| {
            let display_path = file.host_path.unwrap_or(file.path);
            vec![
                file.date,
                file.level,
                format_bytes(file.size_bytes),
                file.modified_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "-".into()),
                display_path,
            ]
        })
        .collect::<Vec<_>>();
    let path_heading = if has_host_paths {
        "宿主机文件路径"
    } else {
        "文件路径"
    };
    println!(
        "{}",
        render_table(&["日期", "级别", "大小", "修改时间", path_heading], &rows)
    );
    Ok(())
}

const WRAPPER_HOST_DATA_DIR_ENV: &str = "KPROXY_WRAPPER_HOST_DATA_DIR";

fn populate_host_log_paths(
    result: &mut LogFilesResult,
    host_data_dir: Option<&Path>,
    container_data_dir: &Path,
) {
    let Some(host_data_dir) = host_data_dir else {
        return;
    };
    result.host_base_path = host_log_path(&result.base_path, host_data_dir, container_data_dir);
    result.host_directory = host_log_path(&result.directory, host_data_dir, container_data_dir);
    for file in &mut result.files {
        file.host_path = host_log_path(&file.path, host_data_dir, container_data_dir);
    }
}

fn host_log_path(path: &str, host_data_dir: &Path, container_data_dir: &Path) -> Option<String> {
    let relative = Path::new(path).strip_prefix(container_data_dir).ok()?;
    Some(host_data_dir.join(relative).display().to_string())
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

fn log_account(value: &serde_json::Value) -> String {
    let id = value["account_id"]
        .as_str()
        .filter(|value| !value.is_empty());
    let name = value["account_name"]
        .as_str()
        .filter(|value| !value.is_empty());
    match (name, id) {
        (Some(name), Some(id)) if name != id => format!("{name} ({id})"),
        (Some(name), _) => name.to_owned(),
        (_, Some(id)) => id.to_owned(),
        _ => "-".into(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct LogModelRoute<'a> {
    original: &'a str,
    routed: &'a str,
    resolved: &'a str,
    mapping_rule: Option<&'a str>,
}

fn log_model_route(request: &serde_json::Value) -> LogModelRoute<'_> {
    let original = request["original_model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .or_else(|| request["model"].as_str())
        .unwrap_or("-");
    let routed = request["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .unwrap_or(original);
    let resolved = request["kiro_model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .unwrap_or(routed);
    LogModelRoute {
        original,
        routed,
        resolved,
        mapping_rule: request["model_mapping_rule"].as_str(),
    }
}

pub async fn run_alert(client: &mut AdminClient, command: AlertCommand, json: bool) -> Result<()> {
    match command {
        AlertCommand::Config => show_alert_config(json),
        AlertCommand::Events => show_alert_events(json),
        AlertCommand::Platforms => show_alert_platforms(json),
        AlertCommand::List => {
            simple_rpc(client, method::WEBHOOK_LIST, serde_json::json!({}), json).await
        }
        AlertCommand::Add {
            name,
            platform,
            webhook_url,
            events,
            disabled,
            dingtalk_sign,
            telegram_chat_id,
            custom_template,
        } => {
            mutate_config_array(client, "webhook", |array| {
                if array.iter().any(|value| named_value_matches(value, &name)) {
                    return Err(anyhow!("告警目标已存在: {name}"));
                }
                let mut table = toml::map::Map::new();
                table.insert("name".into(), toml::Value::String(name.clone()));
                table.insert("kind".into(), toml::Value::String(platform.as_str().into()));
                table.insert("url".into(), toml::Value::String(webhook_url.clone()));
                table.insert("enabled".into(), toml::Value::Boolean(!disabled));
                table.insert("events".into(), alert_event_array_value(&events));
                insert_optional_string(&mut table, "dingtalk_sign", dingtalk_sign.as_deref());
                insert_optional_string(&mut table, "telegram_chat_id", telegram_chat_id.as_deref());
                insert_optional_string(&mut table, "custom_template", custom_template.as_deref());
                array.push(toml::Value::Table(table));
                Ok(())
            })
            .await?;
            println!("已添加告警目标 {name}");
            Ok(())
        }
        AlertCommand::Edit {
            target,
            name,
            rename,
            platform,
            webhook_url,
            events,
            clear_events,
            enable,
            disable,
            dingtalk_sign,
            clear_dingtalk_sign,
            telegram_chat_id,
            clear_telegram_chat_id,
            custom_template,
            clear_custom_template,
        } => {
            let name = target
                .or(name)
                .ok_or_else(|| anyhow!("需指定告警目标名称"))?;
            mutate_config_array(client, "webhook", |array| {
                let table = find_named_table_mut(array, &name, "告警目标")?;
                replace_optional_string(table, "name", rename.as_deref());
                replace_optional_string(table, "kind", platform.map(AlertPlatform::as_str));
                replace_optional_string(table, "url", webhook_url.as_deref());
                replace_or_clear_optional_string(
                    table,
                    "dingtalk_sign",
                    dingtalk_sign.as_deref(),
                    clear_dingtalk_sign,
                );
                replace_or_clear_optional_string(
                    table,
                    "telegram_chat_id",
                    telegram_chat_id.as_deref(),
                    clear_telegram_chat_id,
                );
                replace_or_clear_optional_string(
                    table,
                    "custom_template",
                    custom_template.as_deref(),
                    clear_custom_template,
                );
                if clear_events {
                    table.insert("events".into(), toml::Value::Array(Vec::new()));
                } else if !events.is_empty() {
                    table.insert("events".into(), alert_event_array_value(&events));
                }
                if enable || disable {
                    table.insert("enabled".into(), toml::Value::Boolean(enable));
                }
                Ok(())
            })
            .await?;
            println!("已更新告警目标 {name}");
            Ok(())
        }
        AlertCommand::Delete { name } => {
            if !crate::commands::confirm(&format!("确认删除告警目标 {name}？")).await? {
                println!("已取消");
                return Ok(());
            }
            mutate_config_array(client, "webhook", |array| {
                remove_named_value(array, &name, "告警目标")
            })
            .await?;
            println!("已删除告警目标 {name}");
            Ok(())
        }
        AlertCommand::Test { name, all } => {
            if name.is_none() && !all {
                return Err(anyhow!("需指定告警目标名称或 --all"));
            }
            simple_rpc(
                client,
                method::WEBHOOK_TEST,
                serde_json::json!({"name":name}),
                json,
            )
            .await
        }
        AlertCommand::Logs { tail } => {
            simple_rpc(
                client,
                method::WEBHOOK_LOGS,
                serde_json::json!({"tail":tail}),
                json,
            )
            .await
        }
    }
}

#[derive(serde::Serialize)]
struct AlertEventInfo {
    event: &'static str,
    condition: &'static str,
}

fn alert_event_catalog() -> [AlertEventInfo; 4] {
    [
        AlertEventInfo {
            event: AlertEvent::AccountCreditProtected.as_str(),
            condition:
                "单个启用账号仍有额度，但达到 pool.low_credit_min_remaining 保护阈值并暂停调度；额度恢复后才允许再次告警。",
        },
        AlertEventInfo {
            event: AlertEvent::AccountQuotaExhausted.as_str(),
            condition:
                "单个启用账号的额度完全耗尽；同一次异常只告警一次，额度恢复后才允许再次告警。",
        },
        AlertEventInfo {
            event: AlertEvent::ServiceQuotaExhausted.as_str(),
            condition: "API 代理服务共享的全部启用账号额度完全耗尽；服务恢复前只告警一次。",
        },
        AlertEventInfo {
            event: AlertEvent::TokenRefreshFailed.as_str(),
            condition: "后台或请求触发的 Token 刷新失败；同一账号刷新成功前只告警一次。",
        },
    ]
}

pub fn show_alert_events(json: bool) -> Result<()> {
    let events = alert_event_catalog();
    if json {
        return print_json(&events);
    }
    let rows = events
        .iter()
        .map(|event| vec![event.event.into(), event.condition.into()])
        .collect::<Vec<_>>();
    println!("{}", render_table(&["EVENT", "触发条件"], &rows));
    println!("\n多选方式：重复使用 `--event`，或用逗号分隔多个事件。");
    Ok(())
}

#[derive(serde::Serialize)]
struct AlertPlatformInfo {
    platform: &'static str,
    description: &'static str,
    platform_options: &'static str,
}

fn alert_platform_catalog() -> [AlertPlatformInfo; 6] {
    [
        AlertPlatformInfo {
            platform: AlertPlatform::Dingtalk.as_str(),
            description: "钉钉群机器人",
            platform_options: "--dingtalk-sign（机器人启用加签时使用）",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::WechatWork.as_str(),
            description: "企业微信群机器人",
            platform_options: "无",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Feishu.as_str(),
            description: "飞书群机器人",
            platform_options: "无",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Telegram.as_str(),
            description: "Telegram Bot API",
            platform_options: "--telegram-chat-id（必填）",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Discord.as_str(),
            description: "Discord Webhook",
            platform_options: "无",
        },
        AlertPlatformInfo {
            platform: AlertPlatform::Custom.as_str(),
            description: "自定义 Webhook",
            platform_options: "--custom-template（可选）",
        },
    ]
}

pub fn show_alert_platforms(json: bool) -> Result<()> {
    let platforms = alert_platform_catalog();
    if json {
        return print_json(&platforms);
    }
    let rows = platforms
        .iter()
        .map(|platform| {
            vec![
                platform.platform.into(),
                platform.description.into(),
                platform.platform_options.into(),
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        render_table(&["PLATFORM", "通知平台", "平台专用参数"], &rows)
    );
    println!(
        "\n所有平台都需要 --webhook-url；旧参数 --url 和 --kind 继续兼容，建议新命令使用 --webhook-url 和 --platform。"
    );
    Ok(())
}

fn show_alert_config(json: bool) -> Result<()> {
    if json {
        return print_json(&serde_json::json!({
            "mode":"once_until_recovery",
            "format":"markdown",
            "account_event_aggregation":"same_target_and_event_type",
            "events":alert_event_catalog().map(|event| event.event),
        }));
    }
    println!("告警模式  异常期间只发送一次，恢复后再次异常才重新告警");
    println!("账号聚合  同一目标的同类型多账号事件合并发送");
    println!("消息格式  Markdown");
    println!("事件类型  账号剩余额度保护、单账号额度耗尽、服务全部账号额度耗尽、Token 刷新失败");
    Ok(())
}

pub async fn run_apikey(
    client: &mut AdminClient,
    command: ApiKeyCommand,
    json: bool,
) -> Result<()> {
    match command {
        ApiKeyCommand::List { detail } => show_key_list(client, detail, json).await,
        ApiKeyCommand::Show { id } => show_keys(client, Some(&id), None, json).await,
        ApiKeyCommand::Usage { id } => show_keys(client, Some(&id), None, json).await,
        ApiKeyCommand::History { id, tail } => show_keys(client, Some(&id), Some(tail), json).await,
        ApiKeyCommand::ResetUsage { id } => {
            if !crate::commands::confirm(&format!("确认重置 API key {id} 的全部用量统计？")).await?
            {
                println!("已取消");
                return Ok(());
            }
            simple_rpc(
                client,
                method::APIKEY_RESET_USAGE,
                serde_json::json!({"id":id}),
                json,
            )
            .await
        }
        ApiKeyCommand::Add {
            name,
            format,
            credits_limit,
        } => {
            let key = generate_key(&format)?;
            let id = key_id(&key);
            mutate_config_array(client, "api_key", |array| {
                let mut table = toml::map::Map::new();
                table.insert("id".into(), toml::Value::String(id.clone()));
                table.insert("name".into(), toml::Value::String(name.clone()));
                table.insert("key".into(), toml::Value::String(key.clone()));
                table.insert("format".into(), toml::Value::String(format.clone()));
                table.insert("enabled".into(), toml::Value::Boolean(true));
                if let Some(limit) = credits_limit {
                    table.insert("credits_limit".into(), toml::Value::Float(limit));
                }
                array.push(toml::Value::Table(table));
                Ok(())
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"id":id,"name":name,"key":key}))?;
            } else {
                println!("已创建 {id} ({name})\n请立即保存密钥；之后列表不再显示：\n{key}");
            }
            Ok(())
        }
        ApiKeyCommand::Rm { id } => {
            if !crate::commands::confirm(&format!("确认删除 API key {id}？")).await? {
                println!("已取消");
                return Ok(());
            }
            mutate_config_array(client, "api_key", |array| {
                let before = array.len();
                array.retain(|value| !matches_key(value, &id));
                (array.len() < before)
                    .then_some(())
                    .ok_or_else(|| anyhow!("API key not found: {id}"))
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"removed":true,"id":id}))
            } else {
                println!("已删除 API key {id}");
                Ok(())
            }
        }
        ApiKeyCommand::Enable { id } => {
            mutate_key_and_reload(client, &id, "enabled", toml::Value::Boolean(true)).await?;
            report_apikey_change(client, &id, "已启用", json).await
        }
        ApiKeyCommand::Disable { id } => {
            mutate_key_and_reload(client, &id, "enabled", toml::Value::Boolean(false)).await?;
            report_apikey_change(client, &id, "已停用", json).await
        }
        ApiKeyCommand::Limit { id, credits, clear } => {
            if clear {
                clear_key_field_and_reload(client, &id, "credits_limit").await?;
            } else {
                mutate_key_and_reload(
                    client,
                    &id,
                    "credits_limit",
                    toml::Value::Float(credits.expect("clap requires --credits")),
                )
                .await?;
            }
            report_apikey_change(client, &id, "已更新额度上限", json).await
        }
    }
}

async fn report_apikey_change(
    client: &mut AdminClient,
    id: &str,
    message: &str,
    json: bool,
) -> Result<()> {
    if json {
        show_keys(client, Some(id), None, true).await
    } else {
        println!("{message} API key {id}");
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct ApiKeyListEntry {
    id: String,
    name: String,
    enabled: bool,
    credits_limit: Option<f64>,
    #[serde(default)]
    reserved_credits: f64,
    #[serde(default)]
    usage: ApiKeyListUsage,
}

#[derive(Debug, Default, Deserialize)]
struct ApiKeyListUsage {
    #[serde(default)]
    total_requests: u64,
    #[serde(default)]
    total_credits: f64,
    #[serde(default)]
    total_input_tokens: u64,
    #[serde(default)]
    total_output_tokens: u64,
}

#[derive(Debug, Default)]
struct ApiKeyListSummary {
    total: usize,
    enabled: usize,
    total_requests: u64,
    total_credits: f64,
    reserved_credits: f64,
    total_input_tokens: u64,
    total_output_tokens: u64,
}

impl ApiKeyListSummary {
    fn from_entries(entries: &[ApiKeyListEntry]) -> Self {
        entries.iter().fold(Self::default(), |mut summary, entry| {
            summary.total += 1;
            summary.enabled += usize::from(entry.enabled);
            summary.total_requests += entry.usage.total_requests;
            summary.total_credits += entry.usage.total_credits;
            summary.reserved_credits += entry.reserved_credits;
            summary.total_input_tokens += entry.usage.total_input_tokens;
            summary.total_output_tokens += entry.usage.total_output_tokens;
            summary
        })
    }

    fn disabled(&self) -> usize {
        self.total.saturating_sub(self.enabled)
    }
}

async fn show_key_list(client: &mut AdminClient, detail: bool, json: bool) -> Result<()> {
    let value: serde_json::Value = client
        .call(method::APIKEY_LIST, serde_json::json!({}))
        .await?;
    let mut entries = serde_json::from_value::<Vec<ApiKeyListEntry>>(value)
        .context("daemon 返回的 API key 列表无效")?;
    entries.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.id.cmp(&right.id))
    });
    let summary = ApiKeyListSummary::from_entries(&entries);

    if json {
        return print_json(&apikey_list_json(&entries, &summary, detail));
    }
    if entries.is_empty() {
        println!("暂无 API key。");
        return Ok(());
    }

    let rows = entries
        .iter()
        .map(|entry| {
            let mut row = vec![
                entry.id.clone(),
                entry.name.clone(),
                if entry.enabled { "enabled" } else { "disabled" }.into(),
                entry
                    .credits_limit
                    .map(format_credits)
                    .unwrap_or_else(|| "-".into()),
            ];
            if detail {
                row.extend([
                    entry.usage.total_requests.to_string(),
                    entry.usage.total_input_tokens.to_string(),
                    entry.usage.total_output_tokens.to_string(),
                    format_credits(entry.usage.total_credits),
                    format_credits(entry.reserved_credits),
                ]);
            }
            row
        })
        .collect::<Vec<_>>();
    let headers = if detail {
        vec![
            "ID",
            "名称",
            "状态",
            "Credits 上限",
            "请求",
            "输入 Tokens",
            "输出 Tokens",
            "Credits",
            "预留 Credits",
        ]
    } else {
        vec!["ID", "名称", "状态", "Credits 上限"]
    };
    println!("{}", render_table(&headers, &rows));
    if detail {
        println!(
            "总计：{} 个 API key，{} 启用 / {} 停用，{} 请求，{} 输入 tokens，{} 输出 tokens，{} credits，{} 预留。",
            summary.total,
            summary.enabled,
            summary.disabled(),
            summary.total_requests,
            summary.total_input_tokens,
            summary.total_output_tokens,
            format_credits(summary.total_credits),
            format_credits(summary.reserved_credits),
        );
    } else {
        println!(
            "总计：{} 个 API key，{} 启用 / {} 停用，{} 请求，{} 输入 tokens，{} 输出 tokens，{} credits，{} 预留。使用 --detail 查看分 key 消耗。",
            summary.total,
            summary.enabled,
            summary.disabled(),
            summary.total_requests,
            summary.total_input_tokens,
            summary.total_output_tokens,
            format_credits(summary.total_credits),
            format_credits(summary.reserved_credits),
        );
    }
    Ok(())
}

fn apikey_list_json(
    entries: &[ApiKeyListEntry],
    summary: &ApiKeyListSummary,
    detail: bool,
) -> serde_json::Value {
    let api_keys = entries
        .iter()
        .map(|entry| {
            let mut value = serde_json::json!({
                "id":entry.id,
                "name":entry.name,
                "enabled":entry.enabled,
                "credits_limit":entry.credits_limit,
            });
            if detail {
                value["total_requests"] = entry.usage.total_requests.into();
                value["total_input_tokens"] = entry.usage.total_input_tokens.into();
                value["total_output_tokens"] = entry.usage.total_output_tokens.into();
                value["total_credits"] = entry.usage.total_credits.into();
                value["reserved_credits"] = entry.reserved_credits.into();
            }
            value
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "summary":{
            "total":summary.total,
            "enabled":summary.enabled,
            "disabled":summary.disabled(),
            "total_requests":summary.total_requests,
            "total_input_tokens":summary.total_input_tokens,
            "total_output_tokens":summary.total_output_tokens,
            "total_credits":summary.total_credits,
            "reserved_credits":summary.reserved_credits,
        },
        "api_keys":api_keys,
    })
}

fn format_credits(value: f64) -> String {
    format!("{value:.2}")
}

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

pub async fn edit_config(client: &mut AdminClient) -> Result<()> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = std::path::PathBuf::from(paths.config_file);
    let original = tokio::fs::read(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let editor = resolve_editor()?;
    let mut command = tokio::process::Command::new(&editor.program);
    command.args(&editor.args).arg(&path);
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
    if let Err(error) = validate_config(Some(path.to_string_lossy().as_ref())).await {
        kproxy_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
        return Err(error.context("配置无效，磁盘文件已回滚"));
    }
    let result: ConfigReloadResult = client
        .call(method::CONFIG_RELOAD, serde_json::json!({}))
        .await?;
    if !result.applied {
        kproxy_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
        let rollback: ConfigReloadResult = client
            .call(method::CONFIG_RELOAD, serde_json::json!({}))
            .await?;
        if !rollback.applied {
            return Err(anyhow!("配置重载失败，且回滚配置也无法重载"));
        }
        return Err(anyhow!(
            "配置重载失败，磁盘文件已回滚：{}",
            result.error.unwrap_or_else(|| "未知错误".into())
        ));
    }
    println!("配置已保存并重载");
    Ok(())
}

/// Successful `config reset` result.
pub struct ConfigResetResult {
    pub config_file: PathBuf,
    pub backup_file: PathBuf,
    pub needs_restart: Vec<String>,
}

/// Back up the current configuration, replace it with the commented defaults, and reload it.
pub async fn reset_config(client: &mut AdminClient) -> Result<Option<ConfigResetResult>> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = PathBuf::from(paths.config_file);
    if !crate::commands::confirm(
        "确认将全部配置恢复为默认设置？代理服务、API key、模型映射和告警目标会被清除",
    )
    .await?
    {
        return Ok(None);
    }

    let original = tokio::fs::read(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let defaults = kproxy_store::bootstrap::render_default_config(
        &kproxy_core::config::Config::default().admin.socket,
    );
    let config: kproxy_core::config::Config =
        toml::from_str(&defaults).context("内置默认配置无法解析")?;
    config.validate().context("内置默认配置校验失败")?;

    let backup_file = write_config_backup(&path, &original).await?;
    kproxy_store::atomic::write_bytes_atomically(&path, defaults.as_bytes(), Some(0o600)).await?;
    let result: ConfigReloadResult = match client
        .call(method::CONFIG_RELOAD, serde_json::json!({}))
        .await
    {
        Ok(result) => result,
        Err(reload_error) => {
            kproxy_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
            let rollback = client
                .call::<ConfigReloadResult>(method::CONFIG_RELOAD, serde_json::json!({}))
                .await;
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
            needs_restart: result.needs_restart,
        }));
    }

    kproxy_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
    let rollback: ConfigReloadResult = client
        .call(method::CONFIG_RELOAD, serde_json::json!({}))
        .await?;
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
            "配置默认位于 $KPROXY_HOME/config.toml，修改后热重载；server.host/port、admin.socket 和 TLS 监听变更需要重启。\n`kproxy config validate [file]` 只校验，`kproxy config edit` 保存后校验并重载，`kproxy config show --effective` 查看合并默认值的结果。`kproxy config reset` 确认后备份原文件、恢复全部默认设置并重载。"
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
            "`kproxy logs show` 查看内存中的结构化请求日志，`follow` 持续跟踪；支持 `--level`、`--account` 和 `--tail`。`kproxy logs files [--level error]` 列出实际日志文件，`logs path` 显示目录、基础路径和当前日志配置。旧的 `kproxy logs --tail ...` 与 `kproxy logs -f` 继续兼容。"
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
mod tests {
    use super::*;

    fn pool_output_fixture() -> PoolOutput {
        PoolOutput {
            model: "claude-opus-5".into(),
            queued: 2,
            scoring: Some(PoolScoringOutput {
                weight_active: 0.5,
                weight_credit: 0.4,
                weight_idle: 0.1,
                max_concurrent_per_account: 50,
                idle_window_ms: 300_000,
            }),
            accounts: vec![
                PoolAccountOutput {
                    account_id: "acc_ready".into(),
                    account_name: "Primary account".into(),
                    score: Some(0.123_456),
                    active_factor: 0.02,
                    credit_factor: 0.25,
                    idle_factor: 0.5,
                    eligible: true,
                    reason: "available".into(),
                },
                PoolAccountOutput {
                    account_id: "acc_exhausted".into(),
                    account_name: "Exhausted account".into(),
                    score: None,
                    active_factor: 0.0,
                    credit_factor: 0.0,
                    idle_factor: 0.0,
                    eligible: false,
                    reason: "exhausted".into(),
                },
                PoolAccountOutput {
                    account_id: "acc_wrong_model".into(),
                    account_name: "Different subscription".into(),
                    score: None,
                    active_factor: 0.0,
                    credit_factor: 0.0,
                    idle_factor: 0.0,
                    eligible: false,
                    reason: "model_unavailable".into(),
                },
            ],
        }
    }

    #[test]
    fn pool_default_view_is_compact_and_folds_unavailable_accounts() {
        let output = render_pool_output(&pool_output_fixture(), false);
        assert!(output.contains("模型 claude-opus-5  排队 2  可调度 1/3"));
        assert!(output.contains("额度耗尽 1"));
        assert!(output.contains("模型不支持 1"));
        assert!(output.contains("acc_ready"));
        assert!(output.contains("0.1235"));
        assert!(output.contains("评分说明：越低越优"));
        assert!(output.contains("使用 --explain 查看公式"));
        assert!(!output.contains("acc_exhausted"));
        assert!(!output.contains("acc_wrong_model"));
        assert!(!output.contains('{'));
    }

    #[test]
    fn pool_explain_view_shows_all_accounts_and_factors() {
        let output = render_pool_output(&pool_output_fixture(), true);
        assert!(output.contains("acc_ready"));
        assert!(output.contains("acc_exhausted"));
        assert!(output.contains("acc_wrong_model"));
        assert!(output.contains("并发"));
        assert!(output.contains("2.0%"));
        assert!(output.contains("25.0%"));
        assert!(output.contains("50.0%"));
        assert!(output.contains("额度耗尽"));
        assert!(output.contains("模型不支持"));
        assert!(output.contains("评分 = 并发 × 0.5 + 额度 × 0.4 + 近期使用 × 0.1"));
        assert!(output.contains("并发 = 活跃请求数 ÷ 50"));
        assert!(output.contains("空闲 5 分钟后降为 0%"));
        assert!(output.contains("完全同分时会加入极小随机量打破平局"));
    }

    #[test]
    fn credits_are_displayed_with_two_decimal_places() {
        assert_eq!(format_credits(12.345), "12.35");
        assert_eq!(format_credits(4.0), "4.00");
    }

    #[tokio::test]
    async fn config_backup_never_overwrites_an_existing_backup() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        std::fs::write(config.with_file_name("config.toml.bak"), b"first")
            .expect("existing backup");

        let backup = write_config_backup(&config, b"current")
            .await
            .expect("new backup");

        assert_eq!(backup, config.with_file_name("config.toml.bak.1"));
        assert_eq!(std::fs::read(backup).expect("backup contents"), b"current");
        assert_eq!(
            std::fs::read(config.with_file_name("config.toml.bak")).expect("original backup"),
            b"first"
        );
    }

    #[test]
    fn configured_editor_preserves_quoted_arguments() {
        assert_eq!(
            parse_editor("code --wait --profile 'K Proxy'").expect("editor command"),
            EditorCommand {
                program: PathBuf::from("code"),
                args: vec!["--wait".into(), "--profile".into(), "K Proxy".into()],
            }
        );
        assert!(parse_editor("code '").is_err());
    }

    #[test]
    fn editor_uses_utf8_locale_when_effective_locale_is_not_utf8() {
        use std::ffi::OsStr;

        assert!(editor_needs_utf8_locale(None, None, None));
        assert!(editor_needs_utf8_locale(
            Some(OsStr::new("C")),
            Some(OsStr::new("zh_CN.UTF-8")),
            Some(OsStr::new("zh_CN.UTF-8")),
        ));
        assert!(editor_needs_utf8_locale(
            None,
            None,
            Some(OsStr::new("zh_CN.GB18030")),
        ));
        assert!(!editor_needs_utf8_locale(
            None,
            Some(OsStr::new("C.utf8")),
            Some(OsStr::new("C")),
        ));
        assert!(!editor_needs_utf8_locale(
            None,
            None,
            Some(OsStr::new("en_US.UTF-8")),
        ));
    }

    #[test]
    fn docker_wrapper_maps_log_files_to_the_host_data_volume() {
        let mut result = LogFilesResult {
            base_path: "/var/lib/kproxy/logs/kproxyd.log".into(),
            host_base_path: None,
            directory: "/var/lib/kproxy/logs".into(),
            host_directory: None,
            format: "json".into(),
            level_filter: "info".into(),
            files: vec![kproxy_ipc::protocol::LogFileView {
                path: "/var/lib/kproxy/logs/kproxyd-2026-08-24-info.log".into(),
                host_path: None,
                level: "info".into(),
                date: "2026-08-24".into(),
                size_bytes: 42,
                modified_at: None,
            }],
        };

        populate_host_log_paths(
            &mut result,
            Some(Path::new(
                "/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data",
            )),
            Path::new("/var/lib/kproxy"),
        );

        assert_eq!(
            result.host_directory.as_deref(),
            Some("/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data/logs")
        );
        assert_eq!(
            result.host_base_path.as_deref(),
            Some("/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data/logs/kproxyd.log")
        );
        assert_eq!(
            result.files[0].host_path.as_deref(),
            Some(
                "/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data/logs/\
                 kproxyd-2026-08-24-info.log"
            )
        );
    }

    #[test]
    fn logs_outside_the_data_volume_do_not_claim_a_host_path() {
        assert_eq!(
            host_log_path(
                "/tmp/kproxyd.log",
                Path::new("/var/lib/docker/volumes/kproxy/_data"),
                Path::new("/var/lib/kproxy"),
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn default_editor_prefers_vim_over_vi() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let vim = directory.path().join("vim");
        let vi = directory.path().join("vi");
        std::fs::write(&vim, "").expect("vim");
        std::fs::write(&vi, "").expect("vi");
        std::fs::set_permissions(&vim, std::fs::Permissions::from_mode(0o755))
            .expect("vim executable permissions");
        std::fs::set_permissions(&vi, std::fs::Permissions::from_mode(0o755))
            .expect("vi executable permissions");

        assert_eq!(
            find_default_editor(Some(directory.path().as_os_str())),
            Some(vim)
        );
    }

    #[cfg(unix)]
    #[test]
    fn default_editor_uses_first_executable_candidate_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let first = tempfile::tempdir().expect("first temp directory");
        let second = tempfile::tempdir().expect("second temp directory");
        let non_executable_vi = first.path().join("vi");
        std::fs::write(&non_executable_vi, "").expect("non-executable vi");
        let vim = second.path().join("vim");
        std::fs::write(&vim, "").expect("vim");
        std::fs::set_permissions(&vim, std::fs::Permissions::from_mode(0o755))
            .expect("executable permissions");
        let path = std::env::join_paths([first.path(), second.path()]).expect("PATH");

        assert_eq!(find_default_editor(Some(path.as_os_str())), Some(vim));
    }

    #[test]
    fn log_account_prefers_name_and_keeps_id_for_diagnostics() {
        let value = serde_json::json!({
            "account_id": "acc_deadbeef",
            "account_name": "Enterprise team"
        });
        assert_eq!(log_account(&value), "Enterprise team (acc_deadbeef)");
        assert_eq!(log_account(&serde_json::json!({})), "-");
    }

    #[test]
    fn apikey_list_hides_usage_until_detail_is_requested() {
        let entries = serde_json::from_value::<Vec<ApiKeyListEntry>>(serde_json::json!([{
            "id":"ak_one",
            "name":"one",
            "enabled":true,
            "credits_limit":100.0,
            "reserved_credits":0.5,
            "usage":{
                "total_requests":3,
                "total_credits":1.25,
                "total_input_tokens":120,
                "total_output_tokens":30,
                "daily":{"2026-08-11":{"requests":3}},
                "history":[{"credits":1.25}]
            }
        }]))
        .expect("API key entries");
        let summary = ApiKeyListSummary::from_entries(&entries);

        let compact = apikey_list_json(&entries, &summary, false);
        assert_eq!(compact["summary"]["total"], 1);
        assert_eq!(compact["summary"]["total_credits"], 1.25);
        assert_eq!(compact["summary"]["total_input_tokens"], 120);
        assert!(compact["api_keys"][0].get("total_credits").is_none());
        assert!(compact["api_keys"][0].get("usage").is_none());

        let detail = apikey_list_json(&entries, &summary, true);
        assert_eq!(detail["summary"]["total_requests"], 3);
        assert_eq!(detail["summary"]["total_input_tokens"], 120);
        assert_eq!(detail["summary"]["total_output_tokens"], 30);
        assert_eq!(detail["summary"]["total_credits"], 1.25);
        assert_eq!(detail["api_keys"][0]["reserved_credits"], 0.5);
        assert!(detail["api_keys"][0].get("history").is_none());
    }

    #[test]
    fn log_model_route_distinguishes_mapping_from_automatic_resolution() {
        let automatic = serde_json::json!({
            "original_model": "claude-4.6-sonnet",
            "model": "claude-4.6-sonnet",
            "kiro_model": "claude-sonnet-4.6"
        });
        assert_eq!(
            log_model_route(&automatic),
            LogModelRoute {
                original: "claude-4.6-sonnet",
                routed: "claude-4.6-sonnet",
                resolved: "claude-sonnet-4.6",
                mapping_rule: None,
            }
        );
        let forced = serde_json::json!({
            "original_model": "claude-4.6-sonnet",
            "model": "claude-opus-4.6",
            "kiro_model": "claude-opus-4.6",
            "model_mapping_rule": "force-opus"
        });
        assert_eq!(log_model_route(&forced).mapping_rule, Some("force-opus"));
    }

    #[test]
    fn timestamps_accept_unix_and_timezone_aware_rfc3339() {
        assert_eq!(parse_timestamp("0").expect("unix epoch"), 0);
        assert_eq!(
            parse_timestamp("2026-08-27T10:00:00+08:00").expect("China time"),
            parse_timestamp("2026-08-27T02:00:00Z").expect("UTC time")
        );
        assert_eq!(
            parse_timestamp("2024-02-29T00:00:00Z").expect("leap day"),
            1_709_164_800
        );
    }

    #[test]
    fn timestamps_reject_ambiguous_or_invalid_values() {
        assert!(parse_timestamp("2026-08-27T10:00:00").is_err());
        assert!(parse_timestamp("2026-02-29T10:00:00Z").is_err());
        assert!(parse_timestamp("2026-08-27T25:00:00+08:00").is_err());
    }
}
