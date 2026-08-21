//! `kproxy account` 子命令。

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use kproxy_core::account::Account;
use kproxy_core::ids::{new_account_id, new_machine_id};
use kproxy_ipc::protocol::{
    method, AccountDetail, AccountImportResult, AccountListResult, AccountSummary, ConfigShowResult,
};

use crate::client::AdminClient;
use crate::output::{format_timestamp, print_json, render_table};

/// 账号相关子命令。
#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// 列出账号。
    #[command(
        long_about = "列出账号。\n\n示例：\n  kproxy account list\n  kproxy account list --tag prod --enabled-only"
    )]
    List {
        /// 只显示带该标签的账号。
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,
        /// 只显示已启用账号。
        #[arg(long)]
        enabled_only: bool,
        /// 状态过滤：available/disabled/exhausted。
        #[arg(long)]
        status: Option<String>,
        /// 排序字段：credit/email/id。
        #[arg(long)]
        sort: Option<String>,
    },
    /// 显示单账号详情。
    #[command(
        long_about = "显示账号详情，不显示 token。\n\n示例：\n  kproxy account show acc_7f3a\n  kproxy account show alice@example.com"
    )]
    Show {
        /// 账号 ID 或邮箱。
        id: String,
    },
    /// 从 JSON 导入现成 token。
    #[command(
        long_about = "导入已有凭证。id、machine_id、created_at 缺失时自动生成。\n\n示例：\n  kproxy account import --file accounts.json\n  cat accounts.json | kproxy account import --stdin"
    )]
    Import {
        /// JSON 文件路径。
        #[arg(long, value_name = "PATH", conflicts_with = "stdin")]
        file: Option<String>,
        /// 从标准输入读取。
        #[arg(long)]
        stdin: bool,
    },
    /// 导出账号 JSON；默认含凭证，仅应写入受保护位置。
    #[command(
        after_help = "示例：\n  kproxy --json account export > accounts.json\n  kproxy --json account export --redact"
    )]
    Export {
        /// 隐去 token 与 secret，适合诊断分享。
        #[arg(long)]
        redact: bool,
    },
    /// 通过 IAM Identity Center 登录并添加账号。
    #[command(
        after_help = "示例：\n  printf '%s\\n' \"$PASSWORD\" | kproxy account add-sso --email user@example.com --start-url https://example.awsapps.com/start --password-stdin\n  kproxy account add-sso --batch accounts.csv --start-url https://example.awsapps.com/start\n  kproxy account add-sso --batch - --start-url https://example.awsapps.com/start < accounts.csv"
    )]
    AddSso {
        #[arg(long)]
        email: Option<String>,
        /// IAM Identity Center start URL；未提供时读取 [sso].start_url。
        #[arg(long)]
        start_url: Option<String>,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 必须显式声明，从标准输入读取一行密码；密码不会进入命令行历史。
        #[arg(long)]
        password_stdin: bool,
        /// 两列 CSV（email,password）批量登录；PATH 为 - 时从 stdin 读取。
        #[arg(long, value_name = "PATH", conflicts_with_all = ["email", "password_stdin"])]
        batch: Option<String>,
        /// 批量登录并发数，范围 1..8。
        #[arg(short = 'c', long, default_value_t = 1)]
        concurrency: usize,
        /// 显示浏览器窗口，便于手工处理额外验证。
        #[arg(long)]
        headful: bool,
    },
    /// 从两列 CSV（email,password）批量执行 SSO 登录。
    #[command(hide = true)]
    AddSsoBatch {
        #[arg(long, value_name = "PATH")]
        file: String,
        #[arg(long)]
        start_url: Option<String>,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 同时启动的登录数；默认串行，降低提供商风控概率。
        #[arg(short = 'c', long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        headful: bool,
    },
    /// 删除账号。
    #[command(
        long_about = "删除账号，执行前需输入 y 或 yes 确认。\n\n示例：\n  kproxy account rm acc_7f3a2b1c\n  kproxy account rm user@example.com"
    )]
    Rm {
        /// 账号 ID 或邮箱。
        id: String,
    },
    /// 启用账号。
    #[command(
        after_help = "示例：\n  kproxy account enable acc_7f3a2b1c\n  kproxy account enable user@example.com"
    )]
    Enable {
        /// 账号 ID 或邮箱。
        id: String,
    },
    /// 停用账号。
    #[command(
        after_help = "示例：\n  kproxy account disable acc_7f3a2b1c\n  kproxy account disable user@example.com"
    )]
    Disable {
        /// 账号 ID 或邮箱。
        id: String,
    },
    /// 增删标签。
    #[command(
        long_about = "增删标签，可同时操作。\n\n示例：\n  kproxy account tag acc_7f3a --add prod --add pro\n  kproxy account tag acc_7f3a --rm dev"
    )]
    Tag {
        /// 账号 ID 或邮箱。
        id: String,
        /// 添加标签，可重复。
        #[arg(long = "add", value_name = "TAG")]
        add: Vec<String>,
        /// 移除标签，可重复。
        #[arg(long = "rm", value_name = "TAG")]
        remove: Vec<String>,
    },
    /// 重新生成设备标识。
    #[command(
        long_about = "重新生成 machine_id。仅在怀疑当前组合被标记时使用。\n\n示例：\n  kproxy account regen-machine-id acc_7f3a2b1c\n  kproxy account regen-machine-id user@example.com"
    )]
    RegenMachineId {
        /// 账号 ID 或邮箱。
        id: String,
    },
    /// 立即刷新账号 token。
    #[command(
        after_help = "示例：\n  kproxy account refresh acc_7f3a2b1c\n  kproxy account refresh --all"
    )]
    Refresh {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// 探测账号可用端点与模型。
    #[command(
        after_help = "示例：\n  kproxy account probe acc_7f3a2b1c\n  kproxy account probe --all"
    )]
    Probe {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// 清除冷却、封禁与额度耗尽标记。
    #[command(
        after_help = "示例：\n  kproxy account reset-health acc_7f3a2b1c\n  kproxy account reset-health --all"
    )]
    ResetHealth {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
}

/// 构造账号列表表格行。
pub fn build_list_rows(accounts: &[AccountSummary]) -> Vec<Vec<String>> {
    accounts
        .iter()
        .map(|account| {
            let status = display_health(account);
            let credit = match (account.credit_current, account.credit_limit) {
                (Some(current), Some(limit)) => {
                    format!("{}/{}", current.round() as i64, limit.round() as i64)
                }
                _ => "-".into(),
            };
            vec![
                account.id.clone(),
                account.email.clone(),
                status,
                credit,
                account.subscription.clone().unwrap_or_else(|| "-".into()),
                if account.tags.is_empty() {
                    "-".into()
                } else {
                    account.tags.join(",")
                },
            ]
        })
        .collect()
}

fn display_health(account: &AccountSummary) -> String {
    if !account.enabled {
        return "停用".into();
    }
    if account.credit_exhausted {
        return "额度耗尽".into();
    }
    match account.health.as_deref() {
        Some("available") => "启用",
        Some("cooling") => "冷却",
        Some("exhausted") => "额度耗尽",
        Some("banned") => "已封禁",
        Some("refreshing") => "刷新中",
        Some("disabled") => "停用",
        Some(other) => other,
        None => "启用",
    }
    .into()
}

/// 解析导入载荷，接受单对象或数组并补全本地字段。
pub fn parse_import_payload(raw: &str) -> Result<Vec<Account>> {
    let value: serde_json::Value = serde_json::from_str(raw).context("导入内容不是合法 JSON")?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        object @ serde_json::Value::Object(_) => vec![object],
        _ => return Err(anyhow!("导入内容应为 JSON 对象或对象数组")),
    };

    let now = now_secs();
    let mut accounts = Vec::with_capacity(items.len());
    for (index, mut item) in items.into_iter().enumerate() {
        let object = item
            .as_object_mut()
            .ok_or_else(|| anyhow!("第 {} 项不是 JSON 对象", index + 1))?;
        match object.get("email").and_then(serde_json::Value::as_str) {
            Some(email) if !email.trim().is_empty() => {}
            _ => return Err(anyhow!("第 {} 项缺少有效 email 字段", index + 1)),
        }
        object
            .entry("id")
            .or_insert_with(|| serde_json::Value::String(new_account_id()));
        object
            .entry("machine_id")
            .or_insert_with(|| serde_json::Value::String(new_machine_id()));
        object
            .entry("created_at")
            .or_insert_with(|| serde_json::Value::from(now));

        let account: Account = serde_json::from_value(item)
            .with_context(|| format!("第 {} 项字段不合法", index + 1))?;
        validate_imported_account(&account, index)?;
        accounts.push(account);
    }
    Ok(accounts)
}

fn validate_imported_account(account: &Account, index: usize) -> Result<()> {
    let id_hex = account.id.strip_prefix("acc_").unwrap_or_default();
    if id_hex.len() != 8
        || !id_hex
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(anyhow!("第 {} 项 id 格式无效", index + 1));
    }
    if account.machine_id.len() != 64
        || !account
            .machine_id
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(anyhow!("第 {} 项 machine_id 格式无效", index + 1));
    }
    if account.credentials.access_token.trim().is_empty() {
        return Err(anyhow!("第 {} 项 access_token 不能为空", index + 1));
    }
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// 执行账号子命令。
pub async fn run(client: &mut AdminClient, command: AccountCommand, json: bool) -> Result<()> {
    match command {
        AccountCommand::List {
            tag,
            enabled_only,
            status,
            sort,
        } => {
            let list: AccountListResult = client
                .call(
                    method::ACCOUNT_LIST,
                    serde_json::json!({
                        "tag": tag,
                        "enabled_only": enabled_only.then_some(true),
                        "status":status,
                        "sort":sort,
                    }),
                )
                .await?;
            if json {
                print_json(&list)?;
            } else if list.accounts.is_empty() {
                println!("暂无账号。用 `kproxy account import` 添加。");
            } else {
                print!(
                    "{}",
                    render_table(
                        &["ID", "邮箱", "状态", "额度", "订阅", "标签"],
                        &build_list_rows(&list.accounts),
                    )
                );
            }
        }
        AccountCommand::Show { id } => {
            let detail: AccountDetail = client
                .call(method::ACCOUNT_SHOW, serde_json::json!({"id": id}))
                .await?;
            if json {
                print_json(&detail)?;
            } else {
                print_detail(&detail);
            }
        }
        AccountCommand::Import { file, stdin } => {
            let raw = read_import_source(file.as_deref(), stdin).await?;
            let accounts = parse_import_payload(&raw)?;
            let result: AccountImportResult = client
                .call(
                    method::ACCOUNT_IMPORT,
                    serde_json::json!({"accounts": accounts}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!("已导入 {} 个账号", result.imported);
                if !result.skipped.is_empty() {
                    println!(
                        "跳过 {} 个（ID 或邮箱已存在）：{}",
                        result.skipped.len(),
                        result.skipped.join(", ")
                    );
                }
            }
        }
        AccountCommand::Export { redact } => {
            let accounts: serde_json::Value = client
                .call(method::ACCOUNT_EXPORT, serde_json::json!({"redact":redact}))
                .await?;
            print_json(&accounts)?;
        }
        AccountCommand::AddSso {
            email,
            start_url,
            region,
            password_stdin,
            batch,
            concurrency,
            headful,
        } => {
            let start_url = resolve_start_url(client, start_url.as_deref()).await?;
            if let Some(file) = batch {
                run_sso_batch(
                    client,
                    &file,
                    &start_url,
                    &region,
                    concurrency,
                    headful,
                    json,
                )
                .await?;
                return Ok(());
            }
            if !password_stdin {
                return Err(anyhow!("SSO 密码只能通过 --password-stdin 提供"));
            }
            let email = email.ok_or_else(|| anyhow!("单账号登录需提供 --email"))?;
            let password = read_password_line().await?;
            let result: AccountSummary = client
                .call(
                    method::ACCOUNT_ADD_SSO,
                    serde_json::json!({
                        "email":email,
                        "password":password,
                        "start_url":start_url,
                        "region":region,
                        "headful":headful
                    }),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!("已添加 {}（{}）", result.email, result.id);
            }
        }
        AccountCommand::AddSsoBatch {
            file,
            start_url,
            region,
            concurrency,
            headful,
        } => {
            let start_url = resolve_start_url(client, start_url.as_deref()).await?;
            run_sso_batch(
                client,
                &file,
                &start_url,
                &region,
                concurrency,
                headful,
                json,
            )
            .await?;
        }
        AccountCommand::Rm { id } => {
            if !crate::commands::confirm(&format!("确认删除账号 {id}？")).await? {
                println!("已取消");
                return Ok(());
            }
            let result: serde_json::Value = client
                .call(method::ACCOUNT_REMOVE, serde_json::json!({"id": id}))
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!("已删除 {id}");
            }
        }
        AccountCommand::Enable { id } => set_enabled(client, &id, true, json).await?,
        AccountCommand::Disable { id } => set_enabled(client, &id, false, json).await?,
        AccountCommand::Tag { id, add, remove } => {
            let result: serde_json::Value = client
                .call(
                    method::ACCOUNT_TAG,
                    serde_json::json!({"id": id, "add": add, "remove": remove}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                let tags = result["tags"]
                    .as_array()
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                println!(
                    "{id} 当前标签：{}",
                    if tags.is_empty() { "-" } else { &tags }
                );
            }
        }
        AccountCommand::RegenMachineId { id } => {
            let result: serde_json::Value = client
                .call(
                    method::ACCOUNT_REGEN_MACHINE_ID,
                    serde_json::json!({"id": id}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "{id} 新设备标识：{}",
                    result["machine_id"].as_str().unwrap_or("-")
                );
            }
        }
        AccountCommand::Refresh { id, all } => {
            require_id_or_all(id.as_deref(), all)?;
            let result: serde_json::Value = client
                .call(
                    method::ACCOUNT_REFRESH,
                    serde_json::json!({"id":id,"all":all}),
                )
                .await?;
            print_result(&result, json, "账号 token 已刷新")?;
        }
        AccountCommand::Probe { id, all } => {
            require_id_or_all(id.as_deref(), all)?;
            let result: serde_json::Value = client
                .call(
                    method::ACCOUNT_PROBE,
                    serde_json::json!({"id":id,"all":all}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "账号 {} 探测成功，可用模型 {} 个",
                    id.as_deref().unwrap_or("all"),
                    result["models"].as_array().map_or(0, Vec::len)
                );
            }
        }
        AccountCommand::ResetHealth { id, all } => {
            require_id_or_all(id.as_deref(), all)?;
            let result: serde_json::Value = client
                .call(
                    method::ACCOUNT_RESET_HEALTH,
                    serde_json::json!({"id":id,"all":all}),
                )
                .await?;
            print_result(&result, json, "账号健康状态已重置")?;
        }
    }
    Ok(())
}

async fn resolve_start_url(client: &mut AdminClient, explicit: Option<&str>) -> Result<String> {
    if let Some(url) = explicit.map(str::trim).filter(|url| !url.is_empty()) {
        return validate_start_url(url);
    }
    let show: ConfigShowResult = client
        .call(method::CONFIG_SHOW, serde_json::json!({}))
        .await?;
    let config: kproxy_core::config::Config =
        serde_json::from_value(show.effective_json).context("daemon 返回的生效配置无效")?;
    let configured = config.sso.start_url.trim();
    if configured.is_empty() {
        return Err(anyhow!(
            "需提供 --start-url，或在配置文件中设置 [sso].start_url"
        ));
    }
    validate_start_url(configured)
}

fn validate_start_url(url: &str) -> Result<String> {
    if !url.starts_with("https://") {
        return Err(anyhow!("SSO start URL 必须使用 https://"));
    }
    Ok(url.to_owned())
}

fn require_id_or_all(id: Option<&str>, all: bool) -> Result<()> {
    if id.is_some() || all {
        Ok(())
    } else {
        Err(anyhow!("需指定账号 ID 或 --all"))
    }
}

fn print_result(value: &serde_json::Value, json: bool, message: &str) -> Result<()> {
    if json {
        print_json(value)?;
    } else {
        println!("{message}");
    }
    Ok(())
}

async fn set_enabled(client: &mut AdminClient, id: &str, enabled: bool, json: bool) -> Result<()> {
    let result: serde_json::Value = client
        .call(
            method::ACCOUNT_SET_ENABLED,
            serde_json::json!({"id": id, "enabled": enabled}),
        )
        .await?;
    if json {
        print_json(&result)?;
    } else {
        println!("{id} 已{}", if enabled { "启用" } else { "停用" });
    }
    Ok(())
}

async fn read_import_source(file: Option<&str>, stdin: bool) -> Result<String> {
    if stdin {
        use tokio::io::AsyncReadExt;
        let mut buffer = String::new();
        tokio::io::stdin()
            .read_to_string(&mut buffer)
            .await
            .context("读取标准输入失败")?;
        return Ok(buffer);
    }
    let path = file.ok_or_else(|| anyhow!("需指定 --file <PATH> 或 --stdin"))?;
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("读取 {path} 失败"))
}

async fn read_password_line() -> Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut password = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut password)
        .await
        .context("读取标准输入密码失败")?;
    while matches!(password.chars().last(), Some('\n' | '\r')) {
        password.pop();
    }
    if password.is_empty() {
        return Err(anyhow!("密码不能为空"));
    }
    Ok(password)
}

async fn run_sso_batch(
    client: &AdminClient,
    file: &str,
    start_url: &str,
    region: &str,
    concurrency: usize,
    headful: bool,
    json: bool,
) -> Result<()> {
    use futures::{stream, StreamExt};

    if !(1..=8).contains(&concurrency) {
        return Err(anyhow!("并发数必须在 1..=8 之间"));
    }
    let raw = read_sso_batch_source(file).await?;
    let rows = parse_sso_csv(&raw)?;
    let socket = client.socket_path();
    let start_url = start_url.to_string();
    let region = region.to_string();
    let results = stream::iter(rows.into_iter().map(|(email, password)| {
        let socket = socket.clone();
        let start_url = start_url.clone();
        let region = region.clone();
        async move {
            let mut client = AdminClient::connect(socket);
            let result = client
                .call::<AccountSummary>(
                    method::ACCOUNT_ADD_SSO,
                    serde_json::json!({
                        "email":email,
                        "password":password,
                        "start_url":start_url,
                        "region":region,
                        "headful":headful
                    }),
                )
                .await;
            (email, result)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;
    let mut failures = Vec::new();
    for (email, result) in results {
        match result {
            Ok(summary) if json => print_json(&summary)?,
            Ok(summary) => println!("已添加 {}（{}）", summary.email, summary.id),
            Err(error) => failures.push(format!("{email}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 个账号登录失败：\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

const WRAPPER_BATCH_STDIN_ENV: &str = "KPROXY_WRAPPER_BATCH_STDIN";

fn sso_batch_reads_stdin(file: &str, wrapper_override: Option<&std::ffi::OsStr>) -> bool {
    file == "-" || wrapper_override == Some(std::ffi::OsStr::new("1"))
}

async fn read_sso_batch_source(file: &str) -> Result<String> {
    if sso_batch_reads_stdin(file, std::env::var_os(WRAPPER_BATCH_STDIN_ENV).as_deref()) {
        use tokio::io::AsyncReadExt;

        let mut raw = String::new();
        tokio::io::stdin()
            .read_to_string(&mut raw)
            .await
            .context("读取批量 CSV 标准输入失败")?;
        return Ok(raw);
    }
    tokio::fs::read_to_string(file)
        .await
        .with_context(|| format!("读取 {file} 失败"))
}

fn parse_sso_csv(raw: &str) -> Result<Vec<(String, String)>> {
    let mut rows = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields =
            parse_csv_line(line).with_context(|| format!("CSV 第 {} 行无效", index + 1))?;
        if index == 0
            && fields
                .first()
                .map(|field| field.eq_ignore_ascii_case("email"))
                == Some(true)
        {
            continue;
        }
        if fields.len() != 2 || fields[0].trim().is_empty() || fields[1].is_empty() {
            return Err(anyhow!(
                "CSV 第 {} 行必须只有 email,password 两列",
                index + 1
            ));
        }
        rows.push((fields[0].trim().to_ascii_lowercase(), fields[1].clone()));
    }
    if rows.is_empty() {
        return Err(anyhow!("CSV 中没有账号"));
    }
    Ok(rows)
}

fn parse_csv_line(line: &str) -> Result<Vec<String>> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                fields
                    .last_mut()
                    .ok_or_else(|| anyhow!("CSV 解析状态无效"))?
                    .push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(String::new()),
            other => fields
                .last_mut()
                .ok_or_else(|| anyhow!("CSV 解析状态无效"))?
                .push(other),
        }
    }
    if quoted {
        return Err(anyhow!("未闭合的引号"));
    }
    Ok(fields)
}

fn print_detail(detail: &AccountDetail) {
    let summary = &detail.summary;
    let tags = if summary.tags.is_empty() {
        "-".to_string()
    } else {
        summary.tags.join(", ")
    };
    println!("{}   {}   [{}]", summary.id, summary.email, tags);
    if let Some(label) = &summary.label {
        println!("备注      {label}");
    }
    let status = display_health(summary);
    println!("状态      {status}");
    println!(
        "订阅      {}",
        summary.subscription.clone().unwrap_or_else(|| "-".into())
    );
    match (summary.credit_current, summary.credit_limit) {
        (Some(current), Some(limit)) if limit > 0.0 => println!(
            "额度      {} / {}（{:.0}% 已用）",
            current.round() as i64,
            limit.round() as i64,
            current / limit * 100.0
        ),
        _ => println!("额度      -（尚未拉取）"),
    }
    println!(
        "凭证      {} 过期",
        format_timestamp(summary.token_expires_at)
    );
    println!("区域      {}", detail.region);
    println!("认证      {}", detail.auth_method);
    println!(
        "模型      {}",
        if detail.supported_models.is_empty() {
            "-".into()
        } else {
            detail.supported_models.join(", ")
        }
    );
    println!(
        "端点      {}",
        detail.preferred_endpoint.as_deref().unwrap_or("尚未缓存")
    );
    println!(
        "并发      {} 进行中 / 上限 {}",
        detail.active_requests, detail.max_concurrent_requests
    );
    println!(
        "近期错误  {}",
        if detail.recent_errors.is_empty() {
            "无".into()
        } else {
            detail.recent_errors.join(" | ")
        }
    );
    println!("machineId {}", detail.machine_id);
    println!("创建      {}", format_timestamp(detail.created_at));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(enabled: bool) -> AccountSummary {
        AccountSummary {
            id: "acc_00000001".into(),
            email: "a@example.com".into(),
            label: None,
            enabled,
            health: Some(if enabled { "available" } else { "disabled" }.into()),
            tags: vec!["prod".into()],
            subscription: Some("Pro".into()),
            credit_current: Some(120.0),
            credit_limit: Some(500.0),
            token_expires_at: 1_767_225_600,
            credit_exhausted: false,
        }
    }

    #[test]
    fn list_rows_cover_status_usage_and_tags() {
        let mut enabled = summary(true);
        enabled.tags.push("pro".into());
        let disabled = summary(false);
        let rows = build_list_rows(&[enabled, disabled]);
        assert_eq!(rows[0][2], "启用");
        assert_eq!(rows[1][2], "停用");
        assert_eq!(rows[0][3], "120/500");
        assert_eq!(rows[0][5], "prod,pro");

        let mut exhausted = summary(true);
        exhausted.credit_exhausted = true;
        assert_eq!(build_list_rows(&[exhausted])[0][2], "额度耗尽");
    }

    #[test]
    fn import_accepts_object_and_array_and_fills_ids() {
        let object = r#"{
            "email": "a@example.com",
            "credentials": {
                "access_token": "at",
                "region": "us-east-1",
                "expires_at": 0,
                "auth_method": "idc"
            }
        }"#;
        let account = parse_import_payload(object).expect("object");
        assert_eq!(account.len(), 1);
        assert!(account[0].id.starts_with("acc_"));
        assert_eq!(account[0].machine_id.len(), 64);
        assert!(account[0].created_at > 0);
        let array = format!("[{object}]");
        assert_eq!(parse_import_payload(&array).expect("array").len(), 1);
    }

    #[test]
    fn import_rejects_missing_email_and_malformed_ids() {
        assert!(parse_import_payload(
            r#"{"credentials":{"access_token":"at","region":"us-east-1","expires_at":0,"auth_method":"idc"}}"#
        )
        .is_err());
        assert!(parse_import_payload("{not json").is_err());
        assert!(parse_import_payload(
            r#"{"id":"bad","email":"a@example.com","credentials":{"access_token":"at","region":"us-east-1","expires_at":0,"auth_method":"idc"}}"#
        )
        .is_err());
    }

    #[test]
    fn sso_csv_supports_headers_quotes_and_commas() {
        let rows =
            parse_sso_csv("email,password\na@example.com,secret\n\"b@example.com\",\"p,a\"\n")
                .expect("csv");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], ("b@example.com".into(), "p,a".into()));
        assert!(parse_sso_csv("a@example.com,\"unterminated").is_err());
    }

    #[test]
    fn sso_batch_accepts_explicit_and_wrapper_managed_stdin() {
        assert!(sso_batch_reads_stdin("-", None));
        assert!(sso_batch_reads_stdin(
            "accounts.csv",
            Some(std::ffi::OsStr::new("1"))
        ));
        assert!(!sso_batch_reads_stdin(
            "accounts.csv",
            Some(std::ffi::OsStr::new("0"))
        ));
        assert!(!sso_batch_reads_stdin("accounts.csv", None));
    }
}
