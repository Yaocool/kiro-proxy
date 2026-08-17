//! 配置模型、校验规则与带注释的默认配置。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 配置校验错误。
#[derive(Debug, Error)]
pub enum ConfigError {
    /// 端口落入特权端口范围。
    #[error("server.port must be between 1024 and 65535, got {0}")]
    InvalidPort(u16),
    /// 对公网监听但没有启用的 API key。
    #[error("binding to a non-local host requires at least one enabled api key")]
    PublicBindWithoutApiKey,
    /// 比例字段不在闭区间 0..=1。
    #[error("{field} must be between 0.0 and 1.0, got {value}")]
    RatioOutOfRange {
        /// 字段名。
        field: &'static str,
        /// 非法值。
        value: f64,
    },
    #[error("invalid configuration value for {field}: {message}")]
    InvalidValue { field: String, message: String },
}

/// API key 格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyFormat {
    /// `sk-` 风格。
    #[default]
    Sk,
    /// 简单字符串。
    Simple,
    /// token 风格。
    Token,
}

/// 上游端点偏好。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endpoint {
    /// CodeWhisperer。
    Codewhisperer,
    /// Amazon Q。
    Amazonq,
}

/// Kiro agent mode。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// 自动选择。
    Auto,
    /// Vibe mode。
    Vibe,
    /// Spec mode。
    Spec,
}

/// 思考内容输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingOutputFormat {
    /// Claude thinking block。
    Claude,
    /// OpenAI reasoning_content。
    Openai,
}

fn default_true() -> bool {
    true
}

/// TLS 配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TlsConfig {
    /// 是否启用 HTTPS。
    pub enabled: bool,
    /// 证书文件路径。
    pub cert_path: Option<String>,
    /// 私钥文件路径。
    pub key_path: Option<String>,
    /// 证书 PEM 内容。
    pub cert: Option<String>,
    /// 私钥 PEM 内容。
    pub key: Option<String>,
}

/// 基于延迟反馈的自适应并发。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdaptiveConfig {
    /// 是否启用。
    pub enabled: bool,
    /// 检查间隔。
    pub check_interval_ms: u64,
    /// P99 超过此值时降低并发。
    pub p99_degrade_ms: u64,
    /// P99 低于此值时恢复并发。
    pub p99_recover_ms: u64,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_ms: 10_000,
            p99_degrade_ms: 200,
            p99_recover_ms: 100,
        }
    }
}

/// API 代理服务的共享参数和新建服务默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// 新建代理服务的默认监听主机。
    pub host: String,
    /// 新建代理服务的默认监听端口。
    pub port: u16,
    /// 是否校验 Claude 客户端 User-Agent。
    pub enforce_user_agent_check: bool,
    /// 全局准入上限。
    pub max_concurrent_requests: usize,
    /// HTTP keepalive 超时。
    pub keep_alive_timeout_ms: u64,
    /// 最大连接数。
    pub max_connections: usize,
    /// TLS 配置。
    pub tls: TlsConfig,
    /// 自适应并发配置。
    pub adaptive: AdaptiveConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 5580,
            enforce_user_agent_check: true,
            max_concurrent_requests: 500,
            keep_alive_timeout_ms: 30_000,
            max_connections: 2_000,
            tls: TlsConfig::default(),
            adaptive: AdaptiveConfig::default(),
        }
    }
}

/// 双连接池配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamPoolConfig {
    /// 非流式连接数。
    pub http_max_connections: usize,
    /// 非流式 HTTP/1.1 pipelining 数。
    pub http_pipelining: usize,
    /// 流式连接数。
    pub stream_max_connections: usize,
    /// 流式连接必须为 1，避免队首阻塞。
    pub stream_pipelining: usize,
    /// 连接空闲时间。
    pub keep_alive_idle_ms: u64,
    /// 连接最大寿命。
    pub keep_alive_max_ms: u64,
}

impl Default for UpstreamPoolConfig {
    fn default() -> Self {
        Self {
            http_max_connections: 128,
            http_pipelining: 5,
            stream_max_connections: 256,
            stream_pipelining: 1,
            keep_alive_idle_ms: 30_000,
            keep_alive_max_ms: 60_000,
        }
    }
}

/// 上游请求配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    /// 首选上游端点；空值按账号类型推断。
    pub preferred_endpoint: Option<Endpoint>,
    /// Kiro agent mode。
    pub agent_mode: AgentMode,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 期望提前刷新 token 的秒数。实际值还会受定时扫描间隔的安全下限约束。
    pub token_refresh_before_expiry: i64,
    /// 双连接池参数。
    pub pool: UpstreamPoolConfig,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            preferred_endpoint: None,
            agent_mode: AgentMode::Vibe,
            max_retries: 3,
            token_refresh_before_expiry: 900,
            pool: UpstreamPoolConfig::default(),
        }
    }
}

/// 三因子选号权重。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceConfig {
    /// 并发压力权重。
    pub weight_active: f64,
    /// 额度消耗权重。
    pub weight_credit: f64,
    /// 最近使用权重。
    pub weight_idle: f64,
    /// 空闲归一化窗口。
    pub idle_window_ms: u64,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            weight_active: 0.5,
            weight_credit: 0.4,
            weight_idle: 0.1,
            idle_window_ms: 300_000,
        }
    }
}

/// 错误冷却与配额恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CooldownConfig {
    /// 连续错误阈值。
    pub max_error_count: u32,
    /// 一般冷却时长。
    pub cooldown_ms: u64,
    /// 单次错误短冷却。
    pub error_cooldown_ms: u64,
    /// 额度耗尽后重新探测间隔。
    pub quota_reset_ms: u64,
    /// 额度错误滑动窗口。
    pub quota_error_window_ms: u64,
    /// 额度错误阈值。
    pub quota_error_threshold: u32,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            max_error_count: 3,
            cooldown_ms: 30_000,
            error_cooldown_ms: 5_000,
            quota_reset_ms: 300_000,
            quota_error_window_ms: 300_000,
            quota_error_threshold: 50,
        }
    }
}

/// 账号池配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// 单账号并发上限。
    pub max_concurrent_per_account: usize,
    /// 排队容量。
    pub max_queue_size: usize,
    /// 最大排队时间。
    pub max_queue_wait_ms: u64,
    /// 队列满时等待时间。
    pub queue_full_wait_ms: u64,
    /// 低额度百分比阈值。
    pub low_credit_ratio: f64,
    /// 低额度绝对值阈值。
    pub low_credit_min_remaining: f64,
    /// 每日额度上限；0 表示不限。
    pub daily_credit_limit: f64,
    /// 每 1,000 个估算 token 预留的 credits。
    pub credit_estimate_per_1k_tokens: f64,
    /// 预留估算中计入的最大输出 token 数。
    pub credit_estimate_output_token_cap: u32,
    /// 额度耗尽时是否自动切号。
    pub auto_switch_on_quota_exhausted: bool,
    /// 选号权重。
    pub balance: BalanceConfig,
    /// 错误冷却。
    pub cooldown: CooldownConfig,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_concurrent_per_account: 50,
            max_queue_size: 10,
            max_queue_wait_ms: 30_000,
            queue_full_wait_ms: 5_000,
            low_credit_ratio: 0.0,
            low_credit_min_remaining: 7.0,
            daily_credit_limit: 0.0,
            credit_estimate_per_1k_tokens: 1.0,
            credit_estimate_output_token_cap: 8_192,
            auto_switch_on_quota_exhausted: true,
            balance: BalanceConfig::default(),
            cooldown: CooldownConfig::default(),
        }
    }
}

/// 功能开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeaturesConfig {
    /// 工具调用后自动续轮次数。
    pub auto_continue_rounds: u32,
    /// 是否移除工具。
    pub disable_tools: bool,
    /// 429 时是否启用同族模型降级。
    pub enable_model_fallback: bool,
    /// 是否模拟 prompt cache。
    pub enable_prompt_cache: bool,
    /// 是否注入增强 system prompt。
    pub enhance_system_prompt: bool,
    /// 是否缓冲工具调用。
    pub buffer_tool_calls: bool,
    /// 工具调用缓冲延迟。
    pub tool_call_buffer_delay_ms: u64,
    /// thinking 输出格式。
    pub thinking_output_format: ThinkingOutputFormat,
    /// 是否启用自适应 thinking。
    pub adaptive_thinking: bool,
    /// thinking budget 兜底上限。
    pub max_thinking_budget_tokens: u32,
    /// 是否转换 web 工具。
    pub enable_web_tools: bool,
    /// 是否过滤 tool leak。
    pub enable_tool_leak_filter: bool,
    /// 空字符串表示自动选择。
    pub default_model_id: String,
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            auto_continue_rounds: 0,
            disable_tools: false,
            enable_model_fallback: true,
            enable_prompt_cache: false,
            enhance_system_prompt: true,
            buffer_tool_calls: true,
            tool_call_buffer_delay_ms: 500,
            thinking_output_format: ThinkingOutputFormat::Claude,
            adaptive_thinking: true,
            max_thinking_budget_tokens: 8192,
            enable_web_tools: true,
            enable_tool_leak_filter: true,
            default_model_id: String::new(),
        }
    }
}

/// 动态模型发现。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// 是否从上游动态发现模型。
    pub dynamic_discovery: bool,
    /// 模型缓存有效期。
    pub cache_ttl_ms: u64,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            dynamic_discovery: true,
            cache_ttl_ms: 3_600_000,
        }
    }
}

/// 周期任务间隔。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksConfig {
    /// Token 预刷新扫描间隔。
    pub token_refresh_interval_ms: u64,
    /// 状态检查间隔。
    pub status_check_interval_ms: u64,
    /// 统计落盘间隔。
    pub stats_persist_interval_ms: u64,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            token_refresh_interval_ms: 300_000,
            status_check_interval_ms: 60_000,
            stats_persist_interval_ms: 60_000,
        }
    }
}

/// 上下文窗口保护。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// 默认最大输入 token。
    pub max_input_tokens: u32,
    /// 普通请求安全比例。
    pub safe_input_ratio: f64,
    /// compact 请求安全比例。
    pub compact_safe_input_ratio: f64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_input_tokens: 200_000,
            safe_input_ratio: 0.95,
            compact_safe_input_ratio: 0.99,
        }
    }
}

/// 存储优化。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// 超过此账号数时压缩。
    pub compression_threshold: usize,
    /// 是否增量写。
    pub incremental_write: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 100,
            incremental_write: true,
        }
    }
}

/// 分级递进告警。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// 初始低额度档位。
    pub low_credit_threshold_percent: f64,
    /// 最大告警档位数。
    pub max_notifications: u32,
    /// 非额度事件抑制窗口。
    pub suppress_window_ms: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            low_credit_threshold_percent: 10.0,
            max_notifications: 5,
            suppress_window_ms: 1_800_000,
        }
    }
}

/// 日志配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// tracing 级别。
    pub level: String,
    /// `json` 或 `pretty`。
    pub format: String,
    /// 日志基础路径；空字符串使用数据目录下的 `logs/kproxyd.log`。
    pub file_path: String,
    /// 每个级别、每个分片的最大大小。
    pub max_file_size_mb: u64,
    /// 按 UTC 日期保留最近多少天的文件。
    pub retention_days: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "json".into(),
            file_path: String::new(),
            max_file_size_mb: 100,
            retention_days: 3,
        }
    }
}

/// 管理面配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminConfig {
    /// Unix socket 路径。
    pub socket: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            socket: default_socket_path(),
        }
    }
}

fn default_socket_path() -> String {
    default_socket_path_from(
        std::env::var("KPROXY_HOME").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

fn default_socket_path_from(kproxy_home: Option<&str>, xdg_runtime: Option<&str>) -> String {
    if let Some(home) = kproxy_home {
        return format!("{home}/admin.sock");
    }
    if let Some(runtime) = xdg_runtime {
        return format!("{runtime}/kproxy/admin.sock");
    }
    "/run/kproxy/admin.sock".into()
}

/// 模型映射规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMappingRule {
    /// 规则名。
    pub name: String,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `replace`、`alias` 或 `loadbalance`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 源模型 glob。
    #[serde(default)]
    pub source_models: Vec<String>,
    /// 目标模型。
    #[serde(default)]
    pub target_models: Vec<String>,
    /// 数字越小优先级越高。
    #[serde(default)]
    pub priority: i32,
    /// 负载均衡权重。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<Vec<u32>>,
    /// 剩余额度条件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_remaining_credit_percent: Option<f64>,
    /// API key 条件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_ids: Option<Vec<String>>,
    /// 可选生效时间窗口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ModelMappingSchedule>,
}

/// 模型映射生效时间窗口。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMappingSchedule {
    /// always/daily/range。
    #[serde(default = "default_schedule_mode")]
    pub mode: String,
    /// daily: 0=周日..6=周六。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days_of_week: Option<Vec<u8>>,
    /// daily: 兼容配置文件中的 mon/tue/... 写法。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<Vec<String>>,
    /// daily: 当天开始分钟数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_minutes: Option<u16>,
    /// daily: 兼容配置文件中的 HH:MM 写法。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// daily: 当天结束分钟数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_minutes: Option<u16>,
    /// daily: 兼容配置文件中的 HH:MM 写法。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// range: 起始 Unix 毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<i64>,
    /// range: 结束 Unix 毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_at: Option<i64>,
}

fn default_schedule_mode() -> String {
    "always".into()
}

/// Webhook 目标。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// 可读名称。
    pub name: String,
    /// dingtalk/wechat-work/telegram/discord/feishu/custom。
    pub kind: String,
    /// 目标 URL。
    pub url: String,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 订阅事件。
    #[serde(default)]
    pub events: Vec<String>,
    /// 钉钉签名密钥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dingtalk_sign: Option<String>,
    /// Telegram chat ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram_chat_id: Option<String>,
    /// 自定义模板。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_template: Option<String>,
}

/// API key 配置项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    /// 稳定 ID；为空时后续阶段生成。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 可读名称。
    pub name: String,
    /// key 内容。
    pub key: String,
    /// key 格式。
    #[serde(default)]
    pub format: ApiKeyFormat,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// credits 上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_limit: Option<f64>,
}

/// 一个独立的 API 代理监听实例。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyServiceConfig {
    /// 稳定服务 ID。
    pub id: String,
    /// 可读名称。
    pub name: String,
    /// 监听地址。
    pub host: String,
    /// 监听端口。
    pub port: u16,
    /// 是否启动监听。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 允许访问此服务的 API key ID。
    #[serde(default)]
    pub api_key_ids: Vec<String>,
    /// 创建时间（Unix 秒）。
    #[serde(default)]
    pub created_at: i64,
}

/// 顶层配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// API 代理服务共享参数和默认值。
    pub server: ServerConfig,
    /// 上游。
    pub upstream: UpstreamConfig,
    /// 账号池。
    pub pool: PoolConfig,
    /// 功能开关。
    pub features: FeaturesConfig,
    /// 模型发现。
    pub models: ModelsConfig,
    /// 周期任务。
    pub tasks: TasksConfig,
    /// 上下文保护。
    pub context: ContextConfig,
    /// 存储参数。
    pub storage: StorageConfig,
    /// 告警参数。
    pub notify: NotifyConfig,
    /// 日志。
    pub log: LogConfig,
    /// 管理面。
    pub admin: AdminConfig,
    /// 模型映射列表。
    #[serde(default, rename = "model_mapping")]
    pub model_mapping: Vec<ModelMappingRule>,
    /// Webhook 列表。
    #[serde(default, rename = "webhook")]
    pub webhook: Vec<WebhookConfig>,
    /// API key 列表。
    #[serde(default, rename = "api_key")]
    pub api_key: Vec<ApiKeyConfig>,
    /// API 代理服务列表。默认为空；kproxyd 只启动管理面。
    #[serde(default, rename = "proxy_service")]
    pub proxy_service: Vec<ProxyServiceConfig>,
    /// 模型级 thinking 默认值。
    #[serde(default, rename = "model_thinking_mode")]
    pub model_thinking_mode: BTreeMap<String, bool>,
}

fn is_local_host(host: &str) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    let normalized = lower
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(&lower);
    normalized.is_empty()
        || normalized == "localhost"
        || normalized == "127.0.0.1"
        || normalized == "::1"
}

impl Config {
    /// 是否存在启用且非空的 API key。
    pub fn has_enabled_api_key(&self) -> bool {
        self.api_key
            .iter()
            .any(|key| key.enabled && !key.key.trim().is_empty())
    }

    /// 返回运行时实际使用的 token 提前刷新窗口。
    ///
    /// 定时扫描可能在临界点前刚好跳过一个 token，因此窗口至少为扫描间隔的
    /// 两倍，且不低于 10 分钟。这也保证旧配置中的 300 秒不会导致刷新过晚。
    pub fn effective_token_refresh_before_expiry(&self) -> i64 {
        const MINIMUM_LEAD_SECS: u64 = 600;

        let configured = u64::try_from(self.upstream.token_refresh_before_expiry).unwrap_or(0);
        let scan_interval_secs = self.tasks.token_refresh_interval_ms.div_ceil(1_000);
        let effective = configured
            .max(scan_interval_secs.saturating_mul(2))
            .max(MINIMUM_LEAD_SECS);
        i64::try_from(effective).unwrap_or(i64::MAX)
    }

    /// 校验配置。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.port < 1024 {
            return Err(ConfigError::InvalidPort(self.server.port));
        }
        if self.server.keep_alive_timeout_ms == 0 {
            return invalid_config("server.keep_alive_timeout_ms", "must be greater than zero");
        }
        if self.server.tls.enabled {
            let inline = self
                .server
                .tls
                .cert
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && self
                    .server
                    .tls
                    .key
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
            let files = self
                .server
                .tls
                .cert_path
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && self
                    .server
                    .tls
                    .key_path
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
            if !inline && !files {
                return invalid_config(
                    "server.tls",
                    "enabled TLS requires both inline cert/key or both cert_path/key_path",
                );
            }
        }
        for (field, value) in [
            ("context.safe_input_ratio", self.context.safe_input_ratio),
            (
                "context.compact_safe_input_ratio",
                self.context.compact_safe_input_ratio,
            ),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(ConfigError::RatioOutOfRange { field, value });
            }
        }
        for (field, value) in [
            ("pool.low_credit_ratio", self.pool.low_credit_ratio),
            (
                "notify.low_credit_threshold_percent",
                self.notify.low_credit_threshold_percent / 100.0,
            ),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return invalid_config(field, "must be a finite value in 0.0..=1.0");
            }
        }
        for (field, value) in [
            ("pool.daily_credit_limit", self.pool.daily_credit_limit),
            (
                "pool.credit_estimate_per_1k_tokens",
                self.pool.credit_estimate_per_1k_tokens,
            ),
            (
                "pool.low_credit_min_remaining",
                self.pool.low_credit_min_remaining,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return invalid_config(field, "must be a finite non-negative number");
            }
        }
        for (field, value) in [
            (
                "pool.balance.weight_active",
                self.pool.balance.weight_active,
            ),
            (
                "pool.balance.weight_credit",
                self.pool.balance.weight_credit,
            ),
            ("pool.balance.weight_idle", self.pool.balance.weight_idle),
        ] {
            if !value.is_finite() || value < 0.0 {
                return invalid_config(field, "must be a finite non-negative number");
            }
        }
        if self.pool.balance.weight_active
            + self.pool.balance.weight_credit
            + self.pool.balance.weight_idle
            <= 0.0
        {
            return invalid_config(
                "pool.balance",
                "at least one scheduling weight must be greater than zero",
            );
        }
        for (field, value) in [
            (
                "server.max_concurrent_requests",
                self.server.max_concurrent_requests,
            ),
            ("server.max_connections", self.server.max_connections),
            (
                "upstream.pool.http_max_connections",
                self.upstream.pool.http_max_connections,
            ),
            (
                "upstream.pool.stream_max_connections",
                self.upstream.pool.stream_max_connections,
            ),
            (
                "upstream.pool.http_pipelining",
                self.upstream.pool.http_pipelining,
            ),
        ] {
            if value == 0 {
                return invalid_config(field, "must be greater than zero");
            }
        }
        if self.upstream.pool.stream_pipelining != 1 {
            return invalid_config(
                "upstream.pool.stream_pipelining",
                "must be exactly 1 to avoid stream head-of-line blocking",
            );
        }
        if self.upstream.pool.keep_alive_idle_ms == 0 || self.upstream.pool.keep_alive_max_ms == 0 {
            return invalid_config(
                "upstream.pool.keep_alive",
                "idle and maximum lifetime must be greater than zero",
            );
        }
        if self.server.adaptive.p99_recover_ms > self.server.adaptive.p99_degrade_ms {
            return invalid_config(
                "server.adaptive.p99_recover_ms",
                "must not exceed p99_degrade_ms",
            );
        }
        if self.server.adaptive.enabled
            && (self.server.adaptive.check_interval_ms == 0
                || self.server.adaptive.p99_degrade_ms == 0)
        {
            return invalid_config(
                "server.adaptive",
                "enabled adaptive admission requires positive interval and degrade threshold",
            );
        }
        if self.upstream.token_refresh_before_expiry < 0 {
            return invalid_config(
                "upstream.token_refresh_before_expiry",
                "must be non-negative",
            );
        }
        if self.pool.cooldown.max_error_count == 0 || self.pool.cooldown.quota_error_threshold == 0
        {
            return invalid_config(
                "pool.cooldown",
                "error and quota thresholds must be greater than zero",
            );
        }
        if self.context.max_input_tokens == 0 {
            return invalid_config("context.max_input_tokens", "must be greater than zero");
        }
        if self.models.dynamic_discovery && self.models.cache_ttl_ms == 0 {
            return invalid_config("models.cache_ttl_ms", "must be greater than zero");
        }
        for (field, value) in [
            (
                "tasks.token_refresh_interval_ms",
                self.tasks.token_refresh_interval_ms,
            ),
            (
                "tasks.status_check_interval_ms",
                self.tasks.status_check_interval_ms,
            ),
            (
                "tasks.stats_persist_interval_ms",
                self.tasks.stats_persist_interval_ms,
            ),
        ] {
            if value == 0 {
                return invalid_config(field, "must be greater than zero");
            }
        }
        if !matches!(self.log.format.as_str(), "json" | "pretty") {
            return invalid_config("log.format", "expected json or pretty");
        }
        if self.log.max_file_size_mb == 0 || self.log.retention_days == 0 {
            return invalid_config(
                "log",
                "max_file_size_mb and retention_days must be positive",
            );
        }
        let mut api_key_ids = BTreeSet::new();
        let mut api_key_names = BTreeSet::new();
        let mut api_key_values = BTreeSet::new();
        for (index, key) in self.api_key.iter().enumerate() {
            if key.name.trim().is_empty() {
                return invalid_config(format!("api_key.{index}.name"), "must not be empty");
            }
            if key.enabled && key.key.trim().is_empty() {
                return invalid_config(format!("api_key.{index}.key"), "must not be empty");
            }
            if !api_key_names.insert(key.name.as_str()) {
                return invalid_config(format!("api_key.{index}.name"), "must be unique");
            }
            if !key.key.is_empty() && !api_key_values.insert(key.key.as_str()) {
                return invalid_config(format!("api_key.{index}.key"), "must be unique");
            }
            if let Some(id) = key.id.as_deref() {
                if id.trim().is_empty() || !api_key_ids.insert(id) {
                    return invalid_config(
                        format!("api_key.{index}.id"),
                        "must be non-empty and unique",
                    );
                }
            }
            if key
                .credits_limit
                .is_some_and(|limit| !limit.is_finite() || limit < 0.0)
            {
                return invalid_config(
                    format!("api_key.{index}.credits_limit"),
                    "must be a finite non-negative number",
                );
            }
        }
        let mut service_ids = BTreeSet::new();
        let mut service_names = BTreeSet::new();
        let mut service_addresses = BTreeSet::new();
        for (index, service) in self.proxy_service.iter().enumerate() {
            let field = format!("proxy_service.{index}");
            if service.id.trim().is_empty() || !service_ids.insert(service.id.as_str()) {
                return invalid_config(format!("{field}.id"), "must be non-empty and unique");
            }
            if service.name.trim().is_empty() || !service_names.insert(service.name.as_str()) {
                return invalid_config(format!("{field}.name"), "must be non-empty and unique");
            }
            if service.host.trim().is_empty() {
                return invalid_config(format!("{field}.host"), "must not be empty");
            }
            if service.port < 1024 {
                return invalid_config(format!("{field}.port"), "must be between 1024 and 65535");
            }
            if service.enabled && !service_addresses.insert((service.host.as_str(), service.port)) {
                return invalid_config(
                    format!("{field}.port"),
                    "enabled services must use unique host and port pairs",
                );
            }
            if service.api_key_ids.is_empty() {
                return invalid_config(
                    format!("{field}.api_key_ids"),
                    "at least one API key is required",
                );
            }
            let mut bound_ids = BTreeSet::new();
            for key_id in &service.api_key_ids {
                if !bound_ids.insert(key_id.as_str()) {
                    return invalid_config(
                        format!("{field}.api_key_ids"),
                        "must not contain duplicate IDs",
                    );
                }
                if !api_key_ids.contains(key_id.as_str()) {
                    return invalid_config(
                        format!("{field}.api_key_ids"),
                        format!("references unknown API key ID {key_id}"),
                    );
                }
            }
            if service.enabled && !is_local_host(&service.host) {
                let has_enabled_key = self.api_key.iter().any(|key| {
                    key.enabled
                        && !key.key.trim().is_empty()
                        && key
                            .id
                            .as_ref()
                            .is_some_and(|id| service.api_key_ids.contains(id))
                });
                if !has_enabled_key {
                    return Err(ConfigError::PublicBindWithoutApiKey);
                }
            }
        }
        for (index, rule) in self.model_mapping.iter().enumerate() {
            if !matches!(rule.kind.as_str(), "replace" | "alias" | "loadbalance") {
                return invalid_config(
                    format!("model_mapping.{index}.type"),
                    "expected replace, alias, or loadbalance",
                );
            }
            if rule.enabled && (rule.source_models.is_empty() || rule.target_models.is_empty()) {
                return invalid_config(
                    format!("model_mapping.{index}"),
                    "enabled rules require source_models and target_models",
                );
            }
            if let Some(weights) = &rule.weights {
                if weights.len() != rule.target_models.len()
                    || weights.iter().all(|weight| *weight == 0)
                {
                    return invalid_config(
                        format!("model_mapping.{index}.weights"),
                        "must match target_models and contain a positive weight",
                    );
                }
            }
            if rule
                .max_remaining_credit_percent
                .is_some_and(|value| !value.is_finite() || !(0.0..=100.0).contains(&value))
            {
                return invalid_config(
                    format!("model_mapping.{index}.max_remaining_credit_percent"),
                    "must be in 0..=100",
                );
            }
            if let Some(schedule) = &rule.schedule {
                if schedule
                    .days_of_week
                    .as_ref()
                    .is_some_and(|days| days.iter().any(|day| *day > 6))
                {
                    return invalid_config(
                        format!("model_mapping.{index}.schedule.days_of_week"),
                        "weekday values must be in 0..=6",
                    );
                }
                if [schedule.start_minutes, schedule.end_minutes]
                    .into_iter()
                    .flatten()
                    .any(|minutes| minutes >= 24 * 60)
                {
                    return invalid_config(
                        format!("model_mapping.{index}.schedule"),
                        "minute-of-day values must be below 1440",
                    );
                }
            }
        }
        let mut webhook_names = BTreeSet::new();
        for (index, target) in self.webhook.iter().enumerate() {
            if target.name.trim().is_empty() || !webhook_names.insert(target.name.as_str()) {
                return invalid_config(
                    format!("webhook.{index}.name"),
                    "must be non-empty and unique",
                );
            }
            if !matches!(
                target.kind.as_str(),
                "dingtalk"
                    | "wechat-work"
                    | "wechat"
                    | "telegram"
                    | "discord"
                    | "feishu"
                    | "custom"
            ) {
                return invalid_config(format!("webhook.{index}.type"), "unsupported webhook type");
            }
            if target.enabled
                && !(target.url.starts_with("https://") || target.url.starts_with("http://"))
            {
                return invalid_config(
                    format!("webhook.{index}.url"),
                    "enabled webhook URL must use http or https",
                );
            }
            if target.events.iter().any(|event| {
                !matches!(
                    event.as_str(),
                    "low-credit"
                        | "account-banned"
                        | "token-expired"
                        | "quota-exhausted"
                        | "service-degraded"
                )
            }) {
                return invalid_config(
                    format!("webhook.{index}.events"),
                    "contains an unsupported event",
                );
            }
            if target.kind == "telegram"
                && target.enabled
                && target
                    .telegram_chat_id
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
            {
                return invalid_config(
                    format!("webhook.{index}.telegram_chat_id"),
                    "is required for enabled Telegram targets",
                );
            }
        }
        Ok(())
    }
}

fn invalid_config<T>(
    field: impl Into<String>,
    message: impl Into<String>,
) -> Result<T, ConfigError> {
    Err(ConfigError::InvalidValue {
        field: field.into(),
        message: message.into(),
    })
}

/// 首次运行写入的默认配置，每个字段附说明。
pub const DEFAULT_CONFIG_TOML: &str = r#"# kiro-proxy 配置文件
# 修改后自动生效（daemon 监听本文件），无需重启。
# admin.socket / TLS enabled 模式切换例外，改动需重启。

[server]
host = "0.0.0.0"                   # kproxy service create 的默认监听地址
port = 5580                        # kproxy service create 的默认端口，1024-65535
enforce_user_agent_check = true    # 仅作用于 Claude 路由
max_concurrent_requests = 500      # 超出返回 503 + Retry-After
keep_alive_timeout_ms = 30000
max_connections = 2000

[server.tls]
enabled = false
# cert_path = "/etc/kproxy/fullchain.pem"
# key_path = "/etc/kproxy/privkey.pem"

[server.adaptive]
enabled = true
check_interval_ms = 10000
p99_degrade_ms = 200
p99_recover_ms = 100

[upstream]
# preferred_endpoint = "amazonq"
agent_mode = "vibe"
max_retries = 3
# 默认提前 15 分钟；运行时不低于扫描间隔的 2 倍和 10 分钟
token_refresh_before_expiry = 900

[upstream.pool]
http_max_connections = 128
http_pipelining = 5
stream_max_connections = 256
stream_pipelining = 1              # 每流独占 socket，避免队首阻塞
keep_alive_idle_ms = 30000
keep_alive_max_ms = 60000

[pool]
max_concurrent_per_account = 50
max_queue_size = 10
max_queue_wait_ms = 30000
queue_full_wait_ms = 5000
low_credit_ratio = 0.0
low_credit_min_remaining = 7.0
daily_credit_limit = 0.0
credit_estimate_per_1k_tokens = 1.0  # 启发式预留系数；真实 usage 返回后按实结算
credit_estimate_output_token_cap = 8192
auto_switch_on_quota_exhausted = true

[pool.balance]
weight_active = 0.5
weight_credit = 0.4
weight_idle = 0.1
idle_window_ms = 300000

[pool.cooldown]
max_error_count = 3
cooldown_ms = 30000
error_cooldown_ms = 5000
quota_reset_ms = 300000
quota_error_window_ms = 300000
quota_error_threshold = 50

[features]
auto_continue_rounds = 0
disable_tools = false
enable_model_fallback = true
enable_prompt_cache = false
enhance_system_prompt = true
buffer_tool_calls = true
tool_call_buffer_delay_ms = 500
thinking_output_format = "claude"
adaptive_thinking = true
max_thinking_budget_tokens = 8192
enable_web_tools = true
enable_tool_leak_filter = true
default_model_id = ""

[models]
dynamic_discovery = true
cache_ttl_ms = 3600000

[tasks]
token_refresh_interval_ms = 300000
status_check_interval_ms = 60000
stats_persist_interval_ms = 60000

[context]
max_input_tokens = 200000
safe_input_ratio = 0.95
compact_safe_input_ratio = 0.99

[storage]
compression_threshold = 100
incremental_write = true

[notify]
low_credit_threshold_percent = 10.0
max_notifications = 5
suppress_window_ms = 1800000

[log]
level = "info"
format = "json"
file_path = ""
max_file_size_mb = 100
retention_days = 3

[admin]
socket = "/run/kproxy/admin.sock"

# 首次启动不创建或监听 API 代理服务。使用以下命令创建：
# kproxy service create --name main --port 5580
# 命令会同时生成首个 API key；之后可用以下命令按服务查询明文：
# kproxy service apikeys main --show-secret

# [[proxy_service]]
# id = "svc_..."
# name = "main"
# host = "0.0.0.0"
# port = 5580
# enabled = true
# api_key_ids = ["ak_..."]
# created_at = 0

# [[api_key]]
# name = "alice"
# key = "sk-..."
# format = "sk"
# enabled = true
# credits_limit = 5000

# [[model_mapping]]
# name = "opus 降级到 sonnet"
# enabled = true
# type = "replace"
# source_models = ["claude-opus-4*"]
# target_models = ["claude-sonnet-4"]
# priority = 10

# [[webhook]]
# name = "运维群"
# kind = "dingtalk"
# url = "https://oapi.dingtalk.com/robot/send?access_token=..."
# dingtalk_sign = "SEC..."
# enabled = true
# events = ["low-credit", "account-banned", "token-expired",
#           "quota-exhausted", "service-degraded"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::default();
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 5580);
        assert!(config.server.enforce_user_agent_check);
        assert_eq!(config.server.max_concurrent_requests, 500);
        assert!(!config.server.tls.enabled);

        assert_eq!(config.pool.max_concurrent_per_account, 50);
        assert_eq!(config.pool.max_queue_size, 10);
        assert_eq!(config.pool.max_queue_wait_ms, 30_000);
        assert_eq!(config.pool.queue_full_wait_ms, 5_000);
        assert_eq!(config.pool.low_credit_ratio, 0.0);
        assert_eq!(config.pool.low_credit_min_remaining, 7.0);
        assert_eq!(config.pool.credit_estimate_per_1k_tokens, 1.0);
        assert_eq!(config.pool.credit_estimate_output_token_cap, 8_192);
        assert!(config.pool.auto_switch_on_quota_exhausted);
        assert_eq!(config.pool.balance.weight_active, 0.5);
        assert_eq!(config.pool.balance.weight_credit, 0.4);
        assert_eq!(config.pool.balance.weight_idle, 0.1);
        assert_eq!(config.pool.balance.idle_window_ms, 300_000);
        assert_eq!(config.pool.cooldown.max_error_count, 3);
        assert_eq!(config.pool.cooldown.cooldown_ms, 30_000);
        assert_eq!(config.pool.cooldown.error_cooldown_ms, 5_000);
        assert_eq!(config.pool.cooldown.quota_reset_ms, 300_000);
        assert_eq!(config.pool.cooldown.quota_error_window_ms, 300_000);
        assert_eq!(config.pool.cooldown.quota_error_threshold, 50);

        assert_eq!(config.features.auto_continue_rounds, 0);
        assert!(config.features.enable_model_fallback);
        assert!(!config.features.enable_prompt_cache);
        assert!(config.features.enhance_system_prompt);
        assert!(config.features.buffer_tool_calls);
        assert_eq!(config.features.tool_call_buffer_delay_ms, 500);
        assert!(config.features.adaptive_thinking);
        assert_eq!(config.features.max_thinking_budget_tokens, 8192);
        assert!(config.features.enable_web_tools);
        assert!(config.features.enable_tool_leak_filter);

        assert_eq!(config.upstream.pool.stream_pipelining, 1);
        assert_eq!(config.upstream.pool.http_pipelining, 5);
        assert_eq!(config.upstream.pool.stream_max_connections, 256);
        assert_eq!(config.upstream.pool.http_max_connections, 128);
        assert_eq!(config.upstream.token_refresh_before_expiry, 900);
        assert_eq!(config.effective_token_refresh_before_expiry(), 900);
        assert_eq!(config.context.max_input_tokens, 200_000);
        assert_eq!(config.context.safe_input_ratio, 0.95);
        assert_eq!(config.context.compact_safe_input_ratio, 0.99);
        assert_eq!(config.notify.low_credit_threshold_percent, 10.0);
        assert_eq!(config.notify.max_notifications, 5);
        assert_eq!(config.storage.compression_threshold, 100);
        assert!(config.storage.incremental_write);
        assert_eq!(config.log.max_file_size_mb, 100);
        assert_eq!(config.log.retention_days, 3);
        assert!(config.model_mapping.is_empty());
        assert!(config.webhook.is_empty());
        assert!(config.api_key.is_empty());
        assert!(config.proxy_service.is_empty());
    }

    #[test]
    fn default_toml_parses_into_default_config() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("default toml must parse");
        let expected = Config::default();
        assert_eq!(parsed.server.port, expected.server.port);
        assert_eq!(
            parsed.pool.balance.weight_active,
            expected.pool.balance.weight_active
        );
        assert_eq!(
            parsed.features.auto_continue_rounds,
            expected.features.auto_continue_rounds
        );
        assert_eq!(
            parsed.upstream.pool.stream_pipelining,
            expected.upstream.pool.stream_pipelining
        );
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let parsed: Config = toml::from_str("").expect("empty toml must parse");
        assert_eq!(parsed.server.port, 5580);
        assert_eq!(parsed.pool.max_concurrent_per_account, 50);
    }

    #[test]
    fn token_refresh_lead_has_a_safe_runtime_floor() {
        let mut config = Config::default();
        config.upstream.token_refresh_before_expiry = 300;
        config.tasks.token_refresh_interval_ms = 300_000;
        assert_eq!(config.effective_token_refresh_before_expiry(), 600);

        config.tasks.token_refresh_interval_ms = 600_001;
        assert_eq!(config.effective_token_refresh_before_expiry(), 1_202);
    }

    #[test]
    fn model_mapping_schedule_accepts_documented_human_format() {
        let parsed: Config = toml::from_str(
            r#"
[[model_mapping]]
name = "office hours"
type = "replace"
source_models = ["claude-opus-4*"]
target_models = ["claude-sonnet-4"]
schedule = { start = "09:00", end = "18:00", days = ["mon", "tue"] }
"#,
        )
        .expect("documented schedule");
        let schedule = parsed.model_mapping[0].schedule.as_ref().expect("schedule");
        assert_eq!(schedule.start.as_deref(), Some("09:00"));
        assert_eq!(schedule.days.as_ref().expect("days")[0], "mon");
    }

    #[test]
    fn rejects_port_below_privileged_range() {
        let mut config = Config::default();
        config.server.port = 80;
        let error = config.validate().expect_err("port 80 must be rejected");
        assert!(error.to_string().contains("port"), "{error}");
    }

    #[test]
    fn rejects_non_local_host_without_api_key() {
        let mut config = Config::default();
        config.proxy_service.push(ProxyServiceConfig {
            id: "svc_test".into(),
            name: "test".into(),
            host: "0.0.0.0".into(),
            port: 5580,
            enabled: true,
            api_key_ids: vec!["ak_missing".into()],
            created_at: 0,
        });
        let error = config.validate().expect_err("public bind must fail");
        assert!(error.to_string().contains("API key"), "{error}");
    }

    #[test]
    fn accepts_non_local_host_with_enabled_api_key() {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "alice".into(),
            key: "sk-test".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        });
        config.proxy_service.push(ProxyServiceConfig {
            id: "svc_test".into(),
            name: "test".into(),
            host: "0.0.0.0".into(),
            port: 5580,
            enabled: true,
            api_key_ids: vec!["ak_test".into()],
            created_at: 0,
        });
        config.validate().expect("public bind with key must pass");
    }

    #[test]
    fn treats_loopback_hosts_as_local() {
        for host in ["localhost", "127.0.0.1", "::1", "[::1]"] {
            let mut config = Config::default();
            config.api_key.push(ApiKeyConfig {
                id: Some("ak_test".into()),
                name: "alice".into(),
                key: "sk-test".into(),
                format: ApiKeyFormat::Sk,
                enabled: true,
                credits_limit: None,
            });
            config.proxy_service.push(ProxyServiceConfig {
                id: "svc_test".into(),
                name: "test".into(),
                host: host.into(),
                port: 5580,
                enabled: true,
                api_key_ids: vec!["ak_test".into()],
                created_at: 0,
            });
            config
                .validate()
                .unwrap_or_else(|error| panic!("{host} should be local: {error}"));
        }
    }

    #[test]
    fn rejects_disabled_api_key_as_public_credential() {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "off".into(),
            key: "sk-test".into(),
            format: ApiKeyFormat::Sk,
            enabled: false,
            credits_limit: None,
        });
        config.proxy_service.push(ProxyServiceConfig {
            id: "svc_test".into(),
            name: "test".into(),
            host: "0.0.0.0".into(),
            port: 5580,
            enabled: true,
            api_key_ids: vec!["ak_test".into()],
            created_at: 0,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_ratios_outside_zero_to_one() {
        let mut config = Config::default();
        config.context.safe_input_ratio = 1.5;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_credit_estimation_coefficient() {
        let mut config = Config::default();
        config.pool.credit_estimate_per_1k_tokens = f64::NAN;
        assert!(config.validate().is_err());
        config.pool.credit_estimate_per_1k_tokens = -0.1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn socket_path_honours_development_environment() {
        assert_eq!(
            default_socket_path_from(Some("/tmp/kproxy"), None),
            "/tmp/kproxy/admin.sock"
        );
        assert_eq!(
            default_socket_path_from(None, Some("/run/user/1000")),
            "/run/user/1000/kproxy/admin.sock"
        );
        assert_eq!(default_socket_path_from(None, None), "/run/kproxy/admin.sock");
    }
}
