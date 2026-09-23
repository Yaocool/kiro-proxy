//! Long-form operational guides kept separate from generated command help.

use anyhow::{anyhow, Result};
use clap::ValueEnum;

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Topic {
    Provider,
    Kiro,
    #[value(alias = "github-copilot")]
    Copilot,
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
            Self::Provider => "provider",
            Self::Kiro => "kiro",
            Self::Copilot => "copilot",
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
    ("provider", "Kiro/Copilot 命令适用范围与提供源管理"),
    ("kiro", "Kiro 账号、模型与服务接入"),
    ("copilot", "GitHub Copilot 账号、模型与服务接入"),
    ("status", "服务状态与时间范围"),
    ("health", "健康检查与业务就绪"),
    ("version", "版本和默认上游端点"),
    ("account", "账号管理与确认规则"),
    ("sso", "Kiro IAM Identity Center SSO 单账号与批量导入"),
    ("service", "代理服务生命周期"),
    ("apikey", "API key、限额与用量"),
    ("pool", "Kiro 评分与 Copilot 账号模型支持"),
    ("balance", "Kiro 账号池调度评分原理"),
    ("diagnose", "Kiro 网络和账号真实推理诊断"),
    ("subscriptions", "Kiro 订阅计划查询"),
    ("tasks", "Kiro/Copilot/全局周期任务"),
    ("stats", "持久化统计与时间范围"),
    ("logs", "结构化日志、文件和 Trace ID"),
    ("alert", "Kiro 异常事件和通知目标"),
    ("models", "动态模型发现与解析"),
    ("model-map", "模型映射匹配规则"),
    ("config", "Kiro/Copilot/全局配置范围与编辑"),
    ("docker", "Docker 运行方式和数据卷"),
];

pub fn print(topic: Option<&str>) -> Result<()> {
    let Some(topic) = topic else {
        println!("可用指南主题：");
        for (name, description) in TOPICS {
            println!("  {name:<16}{description}");
        }
        println!("\n新用户可先看 `kproxy guide kiro` 或 `kproxy guide copilot`；命令参数用 `kproxy help <命令路径>` 查看。");
        return Ok(());
    };
    println!("{}", topic_text(topic)?);
    Ok(())
}

fn topic_text(topic: &str) -> Result<&'static str> {
    let text = match topic {
        "provider" => {
            r#"命令适用范围
  两者共用：provider、status、ready、account list/show/rm/enable/disable/tag/export/refresh/probe/reset-health、pool、models、model-map、service、apikey、stats、logs show/follow；按命令使用 --provider 选择实例，省略时注意各命令的默认来源。
  Kiro 专项：account add-sso/add-api-key/import/services/overage/regen-machine-id、diagnose、subscriptions、pool --explain 评分、alert 事件、tasks token_refresh/status_check/health_recheck。
  Copilot 专项：account add --provider copilot --auth device-flow（或 --token-stdin）；OAuth/endpoint 设置属于 Copilot provider 实例。
  全局：health、version、config、logs trace/files/path、daemon 生命周期；其中 config 模块各有适用范围，见 kproxy guide config。

提供源实例
  kproxy provider list
  kproxy provider show kiro
  kproxy provider show copilot

未显式配置 provider 时内置 kiro；首次添加 Copilot 会保留 Kiro。service/API key/model-map 的创建或编辑命令通过 --provider 设置来源范围，list/test 等读取命令用它过滤或选择来源；account 的写命令用它定位账号，status/ready/account 的读命令及 pool/models/stats/logs 用它过滤或选择来源。
接入步骤：kproxy guide kiro / kproxy guide copilot"#
        }
        "kiro" => {
            r#"Kiro 接入（未显式配置 provider 时默认提供 kiro）
1. 添加账号：
   printf '%s\n' "$PASSWORD" | kproxy account add-sso --email user@example.com --start-url https://example.awsapps.com/start --password-stdin
   或 kproxy account add-api-key --email user@example.com --key-stdin < /secure/kiro-key
   SSO 批量导入：kproxy guide sso
2. 检查账号和模型：
   kproxy account list --provider kiro
   kproxy models list --provider kiro --refresh
3. 创建 Kiro 专用服务（端口可按需修改）：
   kproxy service create --name kiro --port 5580 --provider kiro --default-provider kiro

diagnose all/endpoints、account overage 是 Kiro 专项功能。"#
        }
        "copilot" => {
            r#"GitHub Copilot 接入
1. 准备已启用 Device Flow 的 GitHub OAuth App Client ID，并配置提供源：
   kproxy provider add --id copilot --kind copilot --setting 'client_id="Iv1.xxx"'
   已有实例可用 kproxy provider edit copilot --setting 'client_id="Iv1.xxx"' 更新。
2. 添加账号；按提示在浏览器完成 GitHub 登录及组织要求的 SSO：
   kproxy account add --provider copilot --auth device-flow
   CLI 会轮询直到授权和模型探测结束，无需提供用户名密码，也无需另开 kproxy 会话。多个 GitHub 用户可重复此命令，共用同一 Client ID。
3. 检查账号和模型：
   kproxy account list --provider copilot
   kproxy account probe --provider copilot <ACCOUNT_ID>
   kproxy models list --provider copilot --refresh
4. 创建 Copilot 专用服务（与 Kiro 服务使用不同端口）：
   kproxy service create --name copilot --port 5581 --provider copilot --default-provider copilot

Copilot API 地址优先使用账号 token 返回的 endpoint；仅当缺失且企业网络要求专属域名时才需配置 api_endpoint_fallback。"#
        }
        "status" => {
            "`kproxy status` 展示 Kiro/Copilot 提供源、代理服务、账号池和本次启动后的请求统计；`--provider kiro|copilot` 只看指定来源，省略时聚合全部；`--since` 或 `--start/--end` 控制时间范围，`--watch` 每 2 秒刷新。"
        }
        "health" => {
            "`kproxy health` 是全局 daemon 管理面检查，不按来源拆分；账号或代理服务为空不会让该检查失败。`kproxy ready --provider kiro|copilot` 检查指定来源引用的服务与账号是否就绪；省略来源时检查整体业务就绪。"
        }
        "version" => "`kproxy version` 显示 CLI 版本、Rust MSRV 和默认 Kiro 上游端点；Copilot API 地址由各账号的 token 响应决定。",
        "account" => {
            "Kiro 使用 `account add-sso`、`add-api-key` 或 `import`；Copilot 使用 `account add --provider copilot --auth device-flow`，也可用 `--token-stdin` 导入 GitHub token。`list/show/rm/enable/disable/tag/export/refresh/probe/reset-health` 覆盖两者，可用 `--provider` 消歧；但 `refresh --all` 省略来源时默认 Kiro。Kiro 的 `probe` 做真实推理，Copilot 的 `probe` 刷新凭证与模型目录；Copilot 标签不会影响服务的 `--account-tag`。`overage`、`regen-machine-id`、`services` 仅适用于 Kiro。详情见 `kproxy guide kiro` 和 `kproxy guide copilot`。"
        }
        "balance" => {
            "本评分仅用于 Kiro 账号池：score = active_ratio×weight_active + used_credit_ratio×weight_credit + recent_idle_penalty×weight_idle。\n分数越低越优先；随后加入小幅随机抖动。用 `kproxy pool --provider kiro --watch --explain` 查看实时评分。Copilot 的 `pool` 只显示账号认证与模型支持，不提供这组三因子评分。"
        }
        "pool" => {
            "`kproxy pool --provider kiro --model <模型> --explain` 展示 Kiro 账号可调度性、排队和三因子评分；省略 --provider 默认 Kiro。`--provider copilot --model <模型>` 展示 Copilot 账号认证状态及已发现模型，默认模型名来自 Kiro，查询 Copilot 时应显式传入 --model；--explain 不提供 Kiro 式评分。两者均可 --watch；Kiro 评分原理见 `kproxy guide balance`。"
        }
        "model-map" => {
            "模型映射按 priority 从小到大匹配；Kiro/Copilot 都可用 `--provider <ID>` 指定规则范围。add 省略来源时为 Kiro-only，edit 省略时保留原有范围。source_models 支持 `*`；replace/alias 选首个目标，loadbalance 按 weights 随机。`--below-credits-percent` 仅适用于 Kiro，不能与 Copilot 规则或跨来源目标组合。用 `model-map list/test --provider copilot` 核对 Copilot 映射；跨来源映射另需显式开启 provider 的 allow_cross_provider_fallback。"
        }
        "sso" => {
            r#"本主题仅适用于 Kiro 的 IAM Identity Center SSO；GitHub Copilot（即使组织登录经过 Azure SSO）请使用 `kproxy guide copilot` 和 Device Flow，不使用 `account add-sso`。先在配置中设置 `[sso] start_url = "https://..."`。单账号：`printf '%s\n' "$PASSWORD" | kproxy account add-sso --email user@example.com --password-stdin`。
批量：CSV 仅含 email,password 两列，运行 `kproxy account add-sso --batch accounts.csv -c 1 --tag team-a`；也可用 `--batch - < accounts.csv` 从 stdin 读取。可重复的 `--tag` 应用于本批全部账号；`--start-url` 可覆盖全局值，`--headful` 可手工完成额外验证。默认/full 构建包含 SSO。"#
        }
        "service" => {
            "`kproxy service list/show/create/edit/enable/disable/apikeys/delete` 管理独立代理监听。create 省略来源时保持 Kiro-only；`service create --name copilot --port 5581 --provider copilot --default-provider copilot` 创建 Copilot-only 服务；`--provider kiro --provider copilot --default-provider kiro` 创建双来源服务。多个服务需要使用不同端口。`service accounts/add-account/remove-account` 与 `--account-tag` 当前仅作用于 Kiro 账号池，不用于筛选或操作 Copilot 账号；Copilot 可用 `account list --provider copilot` 查看。修改 Kiro 账号标签前须先停用服务。API key 也有独立来源范围；删除服务时仅级联删除未共享 key。"
        }
        "config" => {
            r#"配置默认位于 $KPROXY_HOME/config.toml；全局设置无需为 Kiro/Copilot 分别复制。
  全局：server、log、admin。
  Kiro 专项：upstream、pool、features、models、context、storage、sso、model-thinking-mode、notify、webhook。
  Copilot 专项：[[provider]] 中 kind="copilot" 的 [provider.settings]（client_id、OAuth、endpoint、身份头）、[provider.pool]、[provider.models]、[provider.routing]。
  按来源：provider、model-mapping、api-key、proxy-service；用实例 ID、providers/allowed_providers 和 default_provider 限定。
  混合：tasks 中 token_refresh/status_check 属于 Kiro，stats_persist 为全局任务；model_cache_refresh 可按来源运行。

  注意：`config show` 的原始或生效配置可能包含凭证；请勿直接粘贴输出到日志或工单。

`kproxy config list` 显示每个模块的适用来源，`show <模块> --effective` 查看合并默认值后的配置。修改 Copilot 实例优先用 `kproxy provider edit copilot --setting 'client_id="Iv1.xxx"'`；Kiro SSO 用 `config edit sso`，不是 Copilot 的 Azure SSO URL。`config reset <模块>` 只重置所选模块；不指定模块会重置 Kiro 与全局运行参数、清除模型映射，同时保留提供源、API key、服务和告警。server.host/port 仅是新建服务默认值，已有服务用 service edit 修改；admin.socket 和 TLS 模式切换需重启 daemon。"#
        }
        "apikey" => {
            "API key 可用 `--provider kiro`、`--provider copilot` 限定允许来源；add 省略时保持 Kiro-only，重复传入可允许两者，并可用 `--model 'copilot/*'` 限制模型。service create 自动生成的 key 继承服务来源范围。`apikey list/usage/history --provider <ID>` 是查询过滤；`limit` 的累计 credits 上限跨该 key 的来源共用，`reset-usage` 清除全部来源累计用量，不单独按来源重置。`show/edit` 可查看或修改范围；删除需确认。"
        }
        "diagnose" => {
            "`kproxy diagnose all/endpoints/account` 是 Kiro 上游专项诊断：检查 CodeWhisperer/AmazonQ/OIDC 端点，并对 Kiro 账号发起真实推理。Copilot 请用 `kproxy account probe --provider copilot <ACCOUNT_ID>` 和 `kproxy models list --provider copilot --refresh` 检查账号与模型；不要把 Kiro 端点诊断结果当成 Copilot 可用性。"
        }
        "subscriptions" => {
            "`kproxy subscriptions --provider kiro [账号]` 查询 Kiro 上游企业订阅计划；省略账号时使用可调度 Kiro 账号。Copilot 不提供权威订阅/席位数据，`--provider copilot` 或 `all` 中该来源返回 supported=false；Copilot 可用性请用 `account probe --provider copilot` 和 `models list --provider copilot` 检查。"
        }
        "tasks" => {
            "`kproxy tasks list` 查看全部任务。`token_refresh/status_check/health_recheck` 操作 Kiro 账号池；`model_cache_refresh` 可用 `tasks run model_cache_refresh --provider kiro|copilot` 按来源刷新；`stats_persist/daily_reset/proxy_service_reconcile` 为全局任务。其他任务不接受 --provider。Copilot API token 在其 provider 内独立按需刷新，不使用 Kiro 的 token_refresh 任务。"
        }
        "stats" => {
            "`kproxy stats` 默认聚合 Kiro/Copilot 的持久化请求统计；`--provider kiro|copilot` 限定来源，`--detail --by provider` 比较来源分组。可用 `--since 1h` 或带时区的 `--start/--end` 查询时间段；`--by model|account|apikey|endpoint` 查看其他维度。"
        }
        "logs" => {
            "`kproxy logs show/follow --provider kiro|copilot` 查看对应来源的结构化请求日志；省略来源时显示全部，并可配合 --account、--level 和 --tail。`logs trace <TRACE_ID>` 按 trace ID 跨全部来源和日志分片查询，不提供来源参数；`logs files/path` 管理全局日志文件和路径。"
        }
        "alert" => {
            "`kproxy alert events` 当前列出的额度保护、额度耗尽、服务额度耗尽和 token 刷新失败事件来自 Kiro 账号池；不能作为 Copilot Enterprise 席位、授权或额度监控。`alert platforms/config` 和 `add/edit/delete/list/test/logs` 管理这些事件的通知目标；通知平台本身是共享机制，但当前没有 Copilot 专项告警事件。"
        }
        "models" => {
            "`kproxy models list` 默认聚合全部来源；`--provider kiro --refresh` 和 `--provider copilot --refresh` 分别刷新并查看 Kiro、Copilot 的模型、输入上下文和输出上限，`-` 表示上游未返回对应元数据。`--mapped` 同时显示显式映射结果。`kproxy models resolve <MODEL_ID> --provider <ID>` 显示指定来源中的映射与最终模型；还可配合 `--api-key` 和 `--refresh`。"
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
    Ok(text)
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

    #[test]
    fn provider_guides_cover_both_auth_flows_and_sso_boundary() {
        assert!(topic_text("kiro").unwrap().contains("account add-sso"));
        assert!(topic_text("copilot").unwrap().contains("device-flow"));
        assert!(topic_text("copilot")
            .unwrap()
            .contains("--default-provider copilot"));
        assert!(topic_text("sso").unwrap().contains("仅适用于 Kiro"));
        assert!(topic_text("diagnose")
            .unwrap()
            .contains("Kiro 上游专项诊断"));
        assert_eq!(
            Topic::from_str("github-copilot", false).unwrap().as_str(),
            "copilot"
        );
    }

    #[test]
    fn command_and_config_guides_describe_provider_scope() {
        let provider = topic_text("provider").unwrap();
        assert!(provider.contains("Kiro 专项"));
        assert!(provider.contains("Copilot 专项"));
        assert!(provider.contains("全局"));

        let config = topic_text("config").unwrap();
        assert!(config.contains("server、log、admin"));
        assert!(config.contains("upstream、pool"));
        assert!(config.contains("client_id"));

        assert!(topic_text("pool").unwrap().contains("--explain 不提供"));
        assert!(topic_text("model-map").unwrap().contains("仅适用于 Kiro"));
        assert!(topic_text("subscriptions")
            .unwrap()
            .contains("supported=false"));
        assert!(topic_text("alert").unwrap().contains("不能作为 Copilot"));
    }
}
