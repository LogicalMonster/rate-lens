use crate::{NormalizedUsage, Protocol};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolHint {
    Auto,
    OpenAiResponses,
    AnthropicMessages,
}

impl fmt::Display for ProtocolHint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => formatter.write_str("auto"),
            Self::OpenAiResponses => formatter.write_str("openai"),
            Self::AnthropicMessages => formatter.write_str("anthropic"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseReport {
    pub usage: NormalizedUsage,
    pub warnings: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("输入为空")]
    EmptyInput,
    #[error("无法解析 JSON：{0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("无法自动判断协议；请使用 --protocol openai 或 --protocol anthropic")]
    UnknownProtocol,
    #[error("{protocol} 数据中没有找到可用的 usage")]
    MissingUsage { protocol: &'static str },
    #[error("usage.{field} 不是有效的非负整数")]
    InvalidUsageField { field: String },
    #[error("OpenAI usage 中 cached_tokens + cache_write_tokens 大于 input_tokens")]
    InvalidOpenAiInputBreakdown,
    #[error("Anthropic cache_creation TTL 明细之和大于 cache_creation_input_tokens")]
    InvalidAnthropicCacheBreakdown,
    #[error("输入包含 OpenAI 与 Anthropic 两种协议，不能合并计算")]
    MixedProtocols,
}

pub fn parse_usage(input: &str, hint: ProtocolHint) -> Result<ParseReport, ParseError> {
    if input.trim().is_empty() {
        return Err(ParseError::EmptyInput);
    }

    let candidates = parse_candidates(input)?;
    let protocol = match hint {
        ProtocolHint::OpenAiResponses => Protocol::OpenAiResponses,
        ProtocolHint::AnthropicMessages => Protocol::AnthropicMessages,
        ProtocolHint::Auto => detect_protocols(&candidates)?,
    };

    match protocol {
        Protocol::OpenAiResponses => parse_openai(&candidates),
        Protocol::AnthropicMessages => parse_anthropic(&candidates),
    }
}

fn parse_candidates(input: &str) -> Result<Vec<Value>, ParseError> {
    let trimmed = input.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok(expand_top_level(value));
    }

    let is_sse = trimmed.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("data:") || line.starts_with("event:")
    });
    if is_sse {
        return parse_sse(trimmed);
    }

    let mut values = Vec::new();
    for item in serde_json::Deserializer::from_str(trimmed).into_iter::<Value>() {
        values.extend(expand_top_level(item?));
    }
    if values.is_empty() {
        return Err(ParseError::EmptyInput);
    }
    Ok(values)
}

fn parse_sse(input: &str) -> Result<Vec<Value>, ParseError> {
    let mut values = Vec::new();
    let mut data_lines = Vec::new();

    for raw_line in input.lines().chain(std::iter::once("")) {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            flush_sse_data(&mut data_lines, &mut values)?;
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            // Some captured logs omit the blank separator between events.
            // A new data line following a complete JSON value starts a new
            // event in that common representation.
            if !data_lines.is_empty() {
                let buffered = data_lines.join("\n");
                if serde_json::from_str::<Value>(&buffered).is_ok() || buffered.trim() == "[DONE]" {
                    flush_sse_data(&mut data_lines, &mut values)?;
                }
            }
            data_lines.push(data.trim_start());
        } else if line.starts_with("event:") {
            flush_sse_data(&mut data_lines, &mut values)?;
        } else if line.starts_with("id:") || line.starts_with("retry:") || line.starts_with(':') {
            continue;
        } else {
            // Accept a bare JSON line mixed into captured SSE logs, while
            // still failing closed if that line is malformed.
            flush_sse_data(&mut data_lines, &mut values)?;
            values.extend(expand_top_level(serde_json::from_str(line)?));
        }
    }

    if values.is_empty() {
        return Err(ParseError::MissingUsage { protocol: "SSE" });
    }
    Ok(values)
}

fn flush_sse_data(data_lines: &mut Vec<&str>, values: &mut Vec<Value>) -> Result<(), ParseError> {
    if data_lines.is_empty() {
        return Ok(());
    }
    let payload = data_lines.join("\n");
    data_lines.clear();
    if payload.trim() == "[DONE]" || payload.trim().is_empty() {
        return Ok(());
    }
    values.extend(expand_top_level(serde_json::from_str(&payload)?));
    Ok(())
}

fn expand_top_level(value: Value) -> Vec<Value> {
    match value {
        Value::Array(values) => values,
        value => vec![value],
    }
}

fn detect_protocols(values: &[Value]) -> Result<Protocol, ParseError> {
    let mut openai = false;
    let mut anthropic = false;
    for value in values {
        openai |= looks_like_openai(value);
        anthropic |= looks_like_anthropic(value);
    }
    match (openai, anthropic) {
        (true, false) => Ok(Protocol::OpenAiResponses),
        (false, true) => Ok(Protocol::AnthropicMessages),
        (true, true) => Err(ParseError::MixedProtocols),
        (false, false) => Err(ParseError::UnknownProtocol),
    }
}

fn looks_like_openai(value: &Value) -> bool {
    value.get("object").and_then(Value::as_str) == Some("response")
        || value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("response."))
        || value.get("response").is_some_and(|response| {
            response.get("object").and_then(Value::as_str) == Some("response")
                || response.get("usage").is_some()
        })
        || value.get("usage").is_some_and(|usage| {
            usage.get("input_tokens_details").is_some()
                || (usage.get("input_tokens").is_some()
                    && usage.get("total_tokens").is_some()
                    && usage.get("cache_creation_input_tokens").is_none())
        })
}

fn looks_like_anthropic(value: &Value) -> bool {
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "message" | "message_start" | "message_delta" | "message_stop"
            )
        })
        || value.get("usage").is_some_and(|usage| {
            usage.get("cache_creation_input_tokens").is_some()
                || usage.get("cache_read_input_tokens").is_some()
                || usage.get("server_tool_use").is_some()
                || usage.get("service_tier").is_some()
        })
}

fn parse_openai(values: &[Value]) -> Result<ParseReport, ParseError> {
    let mut totals = Totals::new(Protocol::OpenAiResponses);
    let mut seen = HashSet::new();
    let mut usage_found = false;

    for value in values {
        let response = value.get("response").unwrap_or(value);
        let Some(usage) = response.get("usage") else {
            continue;
        };
        if usage.is_null() {
            continue;
        }

        let id = response
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| value.get("response_id").and_then(Value::as_str));
        if let Some(id) = id
            && !seen.insert(id.to_owned())
        {
            continue;
        }

        usage_found = true;
        totals.requests += 1;
        insert_string(response, "model", &mut totals.models);
        insert_string(response, "service_tier", &mut totals.service_tiers);

        let input = unsigned(usage, "input_tokens")?;
        let cached = nested_unsigned(usage, "input_tokens_details", "cached_tokens")?;
        let cache_write = nested_unsigned(usage, "input_tokens_details", "cache_write_tokens")?;
        if cached + cache_write > input {
            return Err(ParseError::InvalidOpenAiInputBreakdown);
        }
        totals.uncached_input += input - cached - cache_write;
        totals.cache_read += cached;
        totals.cache_write += cache_write;
        totals.output += unsigned(usage, "output_tokens")?;
        totals.reasoning += nested_unsigned(usage, "output_tokens_details", "reasoning_tokens")?;
        let compute_units = unsigned(usage, "compute_units")?;
        if compute_units > 0 {
            *totals
                .metered_extras
                .entry("compute_units".to_owned())
                .or_default() += compute_units;
        }
        collect_openai_hosted_tools(response, &mut totals.metered_extras);
    }

    if !usage_found {
        return Err(ParseError::MissingUsage {
            protocol: "OpenAI Responses",
        });
    }

    let mut warnings = Vec::new();
    if totals.cache_write > 0 {
        warnings.push(
            "响应包含 cache_write_tokens；请确认目标模型是否公布独立缓存写入价格。".to_owned(),
        );
    }
    if !totals.metered_extras.is_empty() {
        warnings.push(
            "检测到非 token 计量项；请通过 --extra-official-cost 加入相应官方费用。".to_owned(),
        );
    }
    Ok(ParseReport {
        usage: totals.finish(),
        warnings,
    })
}

fn parse_anthropic(values: &[Value]) -> Result<ParseReport, ParseError> {
    let mut totals = Totals::new(Protocol::AnthropicMessages);
    let mut usage_found = false;
    let mut anonymous_stream: Option<AnthropicUsage> = None;
    let mut active_message_id: Option<String> = None;

    for value in values {
        let kind = value.get("type").and_then(Value::as_str);
        if kind == Some("message_stop") {
            if let Some(stream) = anonymous_stream.take() {
                totals.requests += 1;
                totals.add_anthropic(&stream);
            }
            active_message_id = None;
            continue;
        }
        if kind == Some("message_start")
            && active_message_id.is_none()
            && let Some(stream) = anonymous_stream.take()
        {
            totals.requests += 1;
            totals.add_anthropic(&stream);
        }
        let message = value.get("message").unwrap_or(value);
        let usage = match kind {
            Some("message_start") => message.get("usage"),
            Some("message_delta") => value.get("usage"),
            Some("message") | None => message.get("usage"),
            _ => None,
        };
        let Some(usage) = usage else {
            continue;
        };
        if usage.is_null() {
            continue;
        }
        usage_found = true;
        let parsed = AnthropicUsage::from_value(usage)?;
        let direct_id = message
            .get("id")
            .and_then(Value::as_str)
            .or_else(|| value.get("message_id").and_then(Value::as_str))
            .map(str::to_owned);
        if kind == Some("message_start") {
            active_message_id = direct_id.clone();
        }
        let id = direct_id.as_deref().or_else(|| {
            (kind == Some("message_delta"))
                .then_some(active_message_id.as_deref())
                .flatten()
        });

        insert_string(message, "model", &mut totals.models);
        insert_string(usage, "service_tier", &mut totals.service_tiers);

        if let Some(id) = id {
            if !totals.anthropic_by_id.contains_key(id) {
                totals.requests += 1;
                totals.add_anthropic(&parsed);
                totals.anthropic_by_id.insert(id.to_owned(), parsed);
            } else {
                // SSE usage is cumulative for a message. Keep the maximum of
                // fields seen for the same id by replacing only upward deltas.
                totals.merge_anthropic_cumulative(id, &parsed);
            }
        } else if matches!(kind, Some("message_start") | Some("message_delta")) {
            anonymous_stream = Some(match anonymous_stream {
                Some(current) => current.max_fields(parsed),
                None => parsed,
            });
        } else {
            totals.requests += 1;
            totals.add_anthropic(&parsed);
        }
    }

    if let Some(stream) = anonymous_stream {
        totals.requests += 1;
        totals.add_anthropic(&stream);
    }

    if !usage_found {
        return Err(ParseError::MissingUsage {
            protocol: "Anthropic Messages",
        });
    }

    if !totals.metered_extras.is_empty() {
        totals.warnings.push(
            "检测到 Anthropic 服务端工具调用；token 费用不包含工具调用费，请用 --extra-official-cost 补充。"
                .to_owned(),
        );
    }
    let warnings = std::mem::take(&mut totals.warnings);
    Ok(ParseReport {
        usage: totals.finish(),
        warnings,
    })
}

#[derive(Debug, Clone, Default)]
struct AnthropicUsage {
    input: u64,
    cache_read: u64,
    cache_write_unspecified: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    output: u64,
    thinking: u64,
    extras: BTreeMap<String, u64>,
}

impl AnthropicUsage {
    fn from_value(usage: &Value) -> Result<Self, ParseError> {
        let cache_creation = unsigned(usage, "cache_creation_input_tokens")?;
        let cache_write_5m = nested_unsigned(usage, "cache_creation", "ephemeral_5m_input_tokens")?;
        let cache_write_1h = nested_unsigned(usage, "cache_creation", "ephemeral_1h_input_tokens")?;
        if cache_write_5m + cache_write_1h > cache_creation {
            return Err(ParseError::InvalidAnthropicCacheBreakdown);
        }
        let mut extras = BTreeMap::new();
        if let Some(tools) = usage.get("server_tool_use").and_then(Value::as_object) {
            for (name, value) in tools {
                let count = as_unsigned(value, &format!("server_tool_use.{name}"))?;
                if count > 0 {
                    extras.insert(name.clone(), count);
                }
            }
        }
        Ok(Self {
            input: unsigned(usage, "input_tokens")?,
            cache_read: unsigned(usage, "cache_read_input_tokens")?,
            cache_write_unspecified: cache_creation - cache_write_5m - cache_write_1h,
            cache_write_5m,
            cache_write_1h,
            output: unsigned(usage, "output_tokens")?,
            thinking: nested_unsigned(usage, "output_tokens_details", "thinking_tokens")?,
            extras,
        })
    }

    fn max_fields(mut self, other: Self) -> Self {
        self.input = self.input.max(other.input);
        self.cache_read = self.cache_read.max(other.cache_read);
        self.cache_write_unspecified = self
            .cache_write_unspecified
            .max(other.cache_write_unspecified);
        self.cache_write_5m = self.cache_write_5m.max(other.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.max(other.cache_write_1h);
        self.output = self.output.max(other.output);
        self.thinking = self.thinking.max(other.thinking);
        for (key, count) in other.extras {
            self.extras
                .entry(key)
                .and_modify(|current| *current = (*current).max(count))
                .or_insert(count);
        }
        self
    }
}

struct Totals {
    protocol: Protocol,
    models: BTreeSet<String>,
    service_tiers: BTreeSet<String>,
    requests: u64,
    uncached_input: u64,
    cache_read: u64,
    cache_write: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    output: u64,
    reasoning: u64,
    metered_extras: BTreeMap<String, u64>,
    anthropic_by_id: BTreeMap<String, AnthropicUsage>,
    warnings: Vec<String>,
}

impl Totals {
    fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            models: BTreeSet::new(),
            service_tiers: BTreeSet::new(),
            requests: 0,
            uncached_input: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 0,
            reasoning: 0,
            metered_extras: BTreeMap::new(),
            anthropic_by_id: BTreeMap::new(),
            warnings: Vec::new(),
        }
    }

    fn add_anthropic(&mut self, usage: &AnthropicUsage) {
        self.uncached_input += usage.input;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write_unspecified;
        self.cache_write_5m += usage.cache_write_5m;
        self.cache_write_1h += usage.cache_write_1h;
        self.output += usage.output;
        self.reasoning += usage.thinking;
        for (key, count) in &usage.extras {
            *self.metered_extras.entry(key.clone()).or_default() += count;
        }
    }

    fn subtract_anthropic(&mut self, usage: &AnthropicUsage) {
        self.uncached_input -= usage.input;
        self.cache_read -= usage.cache_read;
        self.cache_write -= usage.cache_write_unspecified;
        self.cache_write_5m -= usage.cache_write_5m;
        self.cache_write_1h -= usage.cache_write_1h;
        self.output -= usage.output;
        self.reasoning -= usage.thinking;
        for (key, count) in &usage.extras {
            if let Some(total) = self.metered_extras.get_mut(key) {
                *total -= count;
            }
        }
    }

    fn merge_anthropic_cumulative(&mut self, id: &str, usage: &AnthropicUsage) {
        let previous = self.anthropic_by_id.remove(id).unwrap_or_default();
        self.subtract_anthropic(&previous);
        let merged = previous.max_fields(usage.clone());
        self.add_anthropic(&merged);
        self.anthropic_by_id.insert(id.to_owned(), merged);
    }

    fn finish(self) -> NormalizedUsage {
        NormalizedUsage {
            protocol: self.protocol,
            models: self.models,
            service_tiers: self.service_tiers,
            requests: self.requests,
            uncached_input_tokens: self.uncached_input,
            cache_read_input_tokens: self.cache_read,
            cache_write_input_tokens: self.cache_write,
            cache_write_5m_input_tokens: self.cache_write_5m,
            cache_write_1h_input_tokens: self.cache_write_1h,
            output_tokens: self.output,
            reasoning_output_tokens: self.reasoning,
            metered_extras: self.metered_extras,
        }
    }
}

fn unsigned(object: &Value, field: &str) -> Result<u64, ParseError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => as_unsigned(value, field),
    }
}

fn nested_unsigned(object: &Value, parent: &str, field: &str) -> Result<u64, ParseError> {
    let Some(parent_value) = object.get(parent) else {
        return Ok(0);
    };
    if parent_value.is_null() {
        return Ok(0);
    }
    match parent_value.get(field) {
        None | Some(Value::Null) => Ok(0),
        Some(value) => as_unsigned(value, &format!("{parent}.{field}")),
    }
}

fn as_unsigned(value: &Value, field: &str) -> Result<u64, ParseError> {
    value.as_u64().ok_or_else(|| ParseError::InvalidUsageField {
        field: field.to_owned(),
    })
}

fn insert_string(value: &Value, field: &str, target: &mut BTreeSet<String>) {
    if let Some(text) = value.get(field).and_then(Value::as_str) {
        target.insert(text.to_owned());
    }
}

fn collect_openai_hosted_tools(response: &Value, target: &mut BTreeMap<String, u64>) {
    let Some(output) = response.get("output").and_then(Value::as_array) else {
        return;
    };
    for item in output {
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if matches!(
            kind,
            "web_search_call"
                | "file_search_call"
                | "code_interpreter_call"
                | "image_generation_call"
                | "computer_call"
        ) {
            *target.entry(kind.to_owned()).or_default() += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_response_and_keeps_buckets_exclusive() {
        let report = parse_usage(
            r#"{
                "id":"resp_1","object":"response","model":"gpt-test",
                "usage":{
                  "input_tokens":1000,
                  "input_tokens_details":{"cached_tokens":200,"cache_write_tokens":100},
                  "output_tokens":300,
                  "output_tokens_details":{"reasoning_tokens":120},
                  "total_tokens":1300
                }
            }"#,
            ProtocolHint::Auto,
        )
        .unwrap();

        assert_eq!(report.usage.protocol, Protocol::OpenAiResponses);
        assert_eq!(report.usage.uncached_input_tokens, 700);
        assert_eq!(report.usage.cache_read_input_tokens, 200);
        assert_eq!(report.usage.cache_write_input_tokens, 100);
        assert_eq!(report.usage.output_tokens, 300);
        assert_eq!(report.usage.reasoning_output_tokens, 120);
        assert_eq!(report.usage.total_tokens(), 1300);
    }

    #[test]
    fn parses_anthropic_cache_ttl_breakdown() {
        let report = parse_usage(
            r#"{
              "id":"msg_1","type":"message","model":"claude-test",
              "usage":{
                "input_tokens":100,
                "cache_creation_input_tokens":70,
                "cache_read_input_tokens":30,
                "cache_creation":{"ephemeral_5m_input_tokens":50,"ephemeral_1h_input_tokens":20},
                "output_tokens":40,
                "output_tokens_details":{"thinking_tokens":15},
                "service_tier":"standard",
                "server_tool_use":{"web_search_requests":2}
              }
            }"#,
            ProtocolHint::Auto,
        )
        .unwrap();

        assert_eq!(report.usage.protocol, Protocol::AnthropicMessages);
        assert_eq!(report.usage.uncached_input_tokens, 100);
        assert_eq!(report.usage.cache_read_input_tokens, 30);
        assert_eq!(report.usage.cache_write_5m_input_tokens, 50);
        assert_eq!(report.usage.cache_write_1h_input_tokens, 20);
        assert_eq!(report.usage.output_tokens, 40);
        assert_eq!(report.usage.reasoning_output_tokens, 15);
        assert_eq!(report.usage.metered_extras["web_search_requests"], 2);
    }

    #[test]
    fn deduplicates_openai_completed_event_and_response() {
        let input = r#"
event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","model":"gpt-test","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}
{"id":"resp_1","object":"response","model":"gpt-test","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}
        "#;
        let report = parse_usage(input, ProtocolHint::Auto).unwrap();
        assert_eq!(report.usage.requests, 1);
        assert_eq!(report.usage.total_tokens(), 15);
    }

    #[test]
    fn reports_openai_hosted_tools_for_extra_pricing() {
        let report = parse_usage(
            r#"{
              "id":"resp_1","object":"response","model":"gpt-test",
              "output":[
                {"type":"web_search_call","id":"ws_1"},
                {"type":"function_call","id":"fc_1"}
              ],
              "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}
            }"#,
            ProtocolHint::Auto,
        )
        .unwrap();
        assert_eq!(report.usage.metered_extras["web_search_call"], 1);
        assert!(!report.usage.metered_extras.contains_key("function_call"));
        assert!(!report.warnings.is_empty());
    }

    #[test]
    fn merges_anthropic_cumulative_sse_usage() {
        let input = r#"
event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","model":"claude-test","usage":{"input_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}
event: message_delta
data: {"type":"message_delta","usage":{"input_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":25}}
event: message_stop
data: {"type":"message_stop"}
        "#;
        let report = parse_usage(input, ProtocolHint::Auto).unwrap();
        assert_eq!(report.usage.requests, 1);
        assert_eq!(report.usage.uncached_input_tokens, 100);
        assert_eq!(report.usage.output_tokens, 25);
    }

    #[test]
    fn sums_multiple_anonymous_anthropic_streams() {
        let input = r#"
event: message_start
data: {"type":"message_start","message":{"type":"message","model":"claude-test","usage":{"input_tokens":100,"output_tokens":1}}}
event: message_delta
data: {"type":"message_delta","usage":{"input_tokens":100,"output_tokens":20}}
event: message_stop
data: {"type":"message_stop"}
event: message_start
data: {"type":"message_start","message":{"type":"message","model":"claude-test","usage":{"input_tokens":50,"output_tokens":1}}}
event: message_delta
data: {"type":"message_delta","usage":{"input_tokens":50,"output_tokens":10}}
event: message_stop
data: {"type":"message_stop"}
        "#;
        let report = parse_usage(input, ProtocolHint::Auto).unwrap();
        assert_eq!(report.usage.requests, 2);
        assert_eq!(report.usage.uncached_input_tokens, 150);
        assert_eq!(report.usage.output_tokens, 30);
    }
}
