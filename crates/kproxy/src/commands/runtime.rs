//! Pool, diagnostics, statistics, API-key and alert commands.

use anyhow::{anyhow, Context, Result};
use clap::{Subcommand, ValueEnum};
use kproxy_core::paths::Paths;
use kproxy_ipc::protocol::method;
use kproxy_ipc::protocol::{
    ConfigPathResult, ConfigReloadResult, ConfigShowResult, ProxyServiceApiKeysResult,
    ProxyServiceCreateResult, ProxyServiceDeleteResult, ProxyServiceListResult,
};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::client::AdminClient;
use crate::output::{format_timestamp, print_json, render_table};
use crate::ModelMapCommand;

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// 列出 API 代理服务。
    #[command(after_help = "示例：\n  kproxy service list\n  kproxy --json service list")]
    List,
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
    #[command(
        after_help = "示例：\n  kproxy apikey list\n  kproxy apikey list --detail\n  kproxy --json apikey list --detail"
    )]
    List {
        /// 展示每个 API key 的 token/credits 消耗明细。
        #[arg(long)]
        detail: bool,
    },
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
    #[command(after_help = "示例：\n  kproxy apikey rm ak_ab12\n\n执行前需输入 y 或 yes 确认。")]
    Rm { id: String },
    #[command(
        after_help = "示例：\n  kproxy apikey enable ak_ab12\n  kproxy --json apikey enable ak_ab12"
    )]
    Enable { id: String },
    #[command(
        after_help = "示例：\n  kproxy apikey disable ak_ab12\n  kproxy --json apikey disable ak_ab12"
    )]
    Disable { id: String },
    #[command(
        after_help = "示例：\n  kproxy apikey limit ak_ab12 --credits 100\n  kproxy apikey limit ak_ab12 --credits 0"
    )]
    Limit {
        id: String,
        #[arg(long)]
        credits: f64,
    },
    #[command(
        after_help = "示例：\n  kproxy apikey usage ak_ab12\n  kproxy --json apikey usage ak_ab12"
    )]
    Usage { id: String },
    #[command(
        after_help = "示例：\n  kproxy apikey history ak_ab12\n  kproxy apikey history ak_ab12 --tail 200"
    )]
    History {
        id: String,
        #[arg(long, default_value_t = 50)]
        tail: usize,
    },
    #[command(
        after_help = "示例：\n  kproxy apikey reset-usage ak_ab12\n\n执行前需输入 y 或 yes 确认。"
    )]
    ResetUsage { id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AlertEvent {
    /// 账号剩余额度首次跌破配置的递进告警档位。
    LowCredit,
    /// 认证错误后的 Token 刷新失败，账号被封禁。
    AccountBanned,
    /// 后台任务刷新即将过期的 Token 失败。
    TokenExpired,
    /// 账号或代理服务的可用额度耗尽。
    QuotaExhausted,
    /// 额度耗尽导致额度相关请求进入降级状态。
    ServiceDegraded,
}

impl AlertEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::LowCredit => "low-credit",
            Self::AccountBanned => "account-banned",
            Self::TokenExpired => "token-expired",
            Self::QuotaExhausted => "quota-exhausted",
            Self::ServiceDegraded => "service-degraded",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AlertCommand {
    /// 查看或修改告警策略。
    #[command(
        after_help = "示例：\n  kproxy alert config\n  kproxy alert config --low-credit-threshold-percent 15 --max-notifications 3 --suppress-window 20m"
    )]
    Config {
        /// 低额度告警的起始百分比，0 表示关闭低额度告警。
        #[arg(long, value_name = "PERCENT")]
        low_credit_threshold_percent: Option<f64>,
        /// 低额度递进告警的最大档位数，0 表示关闭低额度告警。
        #[arg(long, value_name = "COUNT")]
        max_notifications: Option<u32>,
        /// 非额度事件的重复告警抑制窗口，例如 30s、20m、1h。
        #[arg(long, value_name = "DURATION")]
        suppress_window: Option<String>,
    },
    /// 列出可订阅事件及其触发条件。
    #[command(after_help = "示例：\n  kproxy alert events\n  kproxy --json alert events")]
    Events,
    #[command(after_help = "示例：\n  kproxy alert list\n  kproxy --json alert list")]
    List,
    /// 添加告警目标。
    #[command(
        after_help = "先用 `kproxy alert events` 查看事件说明。\n多选可重复传入 --event，也可使用逗号分隔。\n\n示例：\n  kproxy alert add --name alerts --kind dingtalk --url https://example/hook --event token-expired --event quota-exhausted\n  kproxy alert add --name alerts --kind feishu --url https://example/hook --event low-credit,account-banned"
    )]
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        url: String,
        #[arg(
            long = "event",
            value_delimiter = ',',
            required = true,
            value_name = "EVENT"
        )]
        events: Vec<AlertEvent>,
        #[arg(long)]
        disabled: bool,
        #[arg(long)]
        dingtalk_sign: Option<String>,
        #[arg(long)]
        telegram_chat_id: Option<String>,
        #[arg(long)]
        custom_template: Option<String>,
    },
    /// 编辑告警目标。
    #[command(
        after_help = "先用 `kproxy alert events` 查看事件说明。\n多选可重复传入 --event，也可使用逗号分隔；新的事件列表会整体替换原订阅。\n\n示例：\n  kproxy alert edit alerts --url https://example/new-hook\n  kproxy alert edit alerts --event token-expired --event service-degraded\n  kproxy alert edit alerts --event low-credit,quota-exhausted"
    )]
    Edit {
        /// 当前名称。
        name: String,
        #[arg(long)]
        rename: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(
            long = "event",
            value_delimiter = ',',
            conflicts_with = "clear_events",
            value_name = "EVENT"
        )]
        events: Vec<AlertEvent>,
        #[arg(long)]
        clear_events: bool,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        #[arg(long, conflicts_with = "clear_dingtalk_sign")]
        dingtalk_sign: Option<String>,
        #[arg(long)]
        clear_dingtalk_sign: bool,
        #[arg(long, conflicts_with = "clear_telegram_chat_id")]
        telegram_chat_id: Option<String>,
        #[arg(long)]
        clear_telegram_chat_id: bool,
        #[arg(long, conflicts_with = "clear_custom_template")]
        custom_template: Option<String>,
        #[arg(long)]
        clear_custom_template: bool,
    },
    /// 删除告警目标，执行前需输入 y 或 yes 确认。
    #[command(name = "delete", visible_alias = "rm")]
    Delete { name: String },
    #[command(after_help = "示例：\n  kproxy alert test alerts\n  kproxy alert test --all")]
    Test {
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
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

pub async fn show_stats(
    client: &mut AdminClient,
    detail: bool,
    recent: Option<usize>,
    since_secs: Option<u64>,
    by: Option<&str>,
    json: bool,
) -> Result<()> {
    let effective_recent = detail.then_some(recent.unwrap_or(20));
    let value: serde_json::Value = client
        .call(
            method::STATS,
            serde_json::json!({
                "detail":detail,
                "recent":effective_recent,
                "since_secs":since_secs,
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
        AlertCommand::Config {
            low_credit_threshold_percent,
            max_notifications,
            suppress_window,
        } => {
            let suppress_window_ms = suppress_window
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .map(|seconds| {
                    seconds
                        .checked_mul(1_000)
                        .ok_or_else(|| anyhow!("告警抑制窗口过大"))
                })
                .transpose()?;
            if low_credit_threshold_percent.is_some()
                || max_notifications.is_some()
                || suppress_window_ms.is_some()
            {
                mutate_config_table(client, "notify", |table| {
                    if let Some(value) = low_credit_threshold_percent {
                        table.insert(
                            "low_credit_threshold_percent".into(),
                            toml::Value::Float(value),
                        );
                    }
                    if let Some(value) = max_notifications {
                        table.insert(
                            "max_notifications".into(),
                            toml::Value::Integer(i64::from(value)),
                        );
                    }
                    if let Some(value) = suppress_window_ms {
                        let value = i64::try_from(value)
                            .map_err(|_| anyhow!("告警抑制窗口超过 TOML 整数范围"))?;
                        table.insert("suppress_window_ms".into(), toml::Value::Integer(value));
                    }
                    Ok(())
                })
                .await?;
            }
            show_alert_config(client, json).await
        }
        AlertCommand::Events => show_alert_events(json),
        AlertCommand::List => {
            simple_rpc(client, method::WEBHOOK_LIST, serde_json::json!({}), json).await
        }
        AlertCommand::Add {
            name,
            kind,
            url,
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
                table.insert("kind".into(), toml::Value::String(kind.clone()));
                table.insert("url".into(), toml::Value::String(url.clone()));
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
            name,
            rename,
            kind,
            url,
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
            mutate_config_array(client, "webhook", |array| {
                let table = find_named_table_mut(array, &name, "告警目标")?;
                replace_optional_string(table, "name", rename.as_deref());
                replace_optional_string(table, "kind", kind.as_deref());
                replace_optional_string(table, "url", url.as_deref());
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

fn alert_event_catalog() -> [AlertEventInfo; 5] {
    [
        AlertEventInfo {
            event: AlertEvent::LowCredit.as_str(),
            condition: "定时额度检查发现启用账号的剩余额度首次跌破 [notify] 配置的某个递进档位。",
        },
        AlertEventInfo {
            event: AlertEvent::AccountBanned.as_str(),
            condition: "生成请求发生认证错误，强制刷新 Token 失败，或刷新后重试仍为认证错误，账号被封禁。",
        },
        AlertEventInfo {
            event: AlertEvent::TokenExpired.as_str(),
            condition: "后台 Token 扫描发现账号进入提前刷新窗口，并且自动刷新失败；不要求 Token 已完全过期。",
        },
        AlertEventInfo {
            event: AlertEvent::QuotaExhausted.as_str(),
            condition: "账号连续额度错误达到阈值，或代理日额度达到上限，或全部兼容账号都被判定额度耗尽。",
        },
        AlertEventInfo {
            event: AlertEvent::ServiceDegraded.as_str(),
            condition: "额度耗尽导致相关请求进入降级拒绝状态时发送；daemon、管理面和后台恢复任务保持运行。",
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

async fn show_alert_config(client: &mut AdminClient, json: bool) -> Result<()> {
    let notify = effective_config(client).await?.notify;
    if json {
        return print_json(&notify);
    }
    let suppress_seconds = notify.suppress_window_ms / 1_000;
    let suppress_seconds = i64::try_from(suppress_seconds).unwrap_or(i64::MAX);
    println!(
        "低额度起始阈值  {:.1}%",
        notify.low_credit_threshold_percent
    );
    println!("递进告警档位    {}", notify.max_notifications);
    println!(
        "重复告警抑制    {}（{} ms）",
        crate::output::format_relative(suppress_seconds),
        notify.suppress_window_ms
    );
    Ok(())
}

pub async fn run_apikey(
    client: &mut AdminClient,
    command: ApiKeyCommand,
    json: bool,
) -> Result<()> {
    match command {
        ApiKeyCommand::List { detail } => show_key_list(client, detail, json).await,
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
            .await
        }
        ApiKeyCommand::Enable { id } => {
            mutate_key_and_reload(client, &id, "enabled", toml::Value::Boolean(true)).await
        }
        ApiKeyCommand::Disable { id } => {
            mutate_key_and_reload(client, &id, "enabled", toml::Value::Boolean(false)).await
        }
        ApiKeyCommand::Limit { id, credits } => {
            mutate_key_and_reload(client, &id, "credits_limit", toml::Value::Float(credits)).await
        }
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
    format!("{value:.4}")
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
                                .map(|limit| limit.to_string())
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

async fn mutate_config_table(
    client: &mut AdminClient,
    section: &str,
    mutate: impl FnOnce(&mut toml::map::Map<String, toml::Value>) -> Result<()>,
) -> Result<()> {
    mutate_config(client, |table| {
        let nested = table
            .entry(section)
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow!("{section} must be a table"))?;
        mutate(nested)
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
    let mut root = raw.parse::<toml::Value>().context("配置文件 TOML 无效")?;
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    mutate(table)?;
    let output = toml::to_string_pretty(&root).context("序列化配置失败")?;
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
    let status = tokio::process::Command::new(&editor.program)
        .args(&editor.args)
        .arg(&path)
        .status()
        .await
        .with_context(|| {
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

pub async fn run_model_map(
    client: &mut AdminClient,
    command: ModelMapCommand,
    json: bool,
) -> Result<()> {
    match command {
        ModelMapCommand::List => {
            let config = effective_config(client).await?;
            if json {
                print_json(&config.model_mapping)
            } else {
                for rule in config.model_mapping {
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
            "`kproxy status` 展示 daemon 版本、代理服务、账号池、并发、统计和配置重载状态；`--watch` 每 2 秒刷新。"
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
            "`kproxy service create/list/apikeys/delete` 管理独立代理监听。创建时生成专用 API key；删除时级联删除专用 key，共享 key 保留，并要求 y/yes 确认。"
        }
        "config" => {
            "配置默认位于 $KPROXY_HOME/config.toml，修改后热重载；server.host/port、admin.socket 和 TLS 监听变更需要重启。\n`kproxy config validate [file]` 只校验，`kproxy config edit` 保存后校验并重载，`kproxy config show --effective` 查看合并默认值的结果。`kproxy config reset` 确认后备份原文件、恢复全部默认设置并重载。"
        }
        "apikey" => {
            "API key 限额采用在途预留：请求进入时预留估算 credits，结束后按上游实际用量结算，避免并发突破限额。\n`kproxy apikey list` 只显示基本汇总，增加 `--detail` 查看每个 key 的 token/credits 消耗；日维度、模型、路径和历史可用 `kproxy apikey usage <id>` 与 `history` 查询。"
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
            "`kproxy stats [--since 1h]` 默认只显示请求、成功率、tokens、credits 和延迟汇总。`--detail` 显示最近请求，并可用 `--by model|account|apikey|endpoint` 分组。"
        }
        "logs" => {
            "`kproxy logs` 按请求显示 trace ID、账号名称、模型路由和上游尝试；支持 `--level`、`--account`、`--tail` 和 `-f/--follow`。"
        }
        "alert" => {
            "`kproxy alert events` 列出全部事件和触发条件；`kproxy alert config` 查看或修改低额度阈值、递进档位和重复告警抑制窗口；`kproxy alert add/edit/delete/list/test/logs` 管理钉钉、飞书等告警目标。`--event` 可重复传入或使用逗号分隔实现多选。"
        }
        "models" => {
            "`kproxy models` 显示账号自动探测到的 Kiro 模型；`--mapped` 同时显示显式映射结果。自动别名解析与强制 model-map 是两层机制。"
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
}
