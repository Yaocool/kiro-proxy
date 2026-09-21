//! `kproxy account` 子命令。

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use kproxy_core::account::Account;
use kproxy_core::ids::{new_account_id, new_machine_id};
use kproxy_ipc::protocol::{
    method, AccountImportResult, AccountServiceBinding, AccountServicesResult, AccountSummary,
    AccountTagBatchResult, AccountTagResult, ConfigShowResult, ProviderAccountListResult,
    ProviderAccountSummary,
};

use crate::client::AdminClient;
use crate::output::{print_json, render_table};

/// 账号相关子命令。
#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// 列出账号。
    #[command(
        long_about = "列出账号，默认按邮箱排序。\n\n示例：\n  kproxy account list\n  kproxy account list --tag prod --enabled-only\n  kproxy account list --status low_credit\n  kproxy account list --sort credit"
    )]
    List {
        /// 提供源实例 ID；默认聚合全部提供源。
        #[arg(long, default_value = "all")]
        provider: String,
        /// 按提供源驱动类型筛选。
        #[arg(long)]
        provider_kind: Option<String>,
        /// 只显示带该标签的账号。
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,
        /// 只显示已启用账号。
        #[arg(long)]
        enabled_only: bool,
        /// 状态过滤：available/low_credit/disabled/exhausted/cooling/banned/refreshing/unavailable。
        #[arg(
            long,
            value_parser = [
                "available",
                "low_credit",
                "disabled",
                "exhausted",
                "cooling",
                "banned",
                "refreshing",
                "unavailable"
            ]
        )]
        status: Option<String>,
        /// 排序字段：email（默认）/credit/id。
        #[arg(long, value_parser = ["email", "credit", "id"])]
        sort: Option<String>,
    },
    /// 显示单账号详情。
    #[command(
        long_about = "显示账号详情，不显示 token。\n\n示例：\n  kproxy account show acc_7f3a\n  kproxy account show alice@example.com"
    )]
    Show {
        /// 账号 ID 或邮箱。
        id: String,
        /// 提供源实例 ID；也可直接使用 provider/account_id。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 通过提供源统一入口添加账号。
    #[command(
        after_help = "示例：\n  kproxy account add --provider copilot --auth device-flow\n  printf '%s\\n' \"$GITHUB_TOKEN\" | kproxy account add --provider copilot --token-stdin"
    )]
    Add {
        #[arg(long)]
        provider: String,
        /// 认证方式；Copilot 当前支持 device-flow。
        #[arg(long)]
        auth: Option<String>,
        /// 从标准输入读取 GitHub token 并显式导入。
        #[arg(long, conflicts_with = "auth")]
        token_stdin: bool,
        #[arg(long)]
        label: Option<String>,
    },
    /// 查看账号绑定的 API 代理服务。
    #[command(
        long_about = "查看账号当前属于哪些 API 代理服务，并显示全局池、标签或手工绑定来源。\n\n示例：\n  kproxy account services acc_7f3a2b1c\n  kproxy account services alice@example.com"
    )]
    Services {
        /// 账号 ID 或邮箱。
        id: String,
    },
    /// 从 JSON 导入现成 token。
    #[command(
        long_about = "导入已有凭证。id、machine_id、created_at 缺失时自动生成；--tag 会合并到本次导入的全部账号。\n\n示例：\n  kproxy account import --file accounts.json --tag team-a\n  cat accounts.json | kproxy account import --stdin --tag team-a --tag prod"
    )]
    Import {
        /// JSON 文件路径。
        #[arg(long, value_name = "PATH", conflicts_with = "stdin")]
        file: Option<String>,
        /// 从标准输入读取。
        #[arg(long)]
        stdin: bool,
        /// 合并到全部导入账号的标签；可重复或逗号分隔。
        #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
        tags: Vec<String>,
    },
    /// 导入 Kiro headless API key，从 KIRO_API_KEY 环境变量或标准输入读取。
    AddApiKey {
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 从标准输入读取 key，避免在命令行参数中暴露凭据。
        #[arg(long)]
        key_stdin: bool,
        /// 新账号标签；可重复或逗号分隔。
        #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
        tags: Vec<String>,
    },
    /// 导出账号 JSON；默认含凭证，仅应写入受保护位置。
    #[command(
        after_help = "示例：\n  kproxy --json account export > accounts.json\n  kproxy --json account export --redact"
    )]
    Export {
        /// 隐去 token 与 secret，适合诊断分享。
        #[arg(long)]
        redact: bool,
        /// 提供源实例 ID，使用 all 导出全部；省略时保持旧版 Kiro 输出格式。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 通过 IAM Identity Center 登录并添加账号。
    #[command(
        after_help = "示例：\n  printf '%s\\n' \"$PASSWORD\" | kproxy account add-sso --email user@example.com --start-url https://example.awsapps.com/start --password-stdin\n  kproxy account add-sso --batch accounts.csv --start-url https://example.awsapps.com/start\n  kproxy account add-sso --batch - --start-url https://example.awsapps.com/start < accounts.csv"
    )]
    AddSso {
        #[arg(long)]
        email: Option<String>,
        /// IAM Identity Center start URL；未提供时读取 `[sso].start_url`。
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
        /// 新账号标签；批量模式下应用到本批全部账号，可重复或逗号分隔。
        #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
        tags: Vec<String>,
    },
    /// 删除一个或多个账号。
    #[command(
        visible_alias = "delete",
        long_about = "删除一个或多个账号，整批执行前只需输入一次 y 或 yes 确认。\n\n示例：\n  kproxy account rm acc_7f3a2b1c\n  kproxy account rm user@example.com another@example.com"
    )]
    Rm {
        /// 一个或多个账号 ID/邮箱，以空格分隔。
        #[arg(required = true, num_args = 1.., value_name = "ID_OR_EMAIL")]
        ids: Vec<String>,
        /// 提供源实例 ID；也可使用 provider/account_id。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 启用账号。
    #[command(
        after_help = "示例：\n  kproxy account enable acc_7f3a2b1c\n  kproxy account enable user@example.com"
    )]
    Enable {
        /// 账号 ID 或邮箱。
        id: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// 停用账号。
    #[command(
        after_help = "示例：\n  kproxy account disable acc_7f3a2b1c\n  kproxy account disable user@example.com"
    )]
    Disable {
        /// 账号 ID 或邮箱。
        id: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// 为一个或多个账号增删标签。
    #[command(
        long_about = "为一个或多个账号增删标签，可同时添加和移除。批量修改会先校验全部账号；有账号不存在，或需要修改标签的账号已绑定代理服务时，整批不生效。\n\n示例：\n  kproxy account tag acc_7f3a --add prod --add pro\n  kproxy account tag --add test acc_2c36cfad acc_332c7cb2 acc_41c6e3ad\n  kproxy account tag --rm dev acc_7f3a acc_8b2c"
    )]
    Tag {
        /// 一个或多个账号 ID/邮箱，以空格分隔。
        #[arg(required = true, num_args = 1.., value_name = "ID_OR_EMAIL")]
        ids: Vec<String>,
        /// 添加标签，可重复。
        #[arg(long = "add", value_name = "TAG")]
        add: Vec<String>,
        /// 移除标签，可重复。
        #[arg(long = "rm", value_name = "TAG")]
        remove: Vec<String>,
        /// 提供源实例 ID；也可使用 provider/account_id。
        #[arg(long)]
        provider: Option<String>,
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
        /// 限定提供源；批量修改时必须显式提供或使用 all。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 探测账号可用端点与模型。
    #[command(
        after_help = "示例：\n  kproxy account probe acc_7f3a2b1c\n  kproxy account probe --all"
    )]
    Probe {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// 限定提供源；批量操作可显式使用 all。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 清除冷却、封禁与额度耗尽标记。
    #[command(
        after_help = "示例：\n  kproxy account reset-health acc_7f3a2b1c\n  kproxy account reset-health --all"
    )]
    ResetHealth {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// 限定提供源；批量操作可显式使用 all。
        #[arg(long)]
        provider: Option<String>,
    },
}

/// 构造账号列表表格行。
#[cfg(test)]
pub fn build_list_rows(accounts: &[AccountSummary]) -> Vec<Vec<String>> {
    accounts
        .iter()
        .map(|account| {
            let status = display_health(account);
            let credit = match (account.credit_current, account.credit_limit) {
                (Some(current), Some(limit)) => format!("{current:.2}/{limit:.2}"),
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

fn build_provider_list_rows(accounts: &[ProviderAccountSummary]) -> Vec<Vec<String>> {
    accounts
        .iter()
        .map(|account| {
            let quota = match (account.quota_current, account.quota_limit) {
                (Some(current), Some(limit)) => format!("{current:.2}/{limit:.2}"),
                (Some(current), None) => format!("{current:.2}"),
                _ => "unknown".into(),
            };
            vec![
                account.provider_id.clone(),
                account.id.clone(),
                account.display_name.clone(),
                if account.enabled {
                    account.health.clone()
                } else {
                    "disabled".into()
                },
                quota,
                account
                    .quota_unit
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                if account.tags.is_empty() {
                    "-".into()
                } else {
                    account.tags.join(",")
                },
            ]
        })
        .collect()
}

fn print_provider_detail(account: &ProviderAccountSummary) {
    println!(
        "{}/{}   {}",
        account.provider_id, account.id, account.display_name
    );
    println!("类型      {}", account.provider_kind);
    println!("状态      {}", account.health);
    println!("认证      {}", account.auth_state);
    if let Some(email) = &account.email {
        println!("邮箱      {email}");
    }
    if let Some(label) = &account.label {
        println!("备注      {label}");
    }
    match (account.quota_current, account.quota_limit) {
        (Some(current), Some(limit)) => println!(
            "额度      {current:.2}/{limit:.2} {}",
            account.quota_unit.as_deref().unwrap_or("unknown")
        ),
        _ => println!("额度      unknown"),
    }
    println!(
        "模型      {}",
        if account.supported_models.is_empty() {
            "unknown".into()
        } else {
            account.supported_models.join(", ")
        }
    );
    println!(
        "标签      {}",
        if account.tags.is_empty() {
            "-".into()
        } else {
            account.tags.join(", ")
        }
    );
    if account.details != serde_json::Value::Null {
        println!("详情      {}", account.details);
    }
}

async fn run_provider_login(
    client: &mut AdminClient,
    provider: &str,
    auth: &str,
    label: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut state: serde_json::Value = client
        .call(
            method::V2_LOGIN_START,
            serde_json::json!({"provider":provider,"auth":auth,"label":label}),
        )
        .await?;
    let task_id = state["id"]
        .as_str()
        .ok_or_else(|| anyhow!("daemon 返回的登录任务缺少 id"))?
        .to_owned();
    let verification_uri = state["verification_uri"].as_str().unwrap_or("-");
    let user_code = state["user_code"].as_str().unwrap_or("-");
    if json {
        eprintln!("请打开 {verification_uri} 并输入代码 {user_code}");
    } else {
        println!("请打开 {verification_uri}");
        println!("输入代码 {user_code}");
        println!("等待 GitHub 授权……");
    }
    loop {
        match state["status"].as_str().unwrap_or("failed") {
            "authorized" => {
                if json {
                    print_json(&state)?;
                } else {
                    println!(
                        "授权完成，已添加 {}/{}",
                        provider,
                        state["account_id"].as_str().unwrap_or("-")
                    );
                }
                return Ok(());
            }
            "pending" => {}
            status => {
                return Err(anyhow!(
                    "登录任务 {status}：{}",
                    state["error"].as_str().unwrap_or("未知错误")
                ));
            }
        }
        let interval = state["interval_secs"].as_u64().unwrap_or(5).clamp(1, 60);
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                let _: serde_json::Value = client.call(
                    method::V2_LOGIN_CANCEL,
                    serde_json::json!({"provider":provider,"task_id":task_id}),
                ).await?;
                return Err(anyhow!("登录已取消"));
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
        }
        state = client
            .call(
                method::V2_LOGIN_STATUS,
                serde_json::json!({"provider":provider,"task_id":task_id}),
            )
            .await?;
    }
}

#[cfg(test)]
fn display_health(account: &AccountSummary) -> String {
    if !account.enabled {
        return "停用".into();
    }
    if account.credit_exhausted {
        return "额度耗尽".into();
    }
    match account.health.as_deref() {
        Some("available") => "启用",
        Some("low_credit") => "低额度保护",
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
            provider,
            provider_kind,
            tag,
            enabled_only,
            status,
            sort,
        } => {
            let list: ProviderAccountListResult = client
                .call(
                    method::V2_ACCOUNT_LIST,
                    serde_json::json!({
                        "provider":provider,
                        "provider_kind":provider_kind,
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
                println!("暂无账号。用 `kproxy account add --provider <ID>` 添加。");
            } else {
                print!(
                    "{}",
                    render_table(
                        &["PROVIDER", "ID", "账号", "状态", "额度", "单位", "标签"],
                        &build_provider_list_rows(&list.accounts),
                    )
                );
                for (provider, error) in &list.errors {
                    eprintln!("{provider}: {error}");
                }
            }
            if !list.complete {
                return Err(anyhow!("部分提供源的账号查询失败"));
            }
        }
        AccountCommand::Show { id, provider } => {
            let detail: ProviderAccountSummary = client
                .call(
                    method::V2_ACCOUNT_SHOW,
                    serde_json::json!({"id": id,"provider":provider}),
                )
                .await?;
            if json {
                print_json(&detail)?;
            } else {
                print_provider_detail(&detail);
            }
        }
        AccountCommand::Add {
            provider,
            auth,
            token_stdin,
            label,
        } => {
            if token_stdin {
                let token = read_import_source(None, true).await?;
                let token = token.trim();
                if token.is_empty() || token.chars().any(char::is_whitespace) {
                    return Err(anyhow!("GitHub token 不能为空或包含空白字符"));
                }
                let result: serde_json::Value = client
                    .call(
                        method::V2_ACCOUNT_IMPORT_TOKEN,
                        serde_json::json!({"provider":provider,"token":token,"label":label}),
                    )
                    .await?;
                if json {
                    print_json(&result)?;
                } else {
                    println!(
                        "已添加 {}/{}（{}）",
                        result["provider_id"].as_str().unwrap_or("-"),
                        result["id"].as_str().unwrap_or("-"),
                        result["login"].as_str().unwrap_or("-")
                    );
                }
            } else {
                run_provider_login(
                    client,
                    &provider,
                    auth.as_deref().unwrap_or("device-flow"),
                    label.as_deref(),
                    json,
                )
                .await?;
            }
        }
        AccountCommand::Services { id } => {
            let result: AccountServicesResult = client
                .call(method::ACCOUNT_SERVICES, serde_json::json!({"id":id}))
                .await?;
            print_account_services(&result, json)?;
        }
        AccountCommand::Import { file, stdin, tags } => {
            let tags = normalize_cli_tags(tags)?;
            let raw = read_import_source(file.as_deref(), stdin).await?;
            let accounts = parse_import_payload(&raw)?;
            let result: AccountImportResult = client
                .call(
                    method::ACCOUNT_IMPORT,
                    serde_json::json!({"accounts": accounts, "tags":tags}),
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
        AccountCommand::AddApiKey {
            email,
            region,
            key_stdin,
            tags,
        } => {
            let tags = normalize_cli_tags(tags)?;
            let key = if key_stdin {
                read_import_source(None, true).await?
            } else {
                std::env::var("KIRO_API_KEY").context("请设置 KIRO_API_KEY 或使用 --key-stdin")?
            };
            let key = key.trim();
            if !key.starts_with("ksk_") || key.len() <= 4 || key.chars().any(char::is_whitespace) {
                return Err(anyhow!("Kiro API key 必须是非空的 ksk_... 凭据"));
            }
            let accounts = parse_import_payload(
                &serde_json::json!({
                    "email":email, "credentials":{
                        "access_token":key, "region":region, "expires_at":0, "auth_method":"api_key"
                    }
                })
                .to_string(),
            )?;
            let result: AccountImportResult = client
                .call(
                    method::ACCOUNT_IMPORT,
                    serde_json::json!({"accounts":accounts,"tags":tags}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "已导入 {} 个 API key 账号，跳过 {} 个",
                    result.imported,
                    result.skipped.len()
                );
            }
        }
        AccountCommand::Export { redact, provider } => {
            let accounts: serde_json::Value = client
                .call(
                    if provider.is_some() {
                        method::V2_ACCOUNT_EXPORT
                    } else {
                        method::ACCOUNT_EXPORT
                    },
                    serde_json::json!({"redact":redact,"provider":provider}),
                )
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
            tags,
        } => {
            let tags = normalize_cli_tags(tags)?;
            let start_url = resolve_start_url(client, start_url.as_deref()).await?;
            if let Some(file) = batch {
                run_sso_batch(
                    client,
                    &file,
                    SsoBatchOptions {
                        start_url: &start_url,
                        region: &region,
                        concurrency,
                        headful,
                        tags: &tags,
                        json,
                    },
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
                        "headful":headful,
                        "tags":tags
                    }),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!("已添加 {}（{}）", result.email, result.id);
            }
        }
        AccountCommand::Rm { ids, provider } => {
            if !crate::commands::confirm(&remove_confirmation_prompt(&ids)).await? {
                println!("已取消");
                return Ok(());
            }
            remove_accounts(client, &ids, provider.as_deref(), json).await?;
        }
        AccountCommand::Enable { id, provider } => {
            set_enabled(client, &id, provider.as_deref(), true, json).await?
        }
        AccountCommand::Disable { id, provider } => {
            set_enabled(client, &id, provider.as_deref(), false, json).await?
        }
        AccountCommand::Tag {
            ids,
            add,
            remove,
            provider,
        } => {
            tag_accounts(client, &ids, &add, &remove, provider.as_deref(), json).await?;
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
        AccountCommand::Refresh { id, all, provider } => {
            require_id_or_all(id.as_deref(), all)?;
            if all {
                let provider = provider.as_deref().unwrap_or("kiro");
                let list: ProviderAccountListResult = client
                    .call(
                        method::V2_ACCOUNT_LIST,
                        serde_json::json!({"provider":provider}),
                    )
                    .await?;
                let mut failures = Vec::new();
                let mut results = Vec::new();
                for account in list.accounts {
                    match client
                        .call::<serde_json::Value>(
                            method::V2_ACCOUNT_REFRESH,
                            serde_json::json!({
                                "provider":account.provider_id,
                                "id":account.id
                            }),
                        )
                        .await
                    {
                        Ok(result) => results.push(result),
                        Err(error) => failures.push(error.to_string()),
                    }
                }
                if json {
                    print_json(&serde_json::json!({"results":results,"errors":failures}))?;
                } else {
                    println!("已刷新 {} 个账号", results.len());
                }
                if !failures.is_empty() {
                    return Err(anyhow!(
                        "{} 个账号刷新失败：{}",
                        failures.len(),
                        failures.join("; ")
                    ));
                }
            } else {
                let result: serde_json::Value = client
                    .call(
                        method::V2_ACCOUNT_REFRESH,
                        serde_json::json!({"id":id,"provider":provider}),
                    )
                    .await?;
                print_result(&result, json, "账号 token 已刷新")?;
            }
        }
        AccountCommand::Probe { id, all, provider } => {
            require_id_or_all(id.as_deref(), all)?;
            let result = if all {
                run_provider_account_batch(
                    client,
                    method::V2_ACCOUNT_PROBE,
                    provider.as_deref().unwrap_or("kiro"),
                )
                .await?
            } else {
                client
                    .call(
                        method::V2_ACCOUNT_PROBE,
                        serde_json::json!({"id":id,"provider":provider}),
                    )
                    .await?
            };
            if json {
                print_json(&result)?;
            } else {
                let model_count = result["models"].as_array().map_or_else(
                    || {
                        result["results"].as_array().map_or(0, |results| {
                            results
                                .iter()
                                .map(|entry| entry["models"].as_array().map_or(0, Vec::len))
                                .sum()
                        })
                    },
                    Vec::len,
                );
                println!(
                    "账号 {} 探测成功，可用模型 {} 个",
                    id.as_deref().unwrap_or("all"),
                    model_count
                );
            }
            fail_provider_account_batch(&result, "探测")?;
        }
        AccountCommand::ResetHealth { id, all, provider } => {
            require_id_or_all(id.as_deref(), all)?;
            let result = if all {
                run_provider_account_batch(
                    client,
                    method::V2_ACCOUNT_RESET_HEALTH,
                    provider.as_deref().unwrap_or("kiro"),
                )
                .await?
            } else {
                client
                    .call(
                        method::V2_ACCOUNT_RESET_HEALTH,
                        serde_json::json!({"id":id,"provider":provider}),
                    )
                    .await?
            };
            print_result(&result, json, "账号健康状态已重置")?;
            fail_provider_account_batch(&result, "重置健康状态")?;
        }
    }
    Ok(())
}

async fn run_provider_account_batch(
    client: &mut AdminClient,
    method_name: &str,
    provider: &str,
) -> Result<serde_json::Value> {
    let list: ProviderAccountListResult = client
        .call(
            method::V2_ACCOUNT_LIST,
            serde_json::json!({"provider":provider}),
        )
        .await?;
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for account in list.accounts {
        match client
            .call::<serde_json::Value>(
                method_name,
                serde_json::json!({
                    "provider":account.provider_id,
                    "id":account.id
                }),
            )
            .await
        {
            Ok(result) => results.push(result),
            Err(error) => errors.push(error.to_string()),
        }
    }
    Ok(serde_json::json!({
        "scope":{"provider":provider},
        "results":results,
        "errors":errors,
        "complete":errors.is_empty()
    }))
}

fn fail_provider_account_batch(value: &serde_json::Value, operation: &str) -> Result<()> {
    let errors = value["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 个账号{}失败：{}",
            errors.len(),
            operation,
            errors.join("; ")
        ))
    }
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

fn normalize_cli_tags(tags: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(anyhow!("账号标签不能为空"));
        }
        if !normalized.iter().any(|existing| existing == tag) {
            normalized.push(tag.to_owned());
        }
    }
    normalized.sort();
    Ok(normalized)
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

fn remove_confirmation_prompt(ids: &[String]) -> String {
    if let [id] = ids {
        format!("确认删除账号 {id}？")
    } else {
        format!("确认删除以下 {} 个账号：{}？", ids.len(), ids.join("、"))
    }
}

async fn remove_accounts(
    client: &mut AdminClient,
    ids: &[String],
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    if let [id] = ids {
        let result: serde_json::Value = client
            .call(
                method::V2_ACCOUNT_REMOVE,
                serde_json::json!({"id": id,"provider":provider}),
            )
            .await?;
        if json {
            print_json(&result)?;
        } else {
            println!("已删除 {id}");
        }
        return Ok(());
    }

    let mut failures = Vec::new();
    for id in ids {
        match client
            .call::<serde_json::Value>(
                method::V2_ACCOUNT_REMOVE,
                serde_json::json!({"id": id,"provider":provider}),
            )
            .await
        {
            Ok(result) if json => print_json(&result)?,
            Ok(_) => println!("已删除 {id}"),
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 个账号删除失败：\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

/// Tag one or more accounts.
///
/// A provider-qualified reference resolves per account through the v2 method,
/// which is single-account only; the unqualified Kiro path keeps the batch
/// method so a batch stays all-or-nothing.
async fn tag_accounts(
    client: &mut AdminClient,
    ids: &[String],
    add: &[String],
    remove: &[String],
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    let provider_qualified =
        provider.is_some() || ids.iter().any(|id| id.split_once('/').is_some());
    if provider_qualified {
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            let result: AccountTagResult = client
                .call(
                    method::V2_ACCOUNT_TAG,
                    serde_json::json!({
                        "id": id,
                        "add": add,
                        "remove": remove,
                        "provider": provider,
                    }),
                )
                .await?;
            results.push(result);
        }
        if json {
            print_json(&AccountTagBatchResult { accounts: results })?;
        } else {
            for result in &results {
                print_tag_result(result);
            }
        }
    } else if let [id] = ids {
        let result: AccountTagResult = client
            .call(
                method::ACCOUNT_TAG,
                serde_json::json!({"id": id, "add": add, "remove": remove}),
            )
            .await?;
        if json {
            print_json(&result)?;
        } else {
            print_tag_result(&result);
        }
    } else {
        let result: AccountTagBatchResult = client
            .call(
                method::ACCOUNT_TAG,
                serde_json::json!({"ids": ids, "add": add, "remove": remove}),
            )
            .await?;
        if json {
            print_json(&result)?;
        } else {
            for account in &result.accounts {
                print_tag_result(account);
            }
        }
    }
    Ok(())
}

fn print_tag_result(result: &AccountTagResult) {
    let tags = result.tags.join(",");
    println!(
        "{} 当前标签：{}",
        result.id,
        if tags.is_empty() { "-" } else { &tags }
    );
}

async fn set_enabled(
    client: &mut AdminClient,
    id: &str,
    provider: Option<&str>,
    enabled: bool,
    json: bool,
) -> Result<()> {
    let result: serde_json::Value = client
        .call(
            method::V2_ACCOUNT_SET_ENABLED,
            serde_json::json!({"id": id, "provider":provider, "enabled": enabled}),
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

struct SsoBatchOptions<'a> {
    start_url: &'a str,
    region: &'a str,
    concurrency: usize,
    headful: bool,
    tags: &'a [String],
    json: bool,
}

async fn run_sso_batch(
    client: &AdminClient,
    file: &str,
    options: SsoBatchOptions<'_>,
) -> Result<()> {
    use futures::{stream, StreamExt};

    if !(1..=8).contains(&options.concurrency) {
        return Err(anyhow!("并发数必须在 1..=8 之间"));
    }
    let raw = read_sso_batch_source(file).await?;
    let rows = parse_sso_csv(&raw)?;
    let socket = client.socket_path();
    let start_url = options.start_url.to_string();
    let region = options.region.to_string();
    let tags = options.tags.to_vec();
    let headful = options.headful;
    let results = stream::iter(rows.into_iter().map(|(email, password)| {
        let socket = socket.clone();
        let start_url = start_url.clone();
        let region = region.clone();
        let tags = tags.clone();
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
                        "headful":headful,
                        "tags":tags
                    }),
                )
                .await;
            (email, result)
        }
    }))
    .buffer_unordered(options.concurrency)
    .collect::<Vec<_>>()
    .await;
    let mut failures = Vec::new();
    for (email, result) in results {
        match result {
            Ok(summary) if options.json => print_json(&summary)?,
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

fn print_account_services(result: &AccountServicesResult, json: bool) -> Result<()> {
    if json {
        return print_json(result);
    }
    if result.services.is_empty() {
        println!(
            "账号 {}（{}）未绑定任何 API 代理服务。",
            result.account_email, result.account_id
        );
        return Ok(());
    }
    let rows = result
        .services
        .iter()
        .map(|service| {
            vec![
                service.service_id.clone(),
                service.service_name.clone(),
                format!("{}:{}", service.host, service.port),
                if service.running {
                    "运行中".into()
                } else if service.enabled {
                    "未运行".into()
                } else {
                    "已停用".into()
                },
                display_binding_sources(service),
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "账号      {}（{}）",
        result.account_email, result.account_id
    );
    println!(
        "{}",
        render_table(&["服务 ID", "名称", "监听", "状态", "绑定来源"], &rows)
    );
    Ok(())
}

fn display_binding_sources(service: &AccountServiceBinding) -> String {
    service
        .binding_sources
        .iter()
        .map(|source| {
            source
                .strip_prefix("tag:")
                .map(|tag| format!("标签:{tag}"))
                .unwrap_or_else(|| match source.as_str() {
                    "global" => "全局池".into(),
                    "manual" => "手工".into(),
                    other => other.to_owned(),
                })
        })
        .collect::<Vec<_>>()
        .join(",")
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
            credit_current: Some(120.126),
            credit_limit: Some(500.5),
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
        assert_eq!(rows[0][3], "120.13/500.50");
        assert_eq!(rows[0][5], "prod,pro");

        let mut exhausted = summary(true);
        exhausted.credit_exhausted = true;
        assert_eq!(build_list_rows(&[exhausted])[0][2], "额度耗尽");

        let mut protected = summary(true);
        protected.health = Some("low_credit".into());
        assert_eq!(build_list_rows(&[protected])[0][2], "低额度保护");
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
    fn account_creation_tags_are_trimmed_sorted_and_deduplicated() {
        assert_eq!(
            normalize_cli_tags(vec![" prod ".into(), "team-a".into(), "prod".into()])
                .expect("tags"),
            ["prod", "team-a"]
        );
        assert!(normalize_cli_tags(vec![" ".into()]).is_err());

        let binding = AccountServiceBinding {
            service_id: "svc_1".into(),
            service_name: "team".into(),
            host: "127.0.0.1".into(),
            port: 5581,
            enabled: true,
            running: true,
            binding_sources: vec!["tag:team-a".into(), "manual".into()],
        };
        assert_eq!(display_binding_sources(&binding), "标签:team-a,手工");
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

    #[test]
    fn remove_confirmation_describes_single_and_multiple_accounts() {
        assert_eq!(
            remove_confirmation_prompt(&["acc_00000001".into()]),
            "确认删除账号 acc_00000001？"
        );
        assert_eq!(
            remove_confirmation_prompt(&["acc_00000001".into(), "a@example.com".into()]),
            "确认删除以下 2 个账号：acc_00000001、a@example.com？"
        );
    }

    #[tokio::test]
    async fn batch_remove_continues_after_an_account_fails() {
        use kproxy_ipc::protocol::{decode_line, encode_line, Request, Response, RpcError};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("admin.sock");
        let listener = UnixListener::bind(&socket).expect("bind socket");
        let server = tokio::spawn(async move {
            let mut received = Vec::new();
            for index in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept client");
                let (read_half, mut write_half) = stream.into_split();
                let raw = BufReader::new(read_half)
                    .lines()
                    .next_line()
                    .await
                    .expect("read request")
                    .expect("request line");
                let request: Request = decode_line(&raw).expect("decode request");
                received.push(
                    request.params["id"]
                        .as_str()
                        .expect("account id")
                        .to_owned(),
                );
                let response = if index == 0 {
                    Response::err(request.id, RpcError::bad_params("account not found"))
                } else {
                    Response::ok(request.id, serde_json::json!({"removed": received[index]}))
                };
                write_half
                    .write_all(encode_line(&response).expect("encode response").as_bytes())
                    .await
                    .expect("write response");
            }
            received
        });

        let mut client = AdminClient::connect(socket);
        let error = remove_accounts(
            &mut client,
            &["missing@example.com".into(), "acc_00000002".into()],
            None,
            false,
        )
        .await
        .expect_err("first removal should fail");
        assert!(error.to_string().contains("missing@example.com"));
        assert_eq!(
            server.await.expect("server task"),
            ["missing@example.com", "acc_00000002"]
        );
    }

    #[tokio::test]
    async fn batch_tag_sends_one_request_with_all_account_ids() {
        use kproxy_ipc::protocol::{decode_line, encode_line, Request, Response};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("admin.sock");
        let listener = UnixListener::bind(&socket).expect("bind socket");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let (read_half, mut write_half) = stream.into_split();
            let raw = BufReader::new(read_half)
                .lines()
                .next_line()
                .await
                .expect("read request")
                .expect("request line");
            let request: Request = decode_line(&raw).expect("decode request");
            let response = Response::ok(
                request.id,
                serde_json::json!({
                    "accounts": [
                        {"id": "acc_00000001", "tags": ["test"]},
                        {"id": "acc_00000002", "tags": ["test"]}
                    ]
                }),
            );
            write_half
                .write_all(encode_line(&response).expect("encode response").as_bytes())
                .await
                .expect("write response");
            request
        });

        let mut client = AdminClient::connect(socket);
        tag_accounts(
            &mut client,
            &["acc_00000001".into(), "acc_00000002".into()],
            &["test".into()],
            &[],
            None,
            false,
        )
        .await
        .expect("batch tag");
        let request = server.await.expect("server task");
        assert_eq!(request.method, method::ACCOUNT_TAG);
        assert_eq!(
            request.params,
            serde_json::json!({
                "ids": ["acc_00000001", "acc_00000002"],
                "add": ["test"],
                "remove": []
            })
        );
    }
}
