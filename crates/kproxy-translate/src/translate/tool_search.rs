use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

use regex::Regex;
use serde_json::Value;

use crate::{ClaudeRequest, ClaudeTool, KiroTool, KiroToolUse};

use super::common::{kiro_tool, tool_name};

const DEFAULT_SEARCH_RESULTS: usize = 5;
const MAX_SEARCH_RESULTS: usize = 10_000;
const MAX_REGEX_PATTERN_CHARS: usize = 200;
const MAX_BM25_QUERY_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    Regex,
    Bm25,
}

#[derive(Debug, Clone)]
struct SearchTool {
    kind: SearchKind,
    response_name: String,
}

/// A server-side Tool Search operation synthesized by kproxy.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeToolSearchTrace {
    pub id: String,
    pub name: String,
    pub input: Value,
    pub references: Vec<String>,
    pub error: Option<ClaudeToolSearchError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeToolSearchError {
    pub code: String,
    pub message: String,
}

/// Search result plus the Kiro definitions that should be loaded next round.
#[derive(Debug, Clone)]
pub struct ClaudeToolSearchOutcome {
    pub trace: ClaudeToolSearchTrace,
    pub tools: Vec<KiroTool>,
    pub documentation: Vec<String>,
}

/// Complete deferred catalog retained by the proxy but omitted from Kiro requests.
#[derive(Debug, Clone)]
pub struct ClaudeToolSearchCatalog {
    search_tools: BTreeMap<String, SearchTool>,
    deferred: Vec<ClaudeTool>,
}

impl ClaudeToolSearchCatalog {
    pub fn from_request(request: &ClaudeRequest) -> Option<Self> {
        let deferred = request
            .tools
            .iter()
            .filter(|tool| tool.defer_loading && !is_tool_search_tool(tool))
            .cloned()
            .collect::<Vec<_>>();
        if deferred.is_empty() {
            return None;
        }
        let search_tools = request
            .tools
            .iter()
            .filter_map(|tool| {
                let kind = search_kind(tool.r#type.as_deref())?;
                Some((
                    tool_name(&tool.name),
                    SearchTool {
                        kind,
                        response_name: tool.name.clone(),
                    },
                ))
            })
            .collect::<BTreeMap<_, _>>();
        (!search_tools.is_empty()).then_some(Self {
            search_tools,
            deferred,
        })
    }

    /// Moves deferred definitions out of a parsed request after translation so
    /// large catalogs are not duplicated for the lifetime of an upstream call.
    pub fn take_from_request(request: &mut ClaudeRequest) -> Option<Self> {
        let search_tools = request
            .tools
            .iter()
            .filter_map(|tool| {
                let kind = search_kind(tool.r#type.as_deref())?;
                Some((
                    tool_name(&tool.name),
                    SearchTool {
                        kind,
                        response_name: tool.name.clone(),
                    },
                ))
            })
            .collect::<BTreeMap<_, _>>();
        if search_tools.is_empty() {
            return None;
        }
        let mut deferred = Vec::new();
        let mut loaded = Vec::with_capacity(request.tools.len());
        for tool in std::mem::take(&mut request.tools) {
            if tool.defer_loading && !is_tool_search_tool(&tool) {
                deferred.push(tool);
            } else {
                loaded.push(tool);
            }
        }
        request.tools = loaded;
        (!deferred.is_empty()).then_some(Self {
            search_tools,
            deferred,
        })
    }

    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    pub fn is_search_tool(&self, name: &str) -> bool {
        self.search_tools.contains_key(name)
    }

    pub fn search(&self, tool_use: &KiroToolUse) -> ClaudeToolSearchOutcome {
        let Some(search) = self.search_tools.get(&tool_use.name) else {
            return error_outcome(
                tool_use,
                tool_use.name.clone(),
                "unavailable",
                "unknown tool search implementation",
            );
        };
        let result = match search.kind {
            SearchKind::Regex => self.regex_search(&tool_use.input),
            SearchKind::Bm25 => self.bm25_search(&tool_use.input),
        };
        match result {
            Ok(matches) => self.success_outcome(tool_use, &search.response_name, matches),
            Err((code, message)) => {
                error_outcome(tool_use, search.response_name.clone(), code, message)
            }
        }
    }

    fn regex_search(&self, input: &Value) -> Result<Vec<usize>, (&'static str, String)> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    "invalid_tool_input",
                    "tool search requires a string pattern".to_owned(),
                )
            })?;
        if pattern.chars().count() > MAX_REGEX_PATTERN_CHARS {
            return Err((
                "invalid_tool_input",
                format!("tool search pattern exceeds {MAX_REGEX_PATTERN_CHARS} characters"),
            ));
        }
        let expression = Regex::new(pattern).map_err(|error| {
            (
                "invalid_tool_input",
                format!("invalid regex pattern: {error}"),
            )
        })?;
        let limit = search_limit(input)?;
        Ok(self
            .deferred
            .iter()
            .enumerate()
            .filter(|(_, tool)| expression.is_match(&search_document(tool)))
            .map(|(index, _)| index)
            .take(limit)
            .collect())
    }

    fn bm25_search(&self, input: &Value) -> Result<Vec<usize>, (&'static str, String)> {
        let query = input.get("query").and_then(Value::as_str).ok_or_else(|| {
            (
                "invalid_tool_input",
                "tool search requires a string query".to_owned(),
            )
        })?;
        if query.chars().count() > MAX_BM25_QUERY_CHARS {
            return Err((
                "invalid_tool_input",
                format!("tool search query exceeds {MAX_BM25_QUERY_CHARS} characters"),
            ));
        }
        let limit = search_limit(input)?;
        let query_terms = tokenize(query).into_iter().collect::<HashSet<_>>();
        if query_terms.is_empty() {
            return Ok(Vec::new());
        }
        let documents = self
            .deferred
            .iter()
            .map(|tool| tokenize(&search_document(tool)))
            .collect::<Vec<_>>();
        let document_count = documents.len() as f64;
        let average_length =
            documents.iter().map(Vec::len).sum::<usize>().max(1) as f64 / document_count.max(1.0);
        let frequencies = query_terms
            .iter()
            .map(|term| {
                let count = documents
                    .iter()
                    .filter(|document| document.iter().any(|word| word == term))
                    .count();
                (term, count)
            })
            .collect::<HashMap<_, _>>();
        let mut scores = documents
            .iter()
            .enumerate()
            .filter_map(|(index, document)| {
                let length = document.len().max(1) as f64;
                let mut score = 0.0;
                for term in &query_terms {
                    let term_frequency =
                        document.iter().filter(|word| *word == term).count() as f64;
                    if term_frequency == 0.0 {
                        continue;
                    }
                    let containing = *frequencies.get(term).unwrap_or(&0) as f64;
                    let inverse =
                        ((document_count - containing + 0.5) / (containing + 0.5) + 1.0).ln();
                    let normalized = term_frequency * 2.2
                        / (term_frequency + 1.2 * (0.25 + 0.75 * length / average_length));
                    score += inverse * normalized;
                }
                (score > 0.0).then_some((index, score))
            })
            .collect::<Vec<_>>();
        scores.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        Ok(scores
            .into_iter()
            .take(limit)
            .map(|(index, _)| index)
            .collect())
    }

    fn success_outcome(
        &self,
        tool_use: &KiroToolUse,
        response_name: &str,
        matches: Vec<usize>,
    ) -> ClaudeToolSearchOutcome {
        let mut references = Vec::with_capacity(matches.len());
        let mut tools = Vec::with_capacity(matches.len());
        let mut documentation = Vec::new();
        for index in matches {
            let tool = &self.deferred[index];
            references.push(tool.name.clone());
            let (translated, docs) = translate_deferred_tool(tool);
            tools.push(translated);
            documentation.extend(docs);
        }
        ClaudeToolSearchOutcome {
            trace: ClaudeToolSearchTrace {
                id: tool_use.tool_use_id.clone(),
                name: response_name.to_owned(),
                input: tool_use.input.clone(),
                references,
                error: None,
            },
            tools,
            documentation,
        }
    }
}

fn translate_deferred_tool(tool: &ClaudeTool) -> (KiroTool, Option<String>) {
    match tool.r#type.as_deref() {
        Some(kind) if kind.starts_with("web_search") => kiro_tool(
            "web_search",
            "Search the web for real-time information. Returns relevant search results with titles, URLs, and snippets.",
            &serde_json::json!({
                "type":"object",
                "properties":{"query":{"type":"string","description":"The search query"}},
                "required":["query"]
            }),
        ),
        Some(kind) if kind.starts_with("web_fetch") => kiro_tool(
            "web_fetch",
            "Fetch and read content from a specific URL. Returns the page content in readable text format.",
            &serde_json::json!({
                "type":"object",
                "properties":{"url":{"type":"string","description":"The URL to fetch content from"}},
                "required":["url"]
            }),
        ),
        _ => kiro_tool(&tool.name, &tool_description(tool), &tool.input_schema),
    }
}

pub fn is_tool_search_type(kind: &str) -> bool {
    search_kind(Some(kind)).is_some()
}

pub fn is_tool_search_tool(tool: &ClaudeTool) -> bool {
    tool.r#type.as_deref().is_some_and(is_tool_search_type)
}

pub fn tool_search_kiro_tool(tool: &ClaudeTool) -> Option<KiroTool> {
    let kind = search_kind(tool.r#type.as_deref())?;
    let (description, schema) = match kind {
        SearchKind::Regex => (
            "Search deferred tools by regular expression. Call this tool alone when the required tool is not currently loaded. The optional limit controls how many matching definitions are loaded (default 5, maximum 10000).",
            serde_json::json!({
                "type":"object",
                "properties":{
                    "pattern":{"type":"string","maxLength":MAX_REGEX_PATTERN_CHARS},
                    "limit":{
                        "type":"integer",
                        "minimum":1,
                        "maximum":MAX_SEARCH_RESULTS,
                        "default":DEFAULT_SEARCH_RESULTS
                    }
                },
                "required":["pattern"]
            }),
        ),
        SearchKind::Bm25 => (
            "Search deferred tools using a natural-language query. Call this tool alone when the required tool is not currently loaded. The optional limit controls how many matching definitions are loaded (default 5, maximum 10000).",
            serde_json::json!({
                "type":"object",
                "properties":{
                    "query":{"type":"string","maxLength":MAX_BM25_QUERY_CHARS},
                    "limit":{
                        "type":"integer",
                        "minimum":1,
                        "maximum":MAX_SEARCH_RESULTS,
                        "default":DEFAULT_SEARCH_RESULTS
                    }
                },
                "required":["query"]
            }),
        ),
    };
    Some(kiro_tool(&tool.name, description, &schema).0)
}

fn search_kind(kind: Option<&str>) -> Option<SearchKind> {
    let kind = kind?;
    if kind == "tool_search_tool_regex" || kind.starts_with("tool_search_tool_regex_") {
        Some(SearchKind::Regex)
    } else if kind == "tool_search_tool_bm25" || kind.starts_with("tool_search_tool_bm25_") {
        Some(SearchKind::Bm25)
    } else {
        None
    }
}

fn search_document(tool: &ClaudeTool) -> String {
    format!(
        "{}\n{}\n{}",
        tool.name,
        tool.description,
        serde_json::to_string(&tool.input_schema).unwrap_or_default()
    )
}

fn search_limit(input: &Value) -> Result<usize, (&'static str, String)> {
    let Some(value) = input.get("limit") else {
        return Ok(DEFAULT_SEARCH_RESULTS);
    };
    let Some(limit) = value.as_u64().and_then(|limit| usize::try_from(limit).ok()) else {
        return Err((
            "invalid_tool_input",
            format!("tool search limit must be an integer from 1 to {MAX_SEARCH_RESULTS}"),
        ));
    };
    if !(1..=MAX_SEARCH_RESULTS).contains(&limit) {
        return Err((
            "invalid_tool_input",
            format!("tool search limit must be an integer from 1 to {MAX_SEARCH_RESULTS}"),
        ));
    }
    Ok(limit)
}

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn tool_description(tool: &ClaudeTool) -> String {
    let Some(examples) = tool
        .input_examples
        .as_ref()
        .filter(|examples| !examples.is_empty())
    else {
        return tool.description.clone();
    };
    let examples = serde_json::to_string_pretty(examples).unwrap_or_else(|_| "[]".into());
    if tool.description.trim().is_empty() {
        format!("Input examples:\n{examples}")
    } else {
        format!("{}\n\nInput examples:\n{examples}", tool.description.trim())
    }
}

fn error_outcome(
    tool_use: &KiroToolUse,
    response_name: String,
    code: impl Into<String>,
    message: impl Into<String>,
) -> ClaudeToolSearchOutcome {
    ClaudeToolSearchOutcome {
        trace: ClaudeToolSearchTrace {
            id: tool_use.tool_use_id.clone(),
            name: response_name,
            input: tool_use.input.clone(),
            references: Vec::new(),
            error: Some(ClaudeToolSearchError {
                code: code.into(),
                message: message.into(),
            }),
        },
        tools: Vec::new(),
        documentation: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: &str) -> ClaudeRequest {
        serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4-5",
            "max_tokens":256,
            "messages":[{"role":"user","content":"find an issue"}],
            "tools":[
                {"type":kind,"name":kind.trim_end_matches("_20251119")},
                {"name":"mcp__github__list_issues","description":"List GitHub issues by repository","input_schema":{"type":"object"},"defer_loading":true},
                {"name":"mcp__slack__search","description":"Search Slack messages","input_schema":{"type":"object"},"defer_loading":true}
            ]
        }))
        .expect("request")
    }

    fn request_with_matching_tools(kind: &str, count: usize) -> ClaudeRequest {
        let mut request = request(kind);
        request.tools.truncate(1);
        request.tools.extend((0..count).map(|index| ClaudeTool {
            r#type: None,
            name: format!("mcp__catalog__searchable_{index}"),
            description: format!("Search the shared catalog item {index}"),
            input_schema: serde_json::json!({"type":"object"}),
            cache_control: None,
            strict: None,
            input_examples: None,
            defer_loading: true,
        }));
        request
    }

    #[test]
    fn regex_search_loads_only_matching_deferred_tools() {
        let catalog =
            ClaudeToolSearchCatalog::from_request(&request("tool_search_tool_regex_20251119"))
                .expect("catalog");
        let outcome = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_1".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"(?i)github|issue"}),
        });
        assert_eq!(outcome.trace.references, ["mcp__github__list_issues"]);
        assert_eq!(outcome.tools.len(), 1);
        assert!(outcome.trace.error.is_none());
    }

    #[test]
    fn bm25_search_ranks_relevant_tools() {
        let catalog =
            ClaudeToolSearchCatalog::from_request(&request("tool_search_tool_bm25_20251119"))
                .expect("catalog");
        let outcome = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_1".into(),
            name: "tool_search_tool_bm25".into(),
            input: serde_json::json!({"query":"github repository issues"}),
        });
        assert_eq!(outcome.trace.references[0], "mcp__github__list_issues");
    }

    #[test]
    fn search_limit_defaults_to_five_but_accepts_larger_regex_requests() {
        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_regex_20251119",
            12,
        ))
        .expect("catalog");

        let default = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_default".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"searchable"}),
        });
        assert_eq!(default.trace.references.len(), 5);

        let expanded = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_expanded".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"searchable", "limit":9}),
        });
        assert_eq!(expanded.trace.references.len(), 9);
        assert!(expanded.trace.error.is_none());
    }

    #[test]
    fn bm25_honors_explicit_limit_and_rejects_out_of_range_values() {
        let catalog = ClaudeToolSearchCatalog::from_request(&request_with_matching_tools(
            "tool_search_tool_bm25_20251119",
            12,
        ))
        .expect("catalog");

        let expanded = catalog.search(&KiroToolUse {
            tool_use_id: "srvtoolu_expanded".into(),
            name: "tool_search_tool_bm25".into(),
            input: serde_json::json!({"query":"shared catalog search", "limit":8}),
        });
        assert_eq!(expanded.trace.references.len(), 8);

        for limit in [
            serde_json::json!(0),
            serde_json::json!(10_001),
            serde_json::json!(2.5),
        ] {
            let invalid = catalog.search(&KiroToolUse {
                tool_use_id: "srvtoolu_invalid".into(),
                name: "tool_search_tool_bm25".into(),
                input: serde_json::json!({"query":"shared catalog search", "limit":limit}),
            });
            assert_eq!(
                invalid
                    .trace
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some("invalid_tool_input")
            );
            assert!(invalid.trace.references.is_empty());
        }
    }

    #[test]
    fn taking_catalog_removes_deferred_request_copies() {
        let mut request = request("tool_search_tool_regex_20251119");
        let catalog = ClaudeToolSearchCatalog::take_from_request(&mut request).expect("catalog");
        assert_eq!(catalog.deferred_len(), 2);
        assert_eq!(request.tools.len(), 1);
        assert!(is_tool_search_tool(&request.tools[0]));
    }
}
