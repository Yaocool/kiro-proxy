//! Model mapping and adaptive-thinking decisions.

use kproxy_core::config::{ModelMappingRule, ModelMappingSchedule};
use rand::distributions::{Distribution, WeightedIndex};

/// Resolve a client model name against the exact IDs returned by one account.
/// Matching mirrors the TypeScript authority: exact, normalized/prefix, then
/// highest version in the same model family.
pub fn resolve_dynamic_model(model: &str, available: &[String]) -> Option<String> {
    if available.is_empty() {
        return None;
    }
    if let Some(exact) = available
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(model))
    {
        return Some(exact.clone());
    }
    let requested = normalize_model_name(model);
    if let Some(exact) = available
        .iter()
        .find(|candidate| normalize_model_name(candidate) == requested)
    {
        return Some(exact.clone());
    }
    let prefixed = available
        .iter()
        .filter(|candidate| {
            let normalized = normalize_model_name(candidate);
            prefix_version_match(&requested, &normalized)
                || prefix_version_match(&normalized, &requested)
        })
        .max_by(
            |left, right| match (model_family(left), model_family(right)) {
                (Some((left_family, left_version)), Some((right_family, right_version)))
                    if left_family == right_family =>
                {
                    compare_version(&left_version, &right_version)
                }
                _ => normalize_model_name(left).cmp(&normalize_model_name(right)),
            },
        );
    if let Some(candidate) = prefixed {
        return Some(candidate.clone());
    }
    // Some clients put the Claude family after the version
    // (`claude-4.6-sonnet`) while Kiro returns it before the version
    // (`claude-sonnet-4.6`). Treat separators and token order as aliases of
    // the same discovered model, but keep family isolation so Opus, Sonnet,
    // and Haiku can never resolve across one another.
    if let Some(candidate) = available
        .iter()
        .filter(|candidate| token_model_match(model, candidate))
        .max_by(|left, right| compare_token_matches(left, right))
    {
        return Some(candidate.clone());
    }
    let family = model_family(model)?;
    available
        .iter()
        .filter_map(|candidate| {
            let (candidate_family, version) = model_family(candidate)?;
            (candidate_family == family.0).then_some((version, candidate))
        })
        .max_by(|left, right| compare_version(&left.0, &right.0))
        .map(|(_, candidate)| candidate.clone())
}

fn model_tokens(model: &str) -> Vec<String> {
    model
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty() && *token != "latest" && *token != "model")
        .map(str::to_owned)
        .collect()
}

fn token_model_match(requested: &str, candidate: &str) -> bool {
    const CLAUDE_FAMILIES: [&str; 3] = ["opus", "sonnet", "haiku"];
    let requested = model_tokens(requested);
    let candidate = model_tokens(candidate);
    if requested.is_empty() || !requested.iter().all(|token| candidate.contains(token)) {
        return false;
    }
    CLAUDE_FAMILIES.iter().all(|family| {
        requested.iter().any(|token| token == family)
            == candidate.iter().any(|token| token == family)
    })
}

fn compare_token_matches(left: &str, right: &str) -> std::cmp::Ordering {
    match (model_family(left), model_family(right)) {
        (Some((left_family, left_version)), Some((right_family, right_version)))
            if left_family == right_family =>
        {
            compare_version(&left_version, &right_version)
                .then_with(|| right.len().cmp(&left.len()))
                .then_with(|| left.cmp(right))
        }
        _ => right.len().cmp(&left.len()).then_with(|| left.cmp(right)),
    }
}

pub fn can_resolve_dynamic_model(model: &str, available: &[String]) -> bool {
    resolve_dynamic_model(model, available).is_some()
}

fn normalize_model_name(model: &str) -> String {
    model
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '.'], "-")
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn prefix_version_match(longer: &str, shorter: &str) -> bool {
    let Some(rest) = longer.strip_prefix(shorter) else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('-')
            .and_then(|rest| rest.chars().next())
            .is_some_and(|next| next.is_ascii_digit())
}

fn model_family(model: &str) -> Option<(String, Vec<u32>)> {
    let normalized = normalize_model_name(model);
    let parts = normalized.split('-').collect::<Vec<_>>();
    let first_version = parts
        .iter()
        .position(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))?;
    if first_version == 0 {
        return None;
    }
    let family = parts[..first_version].join("-");
    let version = parts[first_version..]
        .iter()
        .take_while(|part| part.chars().all(|ch| ch.is_ascii_digit()))
        .filter_map(|part| part.parse().ok())
        .collect::<Vec<_>>();
    (!version.is_empty()).then_some((family, version))
}

fn compare_version(left: &[u32], right: &[u32]) -> std::cmp::Ordering {
    (0..left.len().max(right.len()))
        .map(|index| {
            left.get(index)
                .copied()
                .unwrap_or_default()
                .cmp(&right.get(index).copied().unwrap_or_default())
        })
        .find(|ordering| !ordering.is_eq())
        .unwrap_or(std::cmp::Ordering::Equal)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    pub original: String,
    pub mapped: String,
    pub rule: Option<String>,
}

pub fn map_model(
    model: &str,
    rules: &[ModelMappingRule],
    api_key_id: Option<&str>,
    remaining_percent: Option<f64>,
    default_model: &str,
) -> ModelRoute {
    let mut ordered = rules.iter().filter(|rule| rule.enabled).collect::<Vec<_>>();
    ordered.sort_by_key(|rule| rule.priority);
    for rule in ordered {
        if !schedule_active(rule.schedule.as_ref()) {
            continue;
        }
        if !rule
            .source_models
            .iter()
            .any(|pattern| glob(pattern, model))
        {
            continue;
        }
        if let Some(ids) = &rule.api_key_ids {
            if !ids.is_empty() && !api_key_id.is_some_and(|id| ids.iter().any(|item| item == id)) {
                continue;
            }
        }
        if let Some(maximum) = rule.max_remaining_credit_percent {
            if remaining_percent.is_none_or(|value| value >= maximum) {
                continue;
            }
        }
        if let Some(mapped) = choose_target(rule) {
            return ModelRoute {
                original: model.into(),
                mapped,
                rule: Some(rule.name.clone()),
            };
        }
    }
    ModelRoute {
        original: model.into(),
        mapped: if default_model.trim().is_empty() {
            model.into()
        } else {
            default_model.into()
        },
        rule: None,
    }
}

fn schedule_active(schedule: Option<&ModelMappingSchedule>) -> bool {
    let Some(schedule) = schedule else {
        return true;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let (day, minutes) = local_day_and_minutes(now / 1_000);
    schedule_active_at(schedule, now, day, minutes)
}

fn schedule_active_at(
    schedule: &ModelMappingSchedule,
    timestamp_ms: i64,
    day: u8,
    minutes: u16,
) -> bool {
    let mode = if schedule.mode == "always"
        && (schedule.start.is_some()
            || schedule.end.is_some()
            || schedule.start_minutes.is_some()
            || schedule.end_minutes.is_some()
            || schedule.days.is_some()
            || schedule.days_of_week.is_some())
    {
        "daily"
    } else if schedule.mode == "always"
        && (schedule.start_at.is_some() || schedule.end_at.is_some())
    {
        "range"
    } else {
        schedule.mode.as_str()
    };
    match mode {
        "range" => {
            schedule.start_at.is_none_or(|start| timestamp_ms >= start)
                && schedule.end_at.is_none_or(|end| timestamp_ms <= end)
        }
        "daily" => {
            let configured_days = schedule.days_of_week.clone().or_else(|| {
                schedule.days.as_ref().map(|days| {
                    days.iter()
                        .filter_map(|day| parse_weekday(day))
                        .collect::<Vec<_>>()
                })
            });
            if configured_days
                .as_ref()
                .is_some_and(|days| !days.is_empty() && !days.contains(&day))
            {
                return false;
            }
            let start = schedule
                .start_minutes
                .or_else(|| schedule.start.as_deref().and_then(parse_clock));
            let end = schedule
                .end_minutes
                .or_else(|| schedule.end.as_deref().and_then(parse_clock));
            match (start, end) {
                (Some(start), Some(end)) if start <= end => minutes >= start && minutes < end,
                (Some(start), Some(end)) => minutes >= start || minutes < end,
                _ => true,
            }
        }
        _ => true,
    }
}

fn parse_clock(value: &str) -> Option<u16> {
    let (hour, minute) = value.split_once(':')?;
    let hour = hour.parse::<u16>().ok()?;
    let minute = minute.parse::<u16>().ok()?;
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

fn parse_weekday(value: &str) -> Option<u8> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sun" | "sunday" => Some(0),
        "mon" | "monday" => Some(1),
        "tue" | "tues" | "tuesday" => Some(2),
        "wed" | "wednesday" => Some(3),
        "thu" | "thur" | "thurs" | "thursday" => Some(4),
        "fri" | "friday" => Some(5),
        "sat" | "saturday" => Some(6),
        _ => None,
    }
}

#[cfg(unix)]
fn local_day_and_minutes(timestamp: i64) -> (u8, u16) {
    let timestamp = timestamp as libc::time_t;
    let mut output = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r writes one tm to the provided non-null output pointer.
    let result = unsafe { libc::localtime_r(&timestamp, output.as_mut_ptr()) };
    if result.is_null() {
        return utc_day_and_minutes(timestamp as i64);
    }
    // SAFETY: a non-null localtime_r result initialized the output value.
    let output = unsafe { output.assume_init() };
    (
        output.tm_wday.clamp(0, 6) as u8,
        (output.tm_hour.clamp(0, 23) * 60 + output.tm_min.clamp(0, 59)) as u16,
    )
}

#[cfg(not(unix))]
fn local_day_and_minutes(timestamp: i64) -> (u8, u16) {
    utc_day_and_minutes(timestamp)
}

fn utc_day_and_minutes(timestamp: i64) -> (u8, u16) {
    let seconds = timestamp.rem_euclid(86_400);
    let days = timestamp.div_euclid(86_400);
    (((days + 4).rem_euclid(7)) as u8, (seconds / 60) as u16)
}

fn choose_target(rule: &ModelMappingRule) -> Option<String> {
    if rule.target_models.is_empty() {
        return None;
    }
    if rule.kind != "loadbalance" || rule.target_models.len() == 1 {
        return rule.target_models.first().cloned();
    }
    let weights = rule
        .weights
        .clone()
        .filter(|weights| weights.len() == rule.target_models.len())
        .unwrap_or_else(|| vec![1; rule.target_models.len()]);
    let distribution = WeightedIndex::new(weights).ok()?;
    Some(rule.target_models[distribution.sample(&mut rand::thread_rng())].clone())
}

fn glob(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.split_once('*') {
        Some((prefix, suffix)) => value.starts_with(prefix) && value.ends_with(suffix),
        None => pattern.eq_ignore_ascii_case(value),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingReason {
    ClientDisabled,
    ModelDisabled,
    ToolResultFollowup,
    ControlTurn,
    ShortToolTurn,
    ToolTurn,
    Standard,
}

pub fn should_enable_thinking(
    client_enabled: bool,
    model_enabled: bool,
    has_tool_result: bool,
    is_control_turn: bool,
    has_tools: bool,
    input_tokens: usize,
) -> (bool, ThinkingReason) {
    if !client_enabled {
        return (false, ThinkingReason::ClientDisabled);
    }
    if !model_enabled {
        return (false, ThinkingReason::ModelDisabled);
    }
    if has_tool_result {
        return (false, ThinkingReason::ToolResultFollowup);
    }
    if is_control_turn {
        return (false, ThinkingReason::ControlTurn);
    }
    if has_tools && input_tokens < 300 {
        return (true, ThinkingReason::ShortToolTurn);
    }
    if has_tools {
        return (true, ThinkingReason::ToolTurn);
    }
    (true, ThinkingReason::Standard)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingDecision {
    pub enabled: bool,
    pub reason: ThinkingReason,
    pub budget_tokens: Option<u32>,
}

pub fn thinking_enabled_for_model(
    model: &str,
    modes: &std::collections::BTreeMap<String, bool>,
) -> bool {
    modes
        .iter()
        .find(|(key, _)| {
            model.eq_ignore_ascii_case(key)
                || model
                    .to_ascii_lowercase()
                    .starts_with(&format!("{}-", key.to_ascii_lowercase()))
        })
        .map(|(_, enabled)| *enabled)
        .unwrap_or(true)
}

pub fn apply_adaptive_thinking(
    payload: &mut crate::KiroPayload,
    thinking: Option<&crate::ThinkingConfig>,
    model_enabled: bool,
    adaptive: bool,
    configured_cap: u32,
) -> ThinkingDecision {
    let client_enabled = thinking.is_some_and(|thinking| {
        matches!(
            thinking.r#type.to_ascii_lowercase().as_str(),
            "enabled" | "adaptive"
        )
    });
    let current = &payload
        .conversation_state
        .current_message
        .user_input_message;
    let tool_results = current
        .user_input_message_context
        .as_ref()
        .map_or(0, |context| context.tool_results.len());
    let tools = current
        .user_input_message_context
        .as_ref()
        .map_or(0, |context| context.tools.len());
    let normalized = current.content.trim().to_ascii_lowercase();
    let control = !payload.conversation_state.history.is_empty()
        && matches!(
            normalized.as_str(),
            "continue"
                | "continue."
                | "tool results provided."
                | "tool results provided"
                | "done. continue with the next step."
                | "done. continue with the next step"
        );
    let (enabled, reason) = should_enable_thinking(
        client_enabled,
        model_enabled,
        adaptive && tool_results > 0,
        adaptive && control,
        tools > 0,
        current.content.chars().count() / 4,
    );
    if !enabled {
        return ThinkingDecision {
            enabled,
            reason,
            budget_tokens: None,
        };
    }

    let current_max = payload
        .inference_config
        .as_ref()
        .map(|inference| inference.max_tokens)
        .unwrap_or(16_384)
        .min(32_768);
    let fallback = match reason {
        ThinkingReason::ShortToolTurn => 1_024,
        ThinkingReason::ToolTurn => 4_096,
        _ => current_max / 4,
    };
    let reason_cap = match reason {
        ThinkingReason::ShortToolTurn => 1_024,
        ThinkingReason::ToolTurn => 4_096,
        _ => u32::MAX,
    };
    let budget = thinking
        .and_then(|thinking| thinking.budget_tokens)
        .unwrap_or(fallback)
        .max(1)
        .min(current_max / 2)
        .min(configured_cap.max(1))
        .min(8_192)
        .min(reason_cap);
    let instruction = "[CRITICAL - Thinking Mode] Use the same language as the user for BOTH your thinking process and your response. If the user writes in Chinese, think and respond in Chinese; if in English, think and respond in English.";
    let current = &mut payload
        .conversation_state
        .current_message
        .user_input_message;
    if !current
        .content
        .trim_start()
        .starts_with("<thinking_mode>enabled</thinking_mode>")
    {
        current.content = format!(
            "<thinking_mode>enabled</thinking_mode>\n<max_thinking_length>{budget}</max_thinking_length>\n\n{instruction}\n\n{}",
            current.content
        );
    }
    if let Some(inference) = &mut payload.inference_config {
        inference.max_tokens = current_max.saturating_add(budget).min(32_768);
    }
    ThinkingDecision {
        enabled: true,
        reason,
        budget_tokens: Some(budget),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_match_is_anchored() {
        assert!(glob("claude-*", "claude-sonnet-4"));
        assert!(!glob("claude-*", "x-claude-sonnet-4"));
    }

    #[test]
    fn tool_result_disables_adaptive_thinking() {
        assert_eq!(
            should_enable_thinking(true, true, true, false, true, 1000),
            (false, ThinkingReason::ToolResultFollowup)
        );
    }

    #[test]
    fn short_tool_turn_keeps_thinking_with_a_small_budget() {
        assert_eq!(
            should_enable_thinking(true, true, false, false, true, 100),
            (true, ThinkingReason::ShortToolTurn)
        );
    }

    #[test]
    fn daily_schedule_supports_cross_midnight_and_weekdays() {
        let schedule = ModelMappingSchedule {
            mode: "daily".into(),
            days_of_week: Some(vec![1]),
            days: None,
            start_minutes: Some(22 * 60),
            start: None,
            end_minutes: Some(6 * 60),
            end: None,
            start_at: None,
            end_at: None,
        };
        assert!(schedule_active_at(&schedule, 0, 1, 23 * 60));
        assert!(schedule_active_at(&schedule, 0, 1, 5 * 60));
        assert!(!schedule_active_at(&schedule, 0, 1, 12 * 60));
        assert!(!schedule_active_at(&schedule, 0, 2, 23 * 60));

        let human_schedule = ModelMappingSchedule {
            mode: "always".into(),
            days_of_week: None,
            days: Some(vec!["mon".into(), "tue".into()]),
            start_minutes: None,
            start: Some("09:00".into()),
            end_minutes: None,
            end: Some("18:00".into()),
            start_at: None,
            end_at: None,
        };
        assert!(schedule_active_at(&human_schedule, 0, 1, 12 * 60));
        assert!(!schedule_active_at(&human_schedule, 0, 3, 12 * 60));
        assert!(!schedule_active_at(&human_schedule, 0, 1, 20 * 60));
    }

    #[test]
    fn dynamic_model_resolution_preserves_the_accounts_exact_version() {
        let models = vec![
            "claude-sonnet-4-20250514".into(),
            "claude-sonnet-4-20250801".into(),
            "claude-opus-4-1".into(),
        ];
        assert_eq!(
            resolve_dynamic_model("CLAUDE_SONNET_4", &models).as_deref(),
            Some("claude-sonnet-4-20250801")
        );
        assert_eq!(
            resolve_dynamic_model("claude.opus.4.1", &models).as_deref(),
            Some("claude-opus-4-1")
        );
        assert!(resolve_dynamic_model("unrelated-model", &models).is_none());
    }

    #[test]
    fn dynamic_model_resolution_accepts_reordered_client_aliases() {
        let models = vec!["claude-opus-4.6".into(), "claude-sonnet-4.6".into()];
        assert_eq!(
            resolve_dynamic_model("claude-4.6-sonnet", &models).as_deref(),
            Some("claude-sonnet-4.6")
        );
        assert_eq!(
            resolve_dynamic_model("claude-4-6-opus", &models).as_deref(),
            Some("claude-opus-4.6")
        );
        assert!(resolve_dynamic_model("claude-4.6-haiku", &models).is_none());
        assert_eq!(
            resolve_dynamic_model(
                "claude-4.6-sonnet",
                &["CLAUDE_SONNET_4_6_20260217_V1_0".into()]
            )
            .as_deref(),
            Some("CLAUDE_SONNET_4_6_20260217_V1_0")
        );
    }

    #[test]
    fn explicit_mapping_routes_before_automatic_model_resolution() {
        let rule = ModelMappingRule {
            name: "force-opus".into(),
            enabled: true,
            kind: "replace".into(),
            source_models: vec!["claude-4.6-sonnet".into()],
            target_models: vec!["claude-opus-4.6".into()],
            priority: 1,
            weights: None,
            max_remaining_credit_percent: None,
            api_key_ids: None,
            schedule: None,
        };
        let route = map_model("claude-4.6-sonnet", &[rule], None, None, "");
        assert_eq!(route.mapped, "claude-opus-4.6");
        assert_eq!(route.rule.as_deref(), Some("force-opus"));
        assert_eq!(
            resolve_dynamic_model(
                &route.mapped,
                &["claude-sonnet-4.6".into(), "claude-opus-4.6".into()]
            )
            .as_deref(),
            Some("claude-opus-4.6")
        );
    }

    #[test]
    fn low_credit_mapping_is_active_all_day_until_credit_recovers() {
        let rule = ModelMappingRule {
            name: "low-credit-fallback".into(),
            enabled: true,
            kind: "replace".into(),
            source_models: vec!["claude-opus-*".into()],
            target_models: vec!["claude-sonnet-4.6".into()],
            priority: 1,
            weights: None,
            max_remaining_credit_percent: Some(10.0),
            api_key_ids: None,
            // No schedule is the documented all-day default.
            schedule: None,
        };

        let low = map_model(
            "claude-opus-4.6",
            std::slice::from_ref(&rule),
            None,
            Some(9.9),
            "",
        );
        assert_eq!(low.mapped, "claude-sonnet-4.6");
        assert_eq!(low.rule.as_deref(), Some("low-credit-fallback"));

        let recovered = map_model("claude-opus-4.6", &[rule], None, Some(10.0), "");
        assert_eq!(recovered.mapped, "claude-opus-4.6");
        assert!(recovered.rule.is_none());
    }
}
