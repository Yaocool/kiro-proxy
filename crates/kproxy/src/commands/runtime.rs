//! Pool, diagnostics, statistics, API-key and webhook commands.

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use kproxy_core::paths::Paths;
use kproxy_ipc::protocol::method;
use kproxy_ipc::protocol::{
    ConfigPathResult, ConfigReloadResult, ConfigShowResult, ProxyServiceApiKeysResult,
    ProxyServiceCreateResult, ProxyServiceDeleteResult, ProxyServiceListResult,
};
use rand::RngCore;
use sha2::{Digest, Sha256};

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
    /// 删除并停止服务；关联 API key 保留。
    #[command(
        name = "delete",
        visible_alias = "rm",
        after_help = "示例：\n  kproxy service delete main --yes\n  kproxy service rm svc_abcd --yes"
    )]
    Delete {
        /// 服务 ID 或名称。
        service: String,
        /// 确认执行删除。
        #[arg(long)]
        yes: bool,
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
    #[command(after_help = "示例：\n  kproxy apikey list\n  kproxy --json apikey list")]
    List,
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
    #[command(
        after_help = "示例：\n  kproxy apikey rm ak_ab12 --yes\n  kproxy --json apikey rm ak_ab12 --yes"
    )]
    Rm {
        id: String,
        #[arg(long)]
        yes: bool,
    },
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
        after_help = "示例：\n  kproxy apikey reset-usage ak_ab12 --yes\n  kproxy --json apikey reset-usage ak_ab12 --yes"
    )]
    ResetUsage {
        id: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum WebhookCommand {
    #[command(after_help = "示例：\n  kproxy webhook list\n  kproxy --json webhook list")]
    List,
    #[command(after_help = "示例：\n  kproxy webhook test alerts\n  kproxy webhook test --all")]
    Test {
        name: Option<String>,
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    #[command(after_help = "示例：\n  kproxy webhook logs\n  kproxy webhook logs --tail 200")]
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

pub async fn run_webhook(
    client: &mut AdminClient,
    command: WebhookCommand,
    json: bool,
) -> Result<()> {
    match command {
        WebhookCommand::List => {
            simple_rpc(client, method::WEBHOOK_LIST, serde_json::json!({}), json).await
        }
        WebhookCommand::Test { name, all } => {
            if name.is_none() && !all {
                return Err(anyhow!("需指定 webhook 名称或 --all"));
            }
            simple_rpc(
                client,
                method::WEBHOOK_TEST,
                serde_json::json!({"name":name}),
                json,
            )
            .await
        }
        WebhookCommand::Logs { tail } => {
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

pub async fn run_apikey(
    client: &mut AdminClient,
    command: ApiKeyCommand,
    json: bool,
) -> Result<()> {
    match command {
        ApiKeyCommand::List => show_keys(client, None, None, json).await,
        ApiKeyCommand::Usage { id } => show_keys(client, Some(&id), None, json).await,
        ApiKeyCommand::History { id, tail } => show_keys(client, Some(&id), Some(tail), json).await,
        ApiKeyCommand::ResetUsage { id, yes } => {
            require_yes(yes)?;
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
            mutate_config(client, |array| {
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
        ApiKeyCommand::Rm { id, yes } => {
            require_yes(yes)?;
            mutate_config(client, |array| {
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
        ServiceCommand::Delete { service, yes } => {
            require_yes(yes)?;
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
                    "已停止并删除 API 代理服务 {} ({})。关联 API key 未删除。",
                    result.service_id, result.service_name
                );
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
    mutate_config(client, |array| {
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

async fn mutate_config(
    client: &mut AdminClient,
    mutate: impl FnOnce(&mut Vec<toml::Value>) -> Result<()>,
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
    let array = table
        .entry("api_key")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("api_key must be an array of tables"))?;
    mutate(array)?;
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

fn matches_key(value: &toml::Value, id: &str) -> bool {
    value.as_table().is_some_and(|table| {
        table.get("id").and_then(toml::Value::as_str) == Some(id)
            || table.get("name").and_then(toml::Value::as_str) == Some(id)
    })
}

fn require_yes(yes: bool) -> Result<()> {
    if yes {
        Ok(())
    } else {
        Err(anyhow!("破坏性操作需要 --yes"))
    }
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
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("$VISUAL/$EDITOR 不能为空"))?;
    let status = tokio::process::Command::new(program)
        .args(parts)
        .arg(&path)
        .status()
        .await
        .with_context(|| format!("启动编辑器 {program} 失败"))?;
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
    let config = effective_config(client).await?;
    match command {
        ModelMapCommand::List => {
            if json {
                print_json(&config.model_mapping)
            } else {
                for rule in config.model_mapping {
                    println!(
                        "[{:>3}] {:<24} {:<11} {} -> {}{}",
                        rule.priority,
                        rule.name,
                        rule.kind,
                        rule.source_models.join(","),
                        rule.target_models.join(","),
                        if rule.enabled { "" } else { " [disabled]" }
                    );
                }
                Ok(())
            }
        }
        ModelMapCommand::Test { model } => {
            let route = kproxy_translate::model::map_model(
                &model,
                &config.model_mapping,
                None,
                None,
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

pub fn print_topic(topic: &str) -> Result<()> {
    let text = match topic {
        "balance" => {
            "账号池评分 = active_ratio×weight_active + used_credit_ratio×weight_credit + recent_idle_penalty×weight_idle。\n分数越低越优先；随后加入小幅随机抖动，避免并发请求集中到同一账号。\n用 `kproxy pool --watch --explain` 查看实时评分明细。"
        }
        "model-map" => {
            "模型映射按 priority 从小到大匹配。source_models 支持一个 `*` 通配符；replace/alias 选首个目标，loadbalance 按 weights 随机。\n用 `kproxy model-map list` 查看规则，`kproxy model-map test <model>` 验证命中结果。"
        }
        "sso" => {
            r#"单账号：`printf '%s\n' "$PASSWORD" | kproxy account add-sso --email user@example.com --start-url https://example.awsapps.com/start --password-stdin`。
批量：CSV 仅含 email,password 两列，运行 `kproxy account add-sso --batch accounts.csv --start-url <url> -c 1`。
若需要验证码或自动化页面变化，增加 `--headful` 手工完成；slim 构建不含 SSO。"#
        }
        "config" => {
            "配置默认位于 $KPROXY_HOME/config.toml，修改后热重载；server.host/port、admin.socket 和 TLS 监听变更需要重启。\n`kproxy config validate [file]` 只校验，`kproxy config edit` 保存后校验并重载，`kproxy config show --effective` 查看合并默认值的结果。"
        }
        "apikey" => {
            "API key 限额采用在途预留：请求进入时预留估算 credits，结束后按上游实际用量结算，避免并发突破限额。\n统计包含总量、日维度、客户端模型、原模型、Kiro 模型和路径；`kproxy apikey usage <id>` 与 `history` 可查询。"
        }
        _ => {
            return Err(anyhow!(
                "未知帮助主题 {topic}；可用主题：balance, model-map, sso, config, apikey"
            ))
        }
    };
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
