//! kiro-proxy 命令行客户端。

mod client;
mod commands;
mod output;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use kproxy_ipc::protocol::{
    method, ConfigPathResult, ConfigReloadResult, ConfigShowResult, StatusResult,
};

use crate::client::{resolve_socket, AdminClient};
use crate::output::{format_relative, format_timestamp, print_json};

#[derive(Debug, Parser)]
#[command(
    name = "kproxy",
    version,
    disable_help_subcommand = true,
    about = "kiro-proxy 管理工具",
    long_about = "查看服务状态，管理服务生命周期、账号与配置。\n\n示例：\n  kproxy status\n  kproxy restart\n  kproxy account list --tag prod\n  kproxy config show --effective"
)]
struct Cli {
    /// 管理面 socket 路径，默认读取配置文件。
    #[arg(long, global = true, value_name = "PATH", env = "KPROXY_ADMIN_SOCKET")]
    socket: Option<String>,
    /// 以 JSON 输出。
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 服务总览。
    #[command(after_help = "示例：\n  kproxy status\n  kproxy status --watch")]
    Status {
        /// 每 2 秒刷新。
        #[arg(long)]
        watch: bool,
    },
    /// 供容器和 systemd 使用的健康检查。
    #[command(after_help = "示例：\n  kproxy health\n  kproxy --json health")]
    Health,
    /// 检查业务代理是否已具备接收请求的条件。
    #[command(after_help = "示例：\n  kproxy ready\n  kproxy --json ready")]
    Ready,
    /// 显示版本与默认上游端点。
    #[command(after_help = "示例：\n  kproxy version\n  kproxy --json version")]
    Version,
    /// 重启 kproxyd（Docker 宿主机命令）。
    #[command(after_help = "示例：\n  kproxy restart")]
    Restart,
    /// 停止 kproxyd（Docker 宿主机命令）。
    #[command(after_help = "示例：\n  kproxy stop\n\n停止后可用 `kproxy restart` 重新启动。")]
    Stop,
    /// 完全卸载 Docker 服务、数据卷、专用镜像和宿主机 wrapper。
    #[command(
        after_help = "示例：\n  kproxy uninstall\n  kproxy uninstall --backup-dir /srv/backups\n  kproxy uninstall --yes --keep-backup\n  kproxy uninstall --yes --delete-backup\n\n卸载前必定先备份数据。默认备份目录为 ~/.kproxy/backups；会永久删除配置、账号、统计和日志，但不删除源码目录。"
    )]
    Uninstall {
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
        /// 宿主机备份根目录；默认 ~/.kproxy/backups。
        #[arg(long, value_name = "PATH", env = "KPROXY_BACKUP_DIR")]
        backup_dir: Option<String>,
        /// 卸载成功后保留备份。
        #[arg(long, conflicts_with = "delete_backup")]
        keep_backup: bool,
        /// 卸载成功后删除备份。
        #[arg(long, conflicts_with = "keep_backup")]
        delete_backup: bool,
    },
    /// 配置查看、编辑、重载与重置。
    #[command(subcommand)]
    Config(ConfigCommand),
    /// 账号管理。
    #[command(subcommand)]
    Account(crate::commands::account::AccountCommand),
    /// 查看账号池调度评分。
    #[command(
        after_help = "示例：\n  kproxy pool --explain\n  kproxy pool --model claude-sonnet-4 --watch"
    )]
    Pool {
        #[arg(long, default_value = "minimax-m2.5")]
        model: String,
        #[arg(long)]
        watch: bool,
        /// 显示三因子评分明细；默认输出仍保留机器可读的完整数据。
        #[arg(long)]
        explain: bool,
    },
    /// 上游网络与账号诊断。
    #[command(after_help = "示例：\n  kproxy diagnose endpoints\n  kproxy diagnose account --all")]
    Diagnose {
        #[command(subcommand)]
        command: Option<DiagnoseCommand>,
    },
    /// 查询上游可用订阅计划。
    #[command(after_help = "示例：\n  kproxy subscriptions\n  kproxy subscriptions acc_7f3a2b1c")]
    Subscriptions { id: Option<String> },
    /// 显示或手动运行周期任务。
    #[command(after_help = "示例：\n  kproxy tasks\n  kproxy tasks run status_check")]
    Tasks {
        #[command(subcommand)]
        command: Option<TaskCommand>,
    },
    /// 显示代理统计。
    #[command(
        after_help = "示例：\n  kproxy stats --since 1h\n  kproxy stats --detail --by model --recent 20"
    )]
    Stats {
        /// 显示分组和最近请求明细。
        #[arg(long)]
        detail: bool,
        #[arg(long, requires = "detail")]
        recent: Option<usize>,
        /// 时间窗口，例如 30m、1h、7d。
        #[arg(long)]
        since: Option<String>,
        /// 分组维度：model/account/apikey/endpoint。
        #[arg(long, requires = "detail")]
        by: Option<String>,
    },
    /// 显示最近请求日志。
    #[command(
        after_help = "示例：\n  kproxy logs --tail 100\n  kproxy logs --follow --level error"
    )]
    Logs {
        #[arg(long, default_value_t = 50)]
        tail: usize,
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(long)]
        level: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// API key 管理。
    #[command(name = "apikey", subcommand)]
    ApiKey(crate::commands::runtime::ApiKeyCommand),
    /// API 代理服务管理。
    #[command(name = "service", subcommand)]
    Service(crate::commands::runtime::ServiceCommand),
    /// 告警策略与通知目标管理。
    #[command(name = "alert", subcommand)]
    Alert(crate::commands::runtime::AlertCommand),
    /// 显示上游动态模型。
    #[command(after_help = "示例：\n  kproxy models\n  kproxy models --mapped")]
    Models {
        /// 同时显示每个模型经过映射规则后的结果。
        #[arg(long)]
        mapped: bool,
    },
    /// 模型映射规则。
    #[command(name = "model-map", subcommand)]
    ModelMap(ModelMapCommand),
    /// 查看主题帮助。不指定主题时列出全部主题。
    #[command(after_help = "示例：\n  kproxy help\n  kproxy help stats\n  kproxy help model-map")]
    Help { topic: Option<String> },
}

#[derive(Debug, Subcommand)]
enum ModelMapCommand {
    /// 列出全部映射规则。
    #[command(after_help = "示例：\n  kproxy model-map list\n  kproxy --json model-map list")]
    List,
    /// 添加模型映射规则。
    #[command(
        after_help = "示例：\n  kproxy model-map add --name low-credit --source 'claude-opus-*' --target claude-sonnet-4 --below-credits-percent 10"
    )]
    Add {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "replace")]
        kind: String,
        #[arg(long = "source", value_delimiter = ',', required = true)]
        source_models: Vec<String>,
        #[arg(long = "target", value_delimiter = ',', required = true)]
        target_models: Vec<String>,
        #[arg(long, default_value_t = 0)]
        priority: i32,
        #[arg(long = "weight", value_delimiter = ',')]
        weights: Vec<u32>,
        /// 账号剩余 credits 百分比低于此值时生效。
        #[arg(long)]
        below_credits_percent: Option<f64>,
        #[arg(long = "api-key", value_delimiter = ',')]
        api_key_ids: Vec<String>,
        #[arg(long)]
        disabled: bool,
    },
    /// 编辑模型映射规则。
    Edit {
        /// 当前规则名。
        name: String,
        #[arg(long)]
        rename: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long = "source", value_delimiter = ',')]
        source_models: Vec<String>,
        #[arg(long = "target", value_delimiter = ',')]
        target_models: Vec<String>,
        #[arg(long)]
        priority: Option<i32>,
        #[arg(long = "weight", value_delimiter = ',')]
        weights: Vec<u32>,
        #[arg(long)]
        clear_weights: bool,
        #[arg(long)]
        below_credits_percent: Option<f64>,
        #[arg(long)]
        clear_credits_threshold: bool,
        #[arg(long = "api-key", value_delimiter = ',')]
        api_key_ids: Vec<String>,
        #[arg(long)]
        clear_api_keys: bool,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },
    /// 删除模型映射规则，执行前需输入 y 或 yes 确认。
    #[command(name = "delete", visible_alias = "rm")]
    Delete { name: String },
    /// 测试客户端模型名会命中的规则。
    #[command(
        after_help = "示例：\n  kproxy model-map test claude-sonnet-4\n  kproxy model-map test claude-opus-4 --remaining-credits-percent 8"
    )]
    Test {
        model: String,
        #[arg(long)]
        remaining_credits_percent: Option<f64>,
        #[arg(long)]
        api_key: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// 立即运行一个任务。
    #[command(
        after_help = "示例：\n  kproxy tasks run status_check\n  kproxy tasks run model_cache_refresh\n  kproxy tasks run proxy_service_reconcile"
    )]
    Run { name: String },
}

#[derive(Debug, Subcommand)]
enum DiagnoseCommand {
    /// 探测 CodeWhisperer、AmazonQ 与 OIDC 网络连通性。
    #[command(
        after_help = "示例：\n  kproxy diagnose endpoints\n  kproxy diagnose endpoints --region us-west-2"
    )]
    Endpoints {
        #[arg(long, default_value = "us-east-1")]
        region: String,
    },
    /// 拉取模型并发一次真实推理验证账号存活。
    #[command(
        after_help = "示例：\n  kproxy diagnose account acc_7f3a2b1c\n  kproxy diagnose account --all --concurrency 4"
    )]
    Account {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// 单账号真实推理探测超时，例如 30s、2m。
        #[arg(long, default_value = "45s")]
        timeout: String,
        /// `--all` 时的探测并发数，范围 1..8。
        #[arg(short = 'c', long, default_value_t = 1)]
        concurrency: usize,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// 显示配置。
    #[command(after_help = "示例：\n  kproxy config show\n  kproxy config show --effective")]
    Show {
        /// 显示合并默认值后的生效配置。
        #[arg(long)]
        effective: bool,
    },
    /// 打印全部文件路径。
    #[command(after_help = "示例：\n  kproxy config path\n  kproxy --json config path")]
    Path,
    /// 手动触发重载。
    #[command(after_help = "示例：\n  kproxy config reload\n  kproxy --json config reload")]
    Reload,
    /// 用 $VISUAL/$EDITOR 打开配置并在保存后校验、重载。
    #[command(after_help = "示例：\n  kproxy config edit\n  EDITOR=vim kproxy config edit")]
    Edit,
    /// 备份当前配置并恢复全部默认设置，执行前需输入 y 或 yes 确认。
    #[command(
        after_help = "示例：\n  kproxy config reset\n\n会清除配置中的代理服务、API key、模型映射和告警目标；原配置自动备份。"
    )]
    Reset,
    /// 只校验配置，不应用。
    #[command(
        after_help = "示例：\n  kproxy config validate\n  kproxy config validate ./config.toml"
    )]
    Validate { file: Option<String> },
}

#[tokio::main]
async fn main() -> Result<()> {
    kproxy_store::environment::load_dotenv()?;
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    if let Command::Help { topic } = &command {
        crate::commands::runtime::print_topic(topic.as_deref())?;
        return Ok(());
    }
    if matches!(
        &command,
        Command::Alert(crate::commands::runtime::AlertCommand::Events)
    ) {
        crate::commands::runtime::show_alert_events(cli.json)?;
        return Ok(());
    }
    if matches!(
        &command,
        Command::Restart | Command::Stop | Command::Uninstall { .. }
    ) {
        anyhow::bail!(
            "该命令需在 Docker 宿主机通过 kproxy wrapper 执行。\n\
             请在仓库根目录运行 `./deploy/docker-setup.sh` 安装或更新 wrapper；\n\
             原生 systemd 部署请使用 `sudo systemctl restart|stop kproxyd`。"
        );
    }
    let socket = resolve_socket(cli.socket.clone()).await;
    let mut client = AdminClient::connect(socket);

    match command {
        Command::Status { watch } => loop {
            let status: StatusResult = client.call(method::STATUS, serde_json::json!({})).await?;
            if cli.json {
                print_json(&status)?;
            } else {
                print_status(&status);
            }
            if !watch {
                break;
            }
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; break; }
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            }
        },
        Command::Health => {
            let status: StatusResult = client.call(method::STATUS, serde_json::json!({})).await?;
            if cli.json {
                print_json(&serde_json::json!({"healthy":true,"status":status}))?;
            } else {
                println!("ok");
            }
        }
        Command::Ready => {
            let status: StatusResult = client.call(method::STATUS, serde_json::json!({})).await?;
            if cli.json {
                print_json(&serde_json::json!({
                    "ready":status.ready,
                    "reasons":status.readiness_reasons.clone(),
                    "status":status
                }))?;
            } else if status.ready {
                println!("ready");
            } else {
                anyhow::bail!("not ready: {}", status.readiness_reasons.join("; "));
            }
            if !status.ready {
                anyhow::bail!("business proxy is not ready");
            }
        }
        Command::Version => {
            let value = serde_json::json!({
                "version":env!("CARGO_PKG_VERSION"),
                "rust":env!("CARGO_PKG_RUST_VERSION"),
                "codewhisperer":kproxy_kiro::endpoint::CODEWHISPERER_URL,
                "amazonq":kproxy_kiro::endpoint::AMAZONQ_URL
            });
            if cli.json {
                print_json(&value)?;
            } else {
                println!(
                    "kproxy {} (Rust {})",
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_RUST_VERSION")
                );
                println!("CodeWhisperer {}", kproxy_kiro::endpoint::CODEWHISPERER_URL);
                println!("AmazonQ        {}", kproxy_kiro::endpoint::AMAZONQ_URL);
            }
        }
        Command::Restart | Command::Stop | Command::Uninstall { .. } => {
            unreachable!("host lifecycle commands returned before connecting to the daemon")
        }
        Command::Config(ConfigCommand::Show { effective }) => {
            let show: ConfigShowResult = client
                .call(method::CONFIG_SHOW, serde_json::json!({}))
                .await?;
            if cli.json {
                print_json(&show)?;
            } else if effective {
                println!("{}", serde_json::to_string_pretty(&show.effective_json)?);
            } else {
                print!("{}", show.raw);
            }
        }
        Command::Config(ConfigCommand::Path) => {
            let paths: ConfigPathResult = client
                .call(method::CONFIG_PATH, serde_json::json!({}))
                .await?;
            if cli.json {
                print_json(&paths)?;
            } else {
                println!("配置文件    {}", paths.config_file);
                println!("账号库      {}", paths.accounts_file);
                println!("日用量      {}", paths.daily_file);
                println!("统计        {}", paths.stats_file);
                println!("管理 socket {}", paths.admin_socket);
            }
        }
        Command::Config(ConfigCommand::Reload) => {
            let result: ConfigReloadResult = client
                .call(method::CONFIG_RELOAD, serde_json::json!({}))
                .await?;
            if cli.json {
                print_json(&result)?;
            } else if result.applied {
                println!("配置已重载");
                for field in result.needs_restart {
                    println!("注意：{field} 需重启 kproxyd 才能生效");
                }
            } else {
                println!(
                    "重载失败，已保留原配置：{}",
                    result.error.unwrap_or_else(|| "未知原因".into())
                );
            }
        }
        Command::Config(ConfigCommand::Edit) => {
            crate::commands::runtime::edit_config(&mut client).await?;
        }
        Command::Config(ConfigCommand::Reset) => {
            if let Some(result) = crate::commands::runtime::reset_config(&mut client).await? {
                if cli.json {
                    print_json(&serde_json::json!({
                        "config_file": result.config_file,
                        "backup_file": result.backup_file,
                        "applied": true,
                        "needs_restart": result.needs_restart,
                    }))?;
                } else {
                    println!("配置已恢复为默认设置并重载");
                    println!("原配置备份 {}", result.backup_file.display());
                    for field in result.needs_restart {
                        println!("注意：{field} 需重启 kproxyd 才能生效");
                    }
                }
            } else if cli.json {
                print_json(&serde_json::json!({"cancelled": true}))?;
            } else {
                println!("已取消");
            }
        }
        Command::Config(ConfigCommand::Validate { file }) => {
            crate::commands::runtime::validate_config(file.as_deref()).await?;
        }
        Command::Account(command) => {
            crate::commands::account::run(&mut client, command, cli.json).await?;
        }
        Command::Pool {
            model,
            watch,
            explain,
        } => loop {
            crate::commands::runtime::simple_rpc(
                &mut client,
                method::POOL,
                serde_json::json!({"model":model,"explain":explain}),
                cli.json,
            )
            .await?;
            if !watch {
                break;
            }
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            }
        },
        Command::Diagnose { command } => match command {
            Some(DiagnoseCommand::Endpoints { region }) => {
                crate::commands::runtime::simple_rpc(
                    &mut client,
                    method::DIAGNOSE_ENDPOINTS,
                    serde_json::json!({"region":region}),
                    cli.json,
                )
                .await?;
            }
            Some(DiagnoseCommand::Account {
                id,
                all,
                timeout,
                concurrency,
            }) => {
                if id.is_none() && !all {
                    anyhow::bail!("需指定账号 ID 或 --all");
                }
                let timeout_secs = crate::commands::runtime::parse_duration(&timeout)?;
                crate::commands::runtime::simple_rpc(
                    &mut client,
                    method::DIAGNOSE_ACCOUNT,
                    serde_json::json!({
                        "id":id,
                        "all":all,
                        "timeout_secs":timeout_secs,
                        "concurrency":concurrency
                    }),
                    cli.json,
                )
                .await?;
            }
            None => {
                let endpoints: serde_json::Value = client
                    .call(
                        method::DIAGNOSE_ENDPOINTS,
                        serde_json::json!({"region":"us-east-1"}),
                    )
                    .await?;
                let accounts: serde_json::Value = client
                    .call(
                        method::DIAGNOSE_ACCOUNT,
                        serde_json::json!({
                            "all":true,"timeout_secs":45,"concurrency":1
                        }),
                    )
                    .await?;
                print_json(&serde_json::json!({
                    "endpoints":endpoints,"accounts":accounts
                }))?;
            }
        },
        Command::Subscriptions { id } => {
            crate::commands::runtime::simple_rpc(
                &mut client,
                method::SUBSCRIPTIONS,
                serde_json::json!({"id":id}),
                cli.json,
            )
            .await?;
        }
        Command::Tasks { command } => {
            let (method_name, params) = match command {
                Some(TaskCommand::Run { name }) => {
                    (method::TASK_RUN, serde_json::json!({"name":name}))
                }
                None => (method::TASKS, serde_json::json!({})),
            };
            crate::commands::runtime::simple_rpc(&mut client, method_name, params, cli.json)
                .await?;
        }
        Command::Stats {
            detail,
            recent,
            since,
            by,
        } => {
            let since_secs = since
                .as_deref()
                .map(crate::commands::runtime::parse_duration)
                .transpose()?;
            crate::commands::runtime::show_stats(
                &mut client,
                detail,
                recent,
                since_secs,
                by.as_deref(),
                cli.json,
            )
            .await?;
        }
        Command::Logs {
            tail,
            follow,
            level,
            account,
        } => {
            crate::commands::runtime::show_logs(
                &mut client,
                tail,
                follow,
                level.as_deref(),
                account.as_deref(),
                cli.json,
            )
            .await?;
        }
        Command::ApiKey(command) => {
            crate::commands::runtime::run_apikey(&mut client, command, cli.json).await?;
        }
        Command::Service(command) => {
            crate::commands::runtime::run_service(&mut client, command, cli.json).await?;
        }
        Command::Alert(command) => {
            crate::commands::runtime::run_alert(&mut client, command, cli.json).await?;
        }
        Command::Models { mapped } => {
            crate::commands::runtime::show_models(&mut client, mapped, cli.json).await?;
        }
        Command::ModelMap(command) => {
            crate::commands::runtime::run_model_map(&mut client, command, cli.json).await?;
        }
        Command::Help { .. } => unreachable!("help exits before connecting to kproxyd"),
    }
    Ok(())
}

fn print_status(status: &StatusResult) {
    println!(
        "kproxyd {}   运行 {}   PID {}",
        status.version,
        format_relative(status.uptime_secs as i64),
        status.pid
    );
    println!(
        "监听    {}        管理 {}",
        status.listen, status.admin_socket
    );
    println!(
        "代理    {} 个（{} 运行）",
        status.proxy_service_total, status.proxy_service_running
    );
    println!(
        "账号    {} 个（{} 可用 / {} 冷却 / {} 额度耗尽 / {} 封禁 / {} 停用）",
        status.account_total,
        status.account_available,
        status.account_cooling,
        status.account_exhausted,
        status.account_banned,
        status.account_total.saturating_sub(status.account_enabled)
    );
    if status.ready {
        println!("就绪    是");
    } else {
        println!("就绪    否（{}）", status.readiness_reasons.join("；"));
    }
    println!(
        "并发    {} 进行中 / 上限 {}     队列 {} 等待",
        status.active_requests, status.max_concurrent_requests, status.queued_requests
    );
    println!(
        "统计    {} 请求   {:.1}% 成功   均值 {}ms   credits {:.3}",
        status.request_count, status.success_rate, status.average_latency_ms, status.credits
    );
    if status.daily_credit_limit > 0.0 {
        println!(
            "日额度  {}   {:.3} 已用 + {:.3} 在途 / {:.3}",
            status.daily_credit_day,
            status.daily_credit_used,
            status.daily_credit_reserved,
            status.daily_credit_limit
        );
    }
    match status.config_reloaded_at {
        Some(at) => println!(
            "配置    {}（{} 重载）",
            status.config_path,
            format_timestamp(at)
        ),
        None => println!("配置    {}", status.config_path),
    }
    if let Some(hint) = &status.hint {
        println!("提示    {hint}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_reset_is_available_as_a_subcommand() {
        let cli = Cli::try_parse_from(["kproxy", "config", "reset"]).expect("config reset");
        assert!(matches!(
            cli.command,
            Some(Command::Config(ConfigCommand::Reset))
        ));
    }

    #[test]
    fn alert_command_replaces_the_webhook_entrypoint() {
        let cli = Cli::try_parse_from([
            "kproxy",
            "alert",
            "config",
            "--low-credit-threshold-percent",
            "15",
            "--max-notifications",
            "3",
            "--suppress-window",
            "20m",
        ])
        .expect("alert command");
        let Some(Command::Alert(crate::commands::runtime::AlertCommand::Config {
            low_credit_threshold_percent,
            max_notifications,
            suppress_window,
        })) = cli.command
        else {
            panic!("expected alert config command");
        };
        assert_eq!(low_credit_threshold_percent, Some(15.0));
        assert_eq!(max_notifications, Some(3));
        assert_eq!(suppress_window.as_deref(), Some("20m"));
        assert!(Cli::try_parse_from(["kproxy", "webhook", "list"]).is_err());
    }

    #[test]
    fn alert_events_accept_repeated_and_comma_separated_values() {
        let cli = Cli::try_parse_from([
            "kproxy",
            "alert",
            "add",
            "--name",
            "ops",
            "--kind",
            "dingtalk",
            "--url",
            "https://example.com/hook",
            "--event",
            "low-credit,account-banned",
            "--event",
            "quota-exhausted",
        ])
        .expect("multi-event alert target");
        let Some(Command::Alert(crate::commands::runtime::AlertCommand::Add { events, .. })) =
            cli.command
        else {
            panic!("expected alert add command");
        };
        assert_eq!(
            events,
            vec![
                crate::commands::runtime::AlertEvent::LowCredit,
                crate::commands::runtime::AlertEvent::AccountBanned,
                crate::commands::runtime::AlertEvent::QuotaExhausted,
            ]
        );
    }
}
