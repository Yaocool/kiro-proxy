//! 配置模型、校验规则与带注释的默认配置。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of immediately loaded tools accepted by the proxy.
pub const MAX_LOADED_TOOLS: usize = 512;

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

/// 基于代理内部排队与上游过载反馈的自适应并发。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdaptiveConfig {
    /// 是否启用。
    pub enabled: bool,
    /// 检查间隔。
    pub check_interval_ms: u64,
    /// 上游流式连接槽位等待 P99 超过此值时降低并发。
    pub p99_degrade_ms: u64,
    /// 上游流式连接槽位等待 P99 低于此值时恢复并发。
    pub p99_recover_ms: u64,
    /// 每次决策所需的最少上游调用样本数。
    pub minimum_samples: usize,
    /// 上游 429/503 占比达到此值时降低并发。
    pub overload_error_rate: f64,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_interval_ms: 10_000,
            p99_degrade_ms: 200,
            p99_recover_ms: 100,
            minimum_samples: 5,
            overload_error_rate: 0.05,
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
    /// Kiro MCP Web Search 端点。支持 `{region}` 占位符；空值使用
    /// `https://runtime.{region}.kiro.dev/mcp`。
    pub web_search_endpoint: Option<String>,
    /// MCP Web Search 请求超时。
    pub web_search_timeout_ms: u64,
    /// 等待流式连接槽位的最长时间。
    pub stream_slot_wait_timeout_ms: u64,
    /// 流式上游连续无响应数据的最长时间；不是请求总时长。
    pub stream_read_timeout_ms: u64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            preferred_endpoint: None,
            agent_mode: AgentMode::Vibe,
            max_retries: 3,
            token_refresh_before_expiry: 900,
            pool: UpstreamPoolConfig::default(),
            web_search_endpoint: None,
            web_search_timeout_ms: 60_000,
            stream_slot_wait_timeout_ms: 30_000,
            stream_read_timeout_ms: 600_000,
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
            low_credit_min_remaining: 4.0,
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
    /// 代理内部 Tool Search 的最大续轮次数（硬上限 8）。
    pub tool_search_max_rounds: u32,
    /// 单个客户端请求内允许实际执行的 Tool Search 操作数。
    pub tool_search_max_operations: u32,
    /// 是否启用原生 Anthropic Tool Search 模拟。
    pub enable_tool_search: bool,
    /// 单个客户端请求内代理执行 MCP Web Search 的安全上限。
    pub web_search_max_rounds: u32,
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
            tool_search_max_rounds: 4,
            tool_search_max_operations: 32,
            enable_tool_search: true,
            web_search_max_rounds: 20,
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
    /// 映射后的 Claude 模型窗口不足时，在首次上游调用前自动 compact。
    pub auto_compact_on_overflow: bool,
    /// deferred Tool Search 工作集中工具定义允许占用的最大估算 token。
    pub max_tool_input_tokens: u32,
    /// 单次 Kiro 请求中允许发送的已加载工具数量。
    pub max_loaded_tools: usize,
    /// 单次序列化 Kiro 请求的最大字节数。
    pub max_upstream_payload_bytes: usize,
    /// compact 摘要模型；留空时复用当轮映射后的模型。
    pub compaction_summary_model: String,
    /// 主链路等待 compact 摘要的超时；后台结算另有固定有界宽限期。
    pub compaction_summary_timeout_ms: u64,
    /// 当轮 compact 后额外保留的最近完整 user/assistant 轮数。
    pub compaction_preserve_recent_turns: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_input_tokens: 200_000,
            safe_input_ratio: 0.95,
            compact_safe_input_ratio: 0.99,
            auto_compact_on_overflow: false,
            max_tool_input_tokens: 32_000,
            max_loaded_tools: MAX_LOADED_TOOLS,
            max_upstream_payload_bytes: 8 * 1024 * 1024,
            compaction_summary_model: String::new(),
            compaction_summary_timeout_ms: 30_000,
            compaction_preserve_recent_turns: 3,
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
    /// 每个日志级别每天最多保留多少个分片，达到上限后丢弃该级别文件日志。
    pub max_files_per_day: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "json".into(),
            file_path: String::new(),
            max_file_size_mb: 100,
            retention_days: 3,
            max_files_per_day: 3,
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

/// IAM Identity Center SSO defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SsoConfig {
    /// Default IAM Identity Center start URL used by manual account login.
    pub start_url: String,
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
    /// IAM Identity Center SSO defaults.
    pub sso: SsoConfig,
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
        if self.context.max_loaded_tools > MAX_LOADED_TOOLS {
            return invalid_config(
                "context.max_loaded_tools",
                format!("must not exceed the proxy ceiling of {MAX_LOADED_TOOLS}"),
            );
        }
        if self.context.compaction_preserve_recent_turns > 64 {
            return invalid_config(
                "context.compaction_preserve_recent_turns",
                "must not exceed 64",
            );
        }
        if !(1..=256).contains(&self.features.tool_search_max_operations) {
            return invalid_config(
                "features.tool_search_max_operations",
                "must be between 1 and 256",
            );
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
        if self.upstream.stream_slot_wait_timeout_ms == 0
            || self.upstream.stream_read_timeout_ms == 0
        {
            return invalid_config(
                "upstream.stream_timeout",
                "slot wait and read timeout must be greater than zero",
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
                || self.server.adaptive.p99_degrade_ms == 0
                || self.server.adaptive.minimum_samples == 0
                || self.server.adaptive.minimum_samples > 10_000)
        {
            return invalid_config(
                "server.adaptive",
                "enabled adaptive admission requires a positive interval and degrade threshold, with sample count in 1..=10000",
            );
        }
        if !self.server.adaptive.overload_error_rate.is_finite()
            || !(0.0..=1.0).contains(&self.server.adaptive.overload_error_rate)
        {
            return invalid_config(
                "server.adaptive.overload_error_rate",
                "must be a finite number between 0 and 1",
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
        for (field, value) in [
            (
                "context.max_input_tokens",
                self.context.max_input_tokens as usize,
            ),
            (
                "context.max_tool_input_tokens",
                self.context.max_tool_input_tokens as usize,
            ),
            ("context.max_loaded_tools", self.context.max_loaded_tools),
            (
                "context.max_upstream_payload_bytes",
                self.context.max_upstream_payload_bytes,
            ),
        ] {
            if value == 0 {
                return invalid_config(field, "must be greater than zero");
            }
        }
        if self.context.compaction_summary_timeout_ms == 0 {
            return invalid_config(
                "context.compaction_summary_timeout_ms",
                "must be greater than zero",
            );
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
        if !self.sso.start_url.trim().is_empty()
            && !self.sso.start_url.trim().starts_with("https://")
        {
            return invalid_config("sso.start_url", "must use https://");
        }
        if self.log.max_file_size_mb == 0
            || self.log.retention_days == 0
            || self.log.max_files_per_day == 0
        {
            return invalid_config(
                "log",
                "max_file_size_mb, retention_days, and max_files_per_day must be positive",
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
pub const DEFAULT_CONFIG_TOML: &str = r#"# ============================================================================
# kiro-proxy 配置文件
# ============================================================================
#
# 本文件列出当前版本支持的全部配置。常用配置使用默认值生效；以 `#` 开头的
# 配置是可选示例，删除行首 `#` 后再按需修改。请勿把真实 API key、Webhook
# 密钥或 TLS 私钥提交到 Git。
#
# 除特别说明外：
# - `*_ms` 的单位是毫秒，`*_tokens` 的单位是 token，credits 支持小数。
# - daemon 会监听文件并热重载；仅 `admin.socket` 和 TLS enabled 模式切换需重启。
# - 修改已有服务/API key/模型映射时，优先使用 `kproxy service`、
#   `kproxy apikey`、`kproxy model-map` 等命令，以免写错关联 ID。

# ----------------------------------------------------------------------------
# API 服务共享配置
# ----------------------------------------------------------------------------
# 这里的 host/port 是 `kproxy service create` 的默认值；实际监听实例记录在
# `[[proxy_service]]` 中。首次启动时服务列表为空，因此 daemon 只启动管理面。
[server]
# 新建 API 代理服务的默认监听地址。0.0.0.0 表示监听所有 IPv4 网卡。
host = "0.0.0.0"
# 新建 API 代理服务的默认端口；允许范围 1024-65535。
port = 5580
# 是否校验 Claude 路由的客户端 User-Agent；不影响 OpenAI Chat Completions。
enforce_user_agent_check = true
# 所有代理服务共享的请求准入上限；超出时返回 503 和 Retry-After。
max_concurrent_requests = 500
# 下游 HTTP keep-alive 空闲超时。
keep_alive_timeout_ms = 30000
# 所有代理服务允许同时保持的最大下游连接数。
max_connections = 2000

# TLS 默认关闭。启用时必须配置一组证书来源：文件路径 cert_path/key_path，
# 或内联 PEM cert/key。不要同时混用两组。切换 enabled 后需重启 daemon。
[server.tls]
# 是否让代理服务直接提供 HTTPS。
enabled = false
# PEM 证书链文件路径。
# cert_path = "/etc/kproxy/fullchain.pem"
# PEM 私钥文件路径。
# key_path = "/etc/kproxy/privkey.pem"
# 内联 PEM 证书；适合由密钥管理系统生成配置的场景。
# cert = "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----"
# 内联 PEM 私钥；配置文件权限应保持为 0600。
# key = "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----"

# 自适应并发根据上游连接等待延迟和 429/503 比例动态收紧/恢复全局准入。
# 默认关闭；固定并发更容易预测，确认有持续高并发流量后再开启。
[server.adaptive]
# 是否启用自适应并发控制。
enabled = false
# 聚合样本并做一次升降并发决策的周期。
check_interval_ms = 10000
# 上游流式连接槽位等待 P99 达到该值时降低并发。
p99_degrade_ms = 200
# P99 低于该值时逐步恢复并发；不得高于 p99_degrade_ms。
p99_recover_ms = 100
# 每次决策至少需要的上游调用样本数；低流量时样本跨周期累计。
minimum_samples = 5
# 上游 429/503 占比达到该值时降低并发；范围 0.0-1.0。
overload_error_rate = 0.05

# ----------------------------------------------------------------------------
# Kiro 上游请求与连接池
# ----------------------------------------------------------------------------
[upstream]
# 强制首选 Kiro 上游端点；可选值："codewhisperer"、"amazonq"。
# 不配置时按账号类型和已探测结果自动选择。
# preferred_endpoint = "amazonq"
# Kiro agent mode；可选值："auto"、"vibe"、"spec"。
agent_mode = "vibe"
# 一次请求失败后允许的重试次数；实际总尝试次数还受可用账号数限制。
max_retries = 3
# 期望在 token 到期前多少秒刷新。运行时至少取扫描间隔的 2 倍和 10 分钟。
token_refresh_before_expiry = 900
# Kiro MCP Web Search 地址，支持 `{region}` 占位符；不配置时使用下面的默认形式。
# web_search_endpoint = "https://runtime.{region}.kiro.dev/mcp"
# 单次 MCP Web Search 请求超时。
web_search_timeout_ms = 60000
# 等待上游流式连接槽位的最长时间；超时后本次尝试失败。
stream_slot_wait_timeout_ms = 30000
# 流式上游连续无数据的最长时间；不是整个流的总时长限制。
stream_read_timeout_ms = 600000

# 上游使用独立的非流式和流式连接池，避免长连接阻塞普通 HTTP 请求。
[upstream.pool]
# 非流式请求连接池的最大连接数。
http_max_connections = 128
# 每条非流式 HTTP/1.1 连接允许的流水线深度。
http_pipelining = 5
# 流式请求连接池的最大连接数。
stream_max_connections = 256
# 必须为 1，让每个流独占连接，避免队首阻塞。
stream_pipelining = 1
# 上游连接空闲超过该时间后允许回收。
keep_alive_idle_ms = 30000
# 单条上游连接的最大寿命，降低长期连接失效风险。
keep_alive_max_ms = 60000

# ----------------------------------------------------------------------------
# 账号池、排队、额度保护与选号
# ----------------------------------------------------------------------------
[pool]
# 每个 Kiro 账号允许同时处理的最大请求数。
max_concurrent_per_account = 50
# 所有账号都繁忙时，等待队列最多容纳的请求数。
max_queue_size = 10
# 已进入队列的请求最长等待时间。
max_queue_wait_ms = 30000
# 队列已满后，为短暂释放槽位额外等待的时间。
queue_full_wait_ms = 5000
# 按剩余额度百分比停用账号；范围 0.0-1.0，0 表示关闭百分比保护。
low_credit_ratio = 0.0
# 按剩余 credits 绝对值停用账号；剩余值小于等于该值时不再分配请求，0 表示关闭。
low_credit_min_remaining = 4.0
# 整个代理服务每天最多消耗的 credits；0 表示不限，按 UTC 自然日重置。
daily_credit_limit = 0.0
# 上游返回真实 usage 前，每 1,000 个估算 token 预留的 credits。
credit_estimate_per_1k_tokens = 1.0
# 估算预留额度时最多计入多少输出 token，避免超大 max_tokens 过度预留。
credit_estimate_output_token_cap = 8192
# 遇到额度耗尽错误时是否自动尝试其他可用账号。
auto_switch_on_quota_exhausted = true

# 选号分数由并发压力、额度消耗和最近使用三个权重组成。
# 权重无需相加为 1，但至少一个必须大于 0。
[pool.balance]
# 当前活跃请求数的权重。
weight_active = 0.5
# 已用额度比例的权重。
weight_credit = 0.4
# 最近使用时间的权重，鼓励轮换账号。
weight_idle = 0.1
# 将账号空闲时长归一化的窗口。
idle_window_ms = 300000

# 账号错误冷却和额度恢复探测。
[pool.cooldown]
# 连续错误达到该次数后进入一般冷却。
max_error_count = 3
# 一般冷却持续时间。
cooldown_ms = 30000
# 单次可恢复错误触发的短冷却时间。
error_cooldown_ms = 5000
# 账号被标记额度耗尽后，重新探测额度的间隔。
quota_reset_ms = 300000
# 统计额度类错误的滑动窗口长度。
quota_error_window_ms = 300000
# 滑动窗口内达到该错误数后，将账号标记为额度耗尽。
quota_error_threshold = 50

# ----------------------------------------------------------------------------
# 协议转换与工具功能
# ----------------------------------------------------------------------------
[features]
# 普通工具调用结束后由代理自动续轮的最大次数；0 表示不自动续轮，硬上限 30。
auto_continue_rounds = 0
# Anthropic Tool Search 一次搜索工作流允许的最大内部续轮数；运行时限制为 1-8。
tool_search_max_rounds = 4
# 单个客户端请求内实际执行 Tool Search 操作的总上限；允许范围 1-256。
tool_search_max_operations = 32
# 是否模拟 Anthropic 原生 Tool Search，让大量工具按需加载。
enable_tool_search = true
# 单个客户端请求内代理最多执行多少次 MCP Web Search。
web_search_max_rounds = 20
# 是否移除请求中的全部工具定义；用于临时排查工具兼容问题。
disable_tools = false
# 遇到 429 时是否自动选择同族可用模型降级。
enable_model_fallback = true
# 是否模拟 prompt cache 计费字段；不代表 Kiro 提供真实 Anthropic cache。
enable_prompt_cache = false
# 是否向上游 system prompt 注入协议兼容和工具使用说明。
enhance_system_prompt = true
# 是否缓冲流式工具调用片段，降低参数 JSON 被拆碎或乱序的概率。
buffer_tool_calls = true
# 工具调用缓冲等待时间；仅在 buffer_tool_calls=true 时使用。
tool_call_buffer_delay_ms = 500
# 思考内容输出格式；"claude" 输出 thinking block，"openai" 输出 reasoning_content。
thinking_output_format = "claude"
# 是否根据请求和模型能力自动启用或调整 thinking。
adaptive_thinking = true
# 上游未给出有效预算时使用的 thinking token 上限。
max_thinking_budget_tokens = 8192
# 是否转换 Claude/OpenAI 的 Web Search 工具并交给 Kiro MCP 执行。
enable_web_tools = true
# 是否过滤模型误输出的内部工具协议文本。
enable_tool_leak_filter = true
# 客户端未指定模型或模型解析需要兜底时使用的模型 ID；空字符串表示自动选择。
default_model_id = ""

# ----------------------------------------------------------------------------
# 模型发现和后台任务
# ----------------------------------------------------------------------------
[models]
# 是否从 Kiro 上游动态发现可用模型。
dynamic_discovery = true
# 模型发现结果的缓存时间；启用动态发现时必须大于 0。
cache_ttl_ms = 3600000

[tasks]
# 扫描即将过期 token 的周期。
token_refresh_interval_ms = 300000
# 刷新账号额度、订阅与健康状态的周期。
status_check_interval_ms = 60000
# 将请求统计和 credits 用量写入磁盘的周期。
stats_persist_interval_ms = 60000

# ----------------------------------------------------------------------------
# 上下文窗口与自动 compact
# ----------------------------------------------------------------------------
[context]
# 未发现模型元数据时使用的默认最大输入 token 数。
max_input_tokens = 200000
# 普通请求允许使用模型上下文窗口的比例；范围 0.0-1.0。
safe_input_ratio = 0.95
# compact 请求允许使用模型上下文窗口的比例；范围 0.0-1.0。
compact_safe_input_ratio = 0.99
# 模型映射后上下文超限时，是否在首次生成调用前自动摘要压缩。
auto_compact_on_overflow = false
# deferred Tool Search 工作集中，工具定义允许占用的最大估算 token 数。
max_tool_input_tokens = 32000
# 单次请求可直接发送给 Kiro 的已加载工具数量；不得超过程序硬上限 512。
max_loaded_tools = 512
# 单次序列化 Kiro 请求体的最大字节数。
max_upstream_payload_bytes = 8388608
# compact 摘要使用的模型；空字符串表示复用当轮映射后的模型。
compaction_summary_model = ""
# 主请求等待 compact 摘要结果的最长时间。
compaction_summary_timeout_ms = 30000
# compact 摘要之外额外保留的最近完整 user/assistant 轮数；最大 64。
compaction_preserve_recent_turns = 3

# ----------------------------------------------------------------------------
# 本地存储
# ----------------------------------------------------------------------------
[storage]
# 账号数超过该值时，将账号库基线编码为压缩格式以减少磁盘占用。
compression_threshold = 100
# 是否只追加账号变更并在达到阈值后压实；关闭时每次保存完整重写账号库。
incremental_write = true

# ----------------------------------------------------------------------------
# 告警节流
# ----------------------------------------------------------------------------
[notify]
# 账号剩余额度百分比低于该值时开始告警；范围 0-100，0 表示关闭低额度告警。
low_credit_threshold_percent = 10.0
# 从初始阈值递进到 0 期间最多发送的低额度告警档位数。
max_notifications = 5
# 非额度类相同事件的重复通知抑制窗口。
suppress_window_ms = 1800000

# ----------------------------------------------------------------------------
# 日志
# ----------------------------------------------------------------------------
[log]
# tracing 过滤级别，例如 "error"、"warn"、"info"、"debug" 或模块级过滤表达式。
level = "info"
# 文件日志格式；可选值："json"、"pretty"。
format = "json"
# 日志基础文件路径；空字符串使用数据目录下的 logs/kproxyd.log。
file_path = ""
# 每个日志级别、每个分片的最大文件大小（MiB）。
max_file_size_mb = 100
# 按 UTC 日期保留最近多少天的日志文件。
retention_days = 3
# 每个日志级别每天最多保留的分片数；达到上限后丢弃该级别的新增文件日志。
max_files_per_day = 3

# ----------------------------------------------------------------------------
# 本机管理面
# ----------------------------------------------------------------------------
[admin]
# CLI 与 daemon 通信的 Unix socket。首次生成时会按 KPROXY_HOME/XDG_RUNTIME_DIR 改写。
# 修改后必须重启 daemon；容器 wrapper 会使用同一数据目录解析该路径。
socket = "/run/kproxy/admin.sock"

# ----------------------------------------------------------------------------
# IAM Identity Center SSO
# ----------------------------------------------------------------------------
[sso]
# `account add-sso` 未传 --start-url 时使用的默认入口；必须是 https:// URL。
# 留空表示每次命令都必须显式传入 --start-url。
start_url = ""

# ----------------------------------------------------------------------------
# 模型级 thinking 开关
# ----------------------------------------------------------------------------
# key 为模型 ID 或模型族前缀，value 为是否允许 thinking。未配置的模型默认为 true。
# 更精确的完整模型 ID 应避免与宽泛前缀产生歧义。
[model_thinking_mode]
# "claude-sonnet-4.6" = true
# "claude-haiku" = false

# ----------------------------------------------------------------------------
# API key 与代理服务（可重复数组，默认均为空）
# ----------------------------------------------------------------------------
# 推荐使用 `kproxy service create` 和 `kproxy apikey` 命令维护。首次启动不创建
# 代理监听；`kproxy service create --name main --port 5580` 会生成服务及首个 key。

# 单个 API key 示例。每增加一个 key，就增加一个 `[[api_key]]` 块。
# [[api_key]]
# 稳定 ID，供 proxy_service.api_key_ids 和模型映射引用；必须非空且唯一。
# id = "ak_example"
# 可读名称；必须唯一。
# name = "alice"
# 客户端请求时提交的密钥；必须唯一，配置文件权限应保持为 0600。
# key = "sk-replace-me"
# 密钥格式；可选值："sk"、"simple"、"token"。
# format = "sk"
# 是否允许该 key 认证请求。
# enabled = true
# 该 key 的累计 credits 上限；不配置表示不限，0 会禁止产生任何新消耗。
# credits_limit = 5000.0

# 单个代理监听实例示例。每增加一个服务，就增加一个 `[[proxy_service]]` 块。
# [[proxy_service]]
# 稳定服务 ID；必须非空且唯一。
# id = "svc_example"
# 可读名称；必须唯一。
# name = "main"
# 实际监听地址；非本机地址必须绑定至少一个已启用 API key。
# host = "0.0.0.0"
# 实际监听端口；允许范围 1024-65535，启用的服务不得重复 host+port。
# port = 5580
# 是否随 daemon 启动该监听实例。
# enabled = true
# 允许访问该服务的 API key ID；至少一个，且都必须存在于 [[api_key]]。
# api_key_ids = ["ak_example"]
# 创建时间（Unix 秒）；由 CLI 创建时自动填写。
# created_at = 0

# ----------------------------------------------------------------------------
# 模型映射（可重复数组，默认为空）
# ----------------------------------------------------------------------------
# 推荐使用 `kproxy model-map` 命令维护。规则按 priority 从小到大匹配，首个
# 满足模型、API key、剩余额度和时间窗条件的规则生效。

# [[model_mapping]]
# 规则名称；用于 CLI 管理和日志定位。
# name = "opus 降级到 sonnet"
# 是否启用该规则。
# enabled = true
# 规则类型："replace"、"alias" 或 "loadbalance"。
# type = "replace"
# 要匹配的客户端模型 glob 列表。
# source_models = ["claude-opus-4*"]
# 映射后的目标模型列表；replace/alias 通常配置一个，loadbalance 可配置多个。
# target_models = ["claude-sonnet-4.6"]
# 数字越小优先级越高。
# priority = 10
# loadbalance 各目标的权重；数量必须与 target_models 相同，且至少一个大于 0。
# weights = [100]
# 仅当所选账号的剩余额度百分比低于该值时生效；范围 0-100。
# max_remaining_credit_percent = 10.0
# 仅对这些 API key ID 生效；不配置表示不限制 key。
# api_key_ids = ["ak_example"]

# 可选生效时间窗，隶属于上方最近一个 [[model_mapping]]。
# [model_mapping.schedule]
# 模式："always"、"daily" 或 "range"。
# mode = "daily"
# daily 的星期条件，0=周日、1=周一、...、6=周六；与 days 二选一。
# days_of_week = [1, 2, 3, 4, 5]
# daily 的可读星期条件；支持 sun/mon/tue/wed/thu/fri/sat。
# days = ["mon", "tue", "wed", "thu", "fri"]
# daily 的开始分钟数（从 00:00 起算）；与 start 二选一。
# start_minutes = 540
# daily 的可读开始时间，HH:MM。
# start = "09:00"
# daily 的结束分钟数；与 end 二选一。
# end_minutes = 1080
# daily 的可读结束时间，HH:MM。
# end = "18:00"
# range 的开始时间（Unix 毫秒）。
# start_at = 1787587200000
# range 的结束时间（Unix 毫秒）。
# end_at = 1787673600000

# ----------------------------------------------------------------------------
# Webhook 告警目标（可重复数组，默认为空）
# ----------------------------------------------------------------------------
# 推荐使用 `kproxy alert` 命令维护。每增加一个目标，就增加一个 `[[webhook]]` 块。

# [[webhook]]
# 可读名称；必须唯一。
# name = "运维群"
# 类型："dingtalk"、"wechat-work"、"telegram"、"discord"、"feishu"、"custom"。
# kind = "dingtalk"
# Webhook 接收地址；启用时必须使用 http:// 或 https://。
# url = "https://oapi.dingtalk.com/robot/send?access_token=replace-me"
# 是否启用该目标。
# enabled = true
# 订阅事件；可选 low-credit、account-banned、token-expired、quota-exhausted、
# service-degraded。空数组表示不订阅任何事件。
# events = ["low-credit", "account-banned", "token-expired", "quota-exhausted", "service-degraded"]
# 钉钉加签密钥；仅 kind="dingtalk" 且机器人开启加签时需要。
# dingtalk_sign = "SEC-replace-me"
# Telegram chat ID；kind="telegram" 且目标启用时必填。
# telegram_chat_id = "123456789"
# custom JSON/文本模板；支持 {{event}}、{{title}}、{{message}} 占位符。
# custom_template = '{"event":"{{event}}","title":"{{title}}","message":"{{message}}"}'
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
        assert!(!config.server.adaptive.enabled);
        assert_eq!(config.server.adaptive.minimum_samples, 5);
        assert_eq!(config.server.adaptive.overload_error_rate, 0.05);

        assert_eq!(config.pool.max_concurrent_per_account, 50);
        assert_eq!(config.pool.max_queue_size, 10);
        assert_eq!(config.pool.max_queue_wait_ms, 30_000);
        assert_eq!(config.pool.queue_full_wait_ms, 5_000);
        assert_eq!(config.pool.low_credit_ratio, 0.0);
        assert_eq!(config.pool.low_credit_min_remaining, 4.0);
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
        assert_eq!(config.features.tool_search_max_rounds, 4);
        assert_eq!(config.features.tool_search_max_operations, 32);
        assert!(config.features.enable_tool_search);
        assert_eq!(config.features.web_search_max_rounds, 20);

        assert_eq!(config.upstream.pool.stream_pipelining, 1);
        assert_eq!(config.upstream.pool.http_pipelining, 5);
        assert_eq!(config.upstream.pool.stream_max_connections, 256);
        assert_eq!(config.upstream.pool.http_max_connections, 128);
        assert_eq!(config.upstream.token_refresh_before_expiry, 900);
        assert_eq!(config.upstream.web_search_timeout_ms, 60_000);
        assert_eq!(config.upstream.stream_slot_wait_timeout_ms, 30_000);
        assert_eq!(config.upstream.stream_read_timeout_ms, 600_000);
        assert!(config.upstream.web_search_endpoint.is_none());
        assert_eq!(config.effective_token_refresh_before_expiry(), 900);
        assert_eq!(config.context.max_input_tokens, 200_000);
        assert_eq!(config.context.safe_input_ratio, 0.95);
        assert_eq!(config.context.compact_safe_input_ratio, 0.99);
        assert!(!config.context.auto_compact_on_overflow);
        assert_eq!(config.context.max_tool_input_tokens, 32_000);
        assert_eq!(config.context.max_loaded_tools, MAX_LOADED_TOOLS);
        assert_eq!(config.context.max_upstream_payload_bytes, 8 * 1024 * 1024);
        assert!(config.context.compaction_summary_model.is_empty());
        assert_eq!(config.context.compaction_summary_timeout_ms, 30_000);
        assert_eq!(config.context.compaction_preserve_recent_turns, 3);
        assert_eq!(config.notify.low_credit_threshold_percent, 10.0);
        assert_eq!(config.notify.max_notifications, 5);
        assert_eq!(config.storage.compression_threshold, 100);
        assert!(config.storage.incremental_write);
        assert_eq!(config.log.max_file_size_mb, 100);
        assert_eq!(config.log.retention_days, 3);
        assert_eq!(config.log.max_files_per_day, 3);
        assert!(config.model_mapping.is_empty());
        assert!(config.sso.start_url.is_empty());
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
    fn documented_default_covers_every_config_field() {
        let documented: toml::Value = toml::from_str(&uncomment_documented_settings())
            .expect("all documented settings must form valid TOML");
        let expected = fully_populated_config();
        let serialized = toml::to_string(&expected).expect("serialize populated config");
        let expected: toml::Value = toml::from_str(&serialized).expect("parse populated config");

        assert_same_config_shape(&expected, &documented, "config");
    }

    fn uncomment_documented_settings() -> String {
        DEFAULT_CONFIG_TOML
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                let setting = trimmed.strip_prefix("# ").unwrap_or(trimmed);
                let is_table = setting.starts_with('[') && setting.ends_with(']');
                let is_value = setting.contains(" = ");
                (is_table || is_value).then_some(setting)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fully_populated_config() -> Config {
        let mut config = Config::default();
        config.server.tls.cert_path = Some("/cert.pem".into());
        config.server.tls.key_path = Some("/key.pem".into());
        config.server.tls.cert = Some("certificate".into());
        config.server.tls.key = Some("private-key".into());
        config.upstream.preferred_endpoint = Some(Endpoint::Amazonq);
        config.upstream.web_search_endpoint = Some("https://example.com/mcp".into());
        config.model_mapping.push(ModelMappingRule {
            name: "mapping".into(),
            enabled: true,
            kind: "replace".into(),
            source_models: vec!["source".into()],
            target_models: vec!["target".into()],
            priority: 1,
            weights: Some(vec![1]),
            max_remaining_credit_percent: Some(10.0),
            api_key_ids: Some(vec!["ak_example".into()]),
            schedule: Some(ModelMappingSchedule {
                mode: "daily".into(),
                days_of_week: Some(vec![1]),
                days: Some(vec!["mon".into()]),
                start_minutes: Some(540),
                start: Some("09:00".into()),
                end_minutes: Some(1080),
                end: Some("18:00".into()),
                start_at: Some(1),
                end_at: Some(2),
            }),
        });
        config.webhook.push(WebhookConfig {
            name: "webhook".into(),
            kind: "custom".into(),
            url: "https://example.com/webhook".into(),
            enabled: true,
            events: vec!["low-credit".into()],
            dingtalk_sign: Some("secret".into()),
            telegram_chat_id: Some("123".into()),
            custom_template: Some("template".into()),
        });
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_example".into()),
            name: "example".into(),
            key: "sk-example".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: Some(100.0),
        });
        config.proxy_service.push(ProxyServiceConfig {
            id: "svc_example".into(),
            name: "example".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            api_key_ids: vec!["ak_example".into()],
            created_at: 1,
        });
        config
            .model_thinking_mode
            .insert("claude-sonnet-4.6".into(), true);
        config
            .model_thinking_mode
            .insert("claude-haiku".into(), false);
        config
    }

    fn assert_same_config_shape(expected: &toml::Value, actual: &toml::Value, path: &str) {
        match (expected, actual) {
            (toml::Value::Table(expected), toml::Value::Table(actual)) => {
                let expected_keys = expected.keys().collect::<BTreeSet<_>>();
                let actual_keys = actual.keys().collect::<BTreeSet<_>>();
                assert_eq!(
                    actual_keys, expected_keys,
                    "documented fields differ at {path}"
                );
                for (key, expected) in expected {
                    assert_same_config_shape(
                        expected,
                        actual.get(key).expect("matching key checked above"),
                        &format!("{path}.{key}"),
                    );
                }
            }
            (toml::Value::Array(expected), toml::Value::Array(actual)) => {
                assert!(!actual.is_empty(), "documented array is empty at {path}");
                if let Some(expected) = expected.first() {
                    assert_same_config_shape(
                        expected,
                        actual.first().expect("non-empty documented array"),
                        &format!("{path}[0]"),
                    );
                }
            }
            (expected, actual) => assert_eq!(
                std::mem::discriminant(expected),
                std::mem::discriminant(actual),
                "value type differs at {path}"
            ),
        }
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let parsed: Config = toml::from_str("").expect("empty toml must parse");
        assert_eq!(parsed.server.port, 5580);
        assert_eq!(parsed.pool.max_concurrent_per_account, 50);
    }

    #[test]
    fn stream_and_log_resource_limits_must_be_positive() {
        let mut config = Config::default();
        config.upstream.stream_slot_wait_timeout_ms = 0;
        assert!(config
            .validate()
            .expect_err("zero stream timeout")
            .to_string()
            .contains("upstream.stream_timeout"));

        let mut config = Config::default();
        config.log.max_files_per_day = 0;
        assert!(config
            .validate()
            .expect_err("zero log file limit")
            .to_string()
            .contains("log"));
    }

    #[test]
    fn loaded_tool_limit_cannot_exceed_the_proxy_ceiling() {
        let mut config = Config::default();
        config.context.max_loaded_tools = MAX_LOADED_TOOLS + 1;
        let error = config
            .validate()
            .expect_err("limits above the proxy ceiling must be rejected");
        assert!(
            error.to_string().contains("context.max_loaded_tools"),
            "{error}"
        );
    }

    #[test]
    fn compaction_summary_limits_are_validated() {
        let mut config = Config::default();
        config.context.compaction_summary_timeout_ms = 0;
        let error = config
            .validate()
            .expect_err("zero compaction summary timeout must be rejected");
        assert!(
            error
                .to_string()
                .contains("context.compaction_summary_timeout_ms"),
            "{error}"
        );

        let mut config = Config::default();
        config.context.compaction_preserve_recent_turns = 65;
        let error = config
            .validate()
            .expect_err("unbounded retained compact turns must be rejected");
        assert!(
            error
                .to_string()
                .contains("context.compaction_preserve_recent_turns"),
            "{error}"
        );
    }

    #[test]
    fn tool_search_operation_limit_is_positive_and_bounded() {
        for value in [0, 257] {
            let mut config = Config::default();
            config.features.tool_search_max_operations = value;
            let error = config
                .validate()
                .expect_err("unsafe Tool Search operation limit must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("features.tool_search_max_operations"),
                "{error}"
            );
        }
    }

    #[test]
    fn sso_start_url_requires_https_when_configured() {
        let mut config = Config::default();
        config.sso.start_url = "http://example.awsapps.com/start".into();
        let error = config
            .validate()
            .expect_err("HTTP SSO URL must be rejected");
        assert!(error.to_string().contains("sso.start_url"), "{error}");

        config.sso.start_url = "https://example.awsapps.com/start".into();
        config.validate().expect("HTTPS SSO URL must be accepted");
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

        let mut config = Config::default();
        config.server.adaptive.overload_error_rate = f64::NAN;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.server.adaptive.overload_error_rate = 1.1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn adaptive_feedback_sample_count_is_positive_and_bounded() {
        for minimum_samples in [0, 10_001] {
            let mut config = Config::default();
            config.server.adaptive.enabled = true;
            config.server.adaptive.minimum_samples = minimum_samples;
            let error = config
                .validate()
                .expect_err("unsafe adaptive sample count must be rejected");
            assert!(error.to_string().contains("server.adaptive"), "{error}");
        }
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
        assert_eq!(
            default_socket_path_from(None, None),
            "/run/kproxy/admin.sock"
        );
    }
}
