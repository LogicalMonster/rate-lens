use crate::{ParseError, ParseReport, Protocol, ProtocolHint, parse_usage};
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder};
use serde::Serialize;
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::time::Duration;
use thiserror::Error;

const MAX_ERROR_BODY: usize = 4_096;
const MAX_CONTEXT_TOKENS: u64 = 1_000_000;
const DEFAULT_TIMEOUT_SECONDS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    Bearer,
    XApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicThinkingMode {
    Adaptive,
    Enabled,
}

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: String,
    pub auth_style: AuthStyle,
    pub model: String,
    pub context_tokens: u64,
    pub reasoning_effort: Option<String>,
    pub max_output_tokens: u64,
    pub anthropic_thinking_mode: AnthropicThinkingMode,
    pub thinking_budget_tokens: u64,
    /// Enables prompt-cache controls for a cache test. Callers should set this
    /// only after confirming that the relay accepts the relevant fields.
    pub enable_prompt_cache: bool,
    /// A stable key shared by the two requests in an interactive cache test.
    pub prompt_cache_key: Option<String>,
    /// Optional run-unique marker near the beginning of the generated prompt.
    pub prompt_marker: Option<String>,
    /// Optional explicit cache-isolation mode. This is deliberately opt-in;
    /// ordinary wizard probes leave it disabled for relay compatibility.
    pub disable_implicit_prompt_cache: bool,
    /// Whether the relay has explicitly been confirmed to accept cache
    /// controls. For Anthropic this gates `cache_control` as well. The field
    /// name is retained for compatibility with existing callers.
    pub openai_prompt_cache_options: bool,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: Option<String>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub report: ParseReport,
    pub response: Value,
    pub target_context_tokens: u64,
    pub counted_input_tokens: Option<u64>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct Endpoints {
    models: String,
    inference: String,
    count_tokens: String,
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("base URL 不能为空")]
    EmptyBaseUrl,
    #[error("API Key 不能为空")]
    EmptyApiKey,
    #[error("模型名称不能为空")]
    EmptyModel,
    #[error("目标上下文长度必须在 1..={MAX_CONTEXT_TOKENS} token 之间")]
    InvalidContextLength,
    #[error("最大输出长度必须大于 0")]
    InvalidMaxOutputTokens,
    #[error(
        "Anthropic enabled thinking 要求 thinking budget 至少为 1024，且 max output tokens 必须大于 thinking budget"
    )]
    InvalidThinkingBudget,
    #[error("启用缓存控制前必须明确确认中转站支持对应参数")]
    PromptCacheSupportNotConfirmed,
    #[error("无法构建 HTTP 客户端：{0}")]
    Client(String),
    #[error("请求 {endpoint} 失败：{source}")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("请求 {endpoint} 返回 HTTP {status}：{body}")]
    Http {
        endpoint: String,
        status: StatusCode,
        body: String,
    },
    #[error("{endpoint} 返回的内容不是有效 JSON：{source}；响应片段：{body}")]
    InvalidJson {
        endpoint: String,
        #[source]
        source: serde_json::Error,
        body: String,
    },
    #[error("模型列表响应中没有 `data` 数组")]
    InvalidModelsResponse,
    #[error("token 计数响应中没有可用的 token 数")]
    InvalidTokenCountResponse,
    #[error(transparent)]
    Parse(#[from] ParseError),
}

pub fn normalize_api_root(base_url: &str, protocol: Protocol) -> Result<String, ProbeError> {
    let mut url = base_url.trim().trim_end_matches('/').to_owned();
    if url.is_empty() {
        return Err(ProbeError::EmptyBaseUrl);
    }
    let endpoints: &[&str] = match protocol {
        Protocol::OpenAiResponses => &["/responses/input_tokens", "/responses", "/models"],
        Protocol::AnthropicMessages => &["/messages/count_tokens", "/messages", "/models"],
    };
    for endpoint in endpoints {
        if url.ends_with(endpoint) {
            url.truncate(url.len() - endpoint.len());
            break;
        }
    }
    while url.ends_with('/') {
        url.pop();
    }
    if !url.ends_with("/v1") {
        url.push_str("/v1");
    }
    Ok(url)
}

pub fn list_models(
    protocol: Protocol,
    base_url: &str,
    api_key: &str,
    auth_style: AuthStyle,
    timeout_seconds: u64,
) -> Result<Vec<DiscoveredModel>, ProbeError> {
    if api_key.is_empty() {
        return Err(ProbeError::EmptyApiKey);
    }
    let endpoints = endpoints(base_url, protocol)?;
    let client = build_client(timeout_seconds)?;
    let url = match protocol {
        Protocol::OpenAiResponses => endpoints.models,
        Protocol::AnthropicMessages => format!("{}?limit=1000", endpoints.models),
    };
    let response = send_json(
        authenticate(client.get(&url), protocol, auth_style, api_key),
        &url,
    )?;
    let items = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or(ProbeError::InvalidModelsResponse)?;
    let mut models = items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.to_owned();
            let display_name = item
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let capabilities = item.get("capabilities").unwrap_or(item);
            Some(DiscoveredModel {
                id,
                display_name,
                max_input_tokens: capabilities.get("max_input_tokens").and_then(Value::as_u64),
                max_output_tokens: capabilities
                    .get("max_tokens")
                    .or_else(|| capabilities.get("max_output_tokens"))
                    .and_then(Value::as_u64),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| natural_model_cmp(&left.id, &right.id));
    models.dedup_by(|left, right| left.id == right.id);
    Ok(models)
}

pub fn run_probe(config: &ProbeConfig) -> Result<ProbeResult, ProbeError> {
    validate_config(config)?;
    let endpoints = endpoints(&config.base_url, config.protocol)?;
    let client = build_client(config.timeout_seconds)?;
    let (payload, counted_input_tokens, mut warnings) =
        calibrate_payload(config, &client, &endpoints);

    let request = authenticate(
        client.post(&endpoints.inference),
        config.protocol,
        config.auth_style,
        &config.api_key,
    )
    .json(&payload);
    let response = send_json(request, &endpoints.inference)?;
    let response_text =
        serde_json::to_string(&response).expect("JSON value serialization cannot fail");
    let report = parse_usage(
        &response_text,
        match config.protocol {
            Protocol::OpenAiResponses => ProtocolHint::OpenAiResponses,
            Protocol::AnthropicMessages => ProtocolHint::AnthropicMessages,
        },
    )?;
    warnings.extend(report.warnings.iter().cloned());
    if let Some(counted) = counted_input_tokens {
        let actual = report.usage.total_input_tokens();
        if actual.abs_diff(counted) > tolerance(config.context_tokens) {
            warnings.push(format!(
                "计数端点预估输入为 {counted} token，实际响应 usage 为 {actual} token；中转站的计数和推理端点可能口径不同。"
            ));
        }
    }

    Ok(ProbeResult {
        report,
        response,
        target_context_tokens: config.context_tokens,
        counted_input_tokens,
        warnings,
    })
}

pub fn approximate_official_input_cost(
    context_tokens: u64,
    input_rate_per_million: rust_decimal::Decimal,
) -> rust_decimal::Decimal {
    rust_decimal::Decimal::from(context_tokens) * input_rate_per_million
        / rust_decimal::Decimal::from(1_000_000_u64)
}

fn validate_config(config: &ProbeConfig) -> Result<(), ProbeError> {
    if config.api_key.is_empty() {
        return Err(ProbeError::EmptyApiKey);
    }
    if config.model.trim().is_empty() {
        return Err(ProbeError::EmptyModel);
    }
    if !(1..=MAX_CONTEXT_TOKENS).contains(&config.context_tokens) {
        return Err(ProbeError::InvalidContextLength);
    }
    if config.max_output_tokens == 0 {
        return Err(ProbeError::InvalidMaxOutputTokens);
    }
    if (config.enable_prompt_cache || config.disable_implicit_prompt_cache)
        && !config.openai_prompt_cache_options
    {
        return Err(ProbeError::PromptCacheSupportNotConfirmed);
    }
    if config.protocol == Protocol::AnthropicMessages
        && config.reasoning_effort.is_some()
        && config.anthropic_thinking_mode == AnthropicThinkingMode::Enabled
        && (config.thinking_budget_tokens < 1_024
            || config.max_output_tokens <= config.thinking_budget_tokens)
    {
        return Err(ProbeError::InvalidThinkingBudget);
    }
    Ok(())
}

fn endpoints(base_url: &str, protocol: Protocol) -> Result<Endpoints, ProbeError> {
    let root = normalize_api_root(base_url, protocol)?;
    Ok(match protocol {
        Protocol::OpenAiResponses => Endpoints {
            models: format!("{root}/models"),
            inference: format!("{root}/responses"),
            count_tokens: format!("{root}/responses/input_tokens"),
        },
        Protocol::AnthropicMessages => Endpoints {
            models: format!("{root}/models"),
            inference: format!("{root}/messages"),
            count_tokens: format!("{root}/messages/count_tokens"),
        },
    })
}

fn build_client(timeout_seconds: u64) -> Result<Client, ProbeError> {
    Client::builder()
        .timeout(Duration::from_secs(if timeout_seconds == 0 {
            DEFAULT_TIMEOUT_SECONDS
        } else {
            timeout_seconds
        }))
        .user_agent(concat!("rate-lens/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| ProbeError::Client(error.to_string()))
}

fn authenticate(
    request: RequestBuilder,
    protocol: Protocol,
    auth_style: AuthStyle,
    api_key: &str,
) -> RequestBuilder {
    let request = match auth_style {
        AuthStyle::Bearer => request.bearer_auth(api_key),
        AuthStyle::XApiKey => request.header("x-api-key", api_key),
    };
    match protocol {
        Protocol::OpenAiResponses => request,
        Protocol::AnthropicMessages => request.header("anthropic-version", "2023-06-01"),
    }
}

fn send_json(request: RequestBuilder, endpoint: &str) -> Result<Value, ProbeError> {
    let response = request.send().map_err(|source| ProbeError::Transport {
        endpoint: endpoint.to_owned(),
        source,
    })?;
    let status = response.status();
    let body = response.text().map_err(|source| ProbeError::Transport {
        endpoint: endpoint.to_owned(),
        source,
    })?;
    if !status.is_success() {
        return Err(ProbeError::Http {
            endpoint: endpoint.to_owned(),
            status,
            body: truncate_body(&body),
        });
    }
    serde_json::from_str(&body).map_err(|source| ProbeError::InvalidJson {
        endpoint: endpoint.to_owned(),
        source,
        body: truncate_body(&body),
    })
}

fn calibrate_payload(
    config: &ProbeConfig,
    client: &Client,
    endpoints: &Endpoints,
) -> (Value, Option<u64>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut filler_units = initial_filler_units(config.context_tokens);
    let mut best: Option<(u64, u64)> = None;
    let mut previous: Option<(u64, u64)> = None;
    let mut count_requests = 0_u8;

    for _ in 0..6 {
        let payload = build_payload(config, filler_units);
        count_requests += 1;
        match count_tokens(config, client, endpoints, &payload) {
            Ok(count) => {
                if best.as_ref().is_none_or(|(_, current)| {
                    count.abs_diff(config.context_tokens) < current.abs_diff(config.context_tokens)
                }) {
                    best = Some((filler_units, count));
                }
                if count.abs_diff(config.context_tokens) <= tolerance(config.context_tokens) {
                    if count_requests > 1 {
                        warnings.push(format!(
                            "为校准上下文调用了 {count_requests} 次 token 计数端点；部分中转站可能对计数请求收费，请核对账单。"
                        ));
                    }
                    return (payload, Some(count), warnings);
                }
                let current_units = filler_units;
                if let Some((previous_units, previous_count)) = previous
                    && previous_count != count
                {
                    let delta_units = filler_units as i128 - previous_units as i128;
                    let delta_tokens = count as i128 - previous_count as i128;
                    let estimated = filler_units as i128
                        + (config.context_tokens as i128 - count as i128) * delta_units
                            / delta_tokens;
                    filler_units = estimated.max(1) as u64;
                } else {
                    filler_units = scaled_units(filler_units, count, config.context_tokens);
                }
                filler_units = filler_units.clamp(1, max_filler_units());
                previous = Some((current_units, count));
            }
            Err(error) => {
                warnings.push(format!(
                    "token 计数端点不可用（{error}）；已退回确定性近似文本，最终费用仍以真实响应 usage 为准。"
                ));
                if count_requests > 1 {
                    warnings.push(format!(
                        "在降级前调用了 {count_requests} 次 token 计数端点；部分中转站可能对计数请求收费，请核对账单。"
                    ));
                }
                return (build_payload(config, filler_units), None, warnings);
            }
        }
    }

    let (units, count) = best.unwrap_or((filler_units, 0));
    if count.abs_diff(config.context_tokens) > tolerance(config.context_tokens) {
        warnings.push(format!(
            "已尽量校准到 {count} 个输入 token，目标为 {}；费用将按真实响应 usage 计算。",
            config.context_tokens
        ));
    }
    if count_requests > 1 {
        warnings.push(format!(
            "为校准上下文调用了 {count_requests} 次 token 计数端点；部分中转站可能对计数请求收费，请核对账单。"
        ));
    }
    (build_payload(config, units), Some(count), warnings)
}

fn count_tokens(
    config: &ProbeConfig,
    client: &Client,
    endpoints: &Endpoints,
    payload: &Value,
) -> Result<u64, ProbeError> {
    let count_payload = match config.protocol {
        Protocol::OpenAiResponses => {
            let mut value = payload.clone();
            if let Some(object) = value.as_object_mut() {
                object.remove("max_output_tokens");
                object.remove("reasoning");
                object.remove("store");
                object.remove("prompt_cache_options");
                object.remove("prompt_cache_key");
            }
            value
        }
        Protocol::AnthropicMessages => {
            let mut value = payload.clone();
            if let Some(object) = value.as_object_mut() {
                object.remove("max_tokens");
                object.remove("thinking");
                object.remove("output_config");
                object.remove("cache_control");
            }
            value
        }
    };
    let request = authenticate(
        client.post(&endpoints.count_tokens),
        config.protocol,
        config.auth_style,
        &config.api_key,
    )
    .json(&count_payload);
    let response = send_json(request, &endpoints.count_tokens)?;
    response
        .get("input_tokens")
        .or_else(|| response.get("total_tokens"))
        .and_then(Value::as_u64)
        .ok_or(ProbeError::InvalidTokenCountResponse)
}

fn build_payload(config: &ProbeConfig, filler_units: u64) -> Value {
    let prompt = build_prompt(filler_units, config.prompt_marker.as_deref());
    match config.protocol {
        Protocol::OpenAiResponses => {
            let mut payload = json!({
                "model": config.model,
                "input": [{
                    "role": "user",
                    "content": [{"type": "input_text", "text": prompt}]
                }],
                "max_output_tokens": config.max_output_tokens,
                "store": false
            });
            if let Some(effort) = config.reasoning_effort.as_deref() {
                payload["reasoning"] = json!({"effort": effort});
            }
            if config.enable_prompt_cache && config.openai_prompt_cache_options {
                payload["prompt_cache_options"] = json!({"mode": "implicit", "ttl": "30m"});
                if let Some(key) = config.prompt_cache_key.as_deref() {
                    payload["prompt_cache_key"] = json!(key);
                }
            } else if config.disable_implicit_prompt_cache && config.openai_prompt_cache_options {
                payload["prompt_cache_options"] = json!({"mode": "explicit"});
            }
            payload
        }
        Protocol::AnthropicMessages => {
            let mut payload = json!({
                "model": config.model,
                "max_tokens": config.max_output_tokens,
                "messages": [{"role": "user", "content": prompt}]
            });
            if let Some(effort) = config.reasoning_effort.as_deref() {
                match config.anthropic_thinking_mode {
                    AnthropicThinkingMode::Adaptive => {
                        payload["thinking"] = json!({"type": "adaptive"});
                        payload["output_config"] = json!({"effort": effort});
                    }
                    AnthropicThinkingMode::Enabled => {
                        payload["thinking"] = json!({
                            "type": "enabled",
                            "budget_tokens": config.thinking_budget_tokens
                        });
                    }
                }
            }
            if config.enable_prompt_cache && config.openai_prompt_cache_options {
                payload["cache_control"] = json!({"type": "ephemeral"});
            }
            payload
        }
    }
}

fn build_prompt(filler_units: u64, marker: Option<&str>) -> String {
    const PREFIX: &str = "这是 API 计量测试。忽略下方重复内容，只回复 OK，不要解释。\n";
    const UNIT: &str = " context";
    let capacity = PREFIX.len().saturating_add(
        usize::try_from(filler_units)
            .unwrap_or(usize::MAX / UNIT.len())
            .saturating_mul(UNIT.len()),
    );
    let mut prompt = String::with_capacity(capacity);
    prompt.push_str(PREFIX);
    if let Some(marker) = marker {
        prompt.push_str("测试批次：");
        prompt.push_str(marker);
        prompt.push('\n');
    }
    for _ in 0..filler_units {
        prompt.push_str(UNIT);
    }
    prompt
}

fn initial_filler_units(target_tokens: u64) -> u64 {
    target_tokens.saturating_sub(32).max(1)
}

fn max_filler_units() -> u64 {
    MAX_CONTEXT_TOKENS.saturating_mul(4)
}

fn scaled_units(units: u64, counted: u64, target: u64) -> u64 {
    if counted == 0 {
        return units.saturating_mul(2);
    }
    let scaled = (u128::from(units) * u128::from(target)) / u128::from(counted);
    u64::try_from(scaled).unwrap_or(u64::MAX).max(1)
}

fn tolerance(target: u64) -> u64 {
    (target / 100).clamp(8, 1_000)
}

fn truncate_body(body: &str) -> String {
    let mut end = body.len().min(MAX_ERROR_BODY);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if end < body.len() { "…" } else { "" };
    format!("{}{}", &body[..end], suffix)
}

fn natural_model_cmp(left: &str, right: &str) -> Ordering {
    right.to_ascii_lowercase().cmp(&left.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn normalizes_hosts_and_complete_endpoints() {
        assert_eq!(
            normalize_api_root("https://relay.example", Protocol::OpenAiResponses).unwrap(),
            "https://relay.example/v1"
        );
        assert_eq!(
            normalize_api_root(
                "https://relay.example/v1/responses",
                Protocol::OpenAiResponses
            )
            .unwrap(),
            "https://relay.example/v1"
        );
        assert_eq!(
            normalize_api_root(
                "https://relay.example/v1/messages/count_tokens/",
                Protocol::AnthropicMessages
            )
            .unwrap(),
            "https://relay.example/v1"
        );
    }

    #[test]
    fn prompt_has_the_requested_number_of_filler_units() {
        let prompt = build_prompt(123, Some("run-1"));
        assert_eq!(prompt.matches(" context").count(), 123);
        assert!(prompt.starts_with("这是 API 计量测试"));
        assert!(prompt.contains("测试批次：run-1"));
    }

    #[test]
    fn probes_openai_with_counting_and_bearer_auth() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_mock_server(
            vec![
                ("/v1/responses/input_tokens", json!({"input_tokens": 100})),
                (
                    "/v1/responses",
                    json!({
                        "id": "resp_mock",
                        "object": "response",
                        "model": "gpt-5-mini",
                        "usage": {
                            "input_tokens": 100,
                            "input_tokens_details": {"cached_tokens": 0},
                            "output_tokens": 3,
                            "output_tokens_details": {"reasoning_tokens": 1},
                            "total_tokens": 103
                        }
                    }),
                ),
            ],
            Arc::clone(&seen),
        );

        let result = run_probe(&ProbeConfig {
            protocol: Protocol::OpenAiResponses,
            base_url,
            api_key: "secret-openai".to_owned(),
            auth_style: AuthStyle::Bearer,
            model: "gpt-5-mini".to_owned(),
            context_tokens: 100,
            reasoning_effort: Some("low".to_owned()),
            max_output_tokens: 16,
            anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
            thinking_budget_tokens: 1_024,
            enable_prompt_cache: false,
            prompt_cache_key: None,
            prompt_marker: None,
            disable_implicit_prompt_cache: false,
            openai_prompt_cache_options: false,
            timeout_seconds: 5,
        })
        .unwrap();

        assert_eq!(result.counted_input_tokens, Some(100));
        assert_eq!(result.report.usage.output_tokens, 3);
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer secret-openai")
        );
        let body = request_json_body(&requests[1]);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn probes_anthropic_with_x_api_key_and_adaptive_thinking() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_mock_server(
            vec![
                ("/v1/messages/count_tokens", json!({"input_tokens": 80})),
                (
                    "/v1/messages",
                    json!({
                        "id": "msg_mock",
                        "type": "message",
                        "model": "claude-sonnet-4-6",
                        "usage": {
                            "input_tokens": 80,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "output_tokens": 2
                        }
                    }),
                ),
            ],
            Arc::clone(&seen),
        );

        let result = run_probe(&ProbeConfig {
            protocol: Protocol::AnthropicMessages,
            base_url,
            api_key: "secret-anthropic".to_owned(),
            auth_style: AuthStyle::XApiKey,
            model: "claude-sonnet-4-6".to_owned(),
            context_tokens: 80,
            reasoning_effort: Some("high".to_owned()),
            max_output_tokens: 64,
            anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
            thinking_budget_tokens: 1_024,
            enable_prompt_cache: false,
            prompt_cache_key: None,
            prompt_marker: None,
            disable_implicit_prompt_cache: false,
            openai_prompt_cache_options: false,
            timeout_seconds: 5,
        })
        .unwrap();

        assert_eq!(result.counted_input_tokens, Some(80));
        assert_eq!(result.report.usage.uncached_input_tokens, 80);
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let lower = requests[0].to_ascii_lowercase();
        assert!(lower.contains("x-api-key: secret-anthropic"));
        assert!(lower.contains("anthropic-version: 2023-06-01"));
        let body = request_json_body(&requests[1]);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn cache_mode_adds_protocol_specific_cache_controls() {
        let openai = build_payload(
            &ProbeConfig {
                protocol: Protocol::OpenAiResponses,
                base_url: "https://example.test".to_owned(),
                api_key: "secret".to_owned(),
                auth_style: AuthStyle::Bearer,
                model: "gpt-5.6-sol".to_owned(),
                context_tokens: 8_000,
                reasoning_effort: None,
                max_output_tokens: 16,
                anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
                thinking_budget_tokens: 1_024,
                enable_prompt_cache: true,
                prompt_cache_key: Some("rate-lens-test".to_owned()),
                prompt_marker: Some("run-1".to_owned()),
                disable_implicit_prompt_cache: false,
                openai_prompt_cache_options: true,
                timeout_seconds: 5,
            },
            8_000,
        );
        assert_eq!(openai["prompt_cache_options"]["mode"], "implicit");
        assert_eq!(openai["prompt_cache_options"]["ttl"], "30m");
        assert_eq!(openai["prompt_cache_key"], "rate-lens-test");

        let anthropic = build_payload(
            &ProbeConfig {
                protocol: Protocol::AnthropicMessages,
                base_url: "https://example.test".to_owned(),
                api_key: "secret".to_owned(),
                auth_style: AuthStyle::XApiKey,
                model: "claude-sonnet-4-6".to_owned(),
                context_tokens: 8_000,
                reasoning_effort: None,
                max_output_tokens: 16,
                anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
                thinking_budget_tokens: 1_024,
                enable_prompt_cache: true,
                prompt_cache_key: None,
                prompt_marker: Some("run-1".to_owned()),
                disable_implicit_prompt_cache: false,
                openai_prompt_cache_options: true,
                timeout_seconds: 5,
            },
            8_000,
        );
        assert_eq!(anthropic["cache_control"]["type"], "ephemeral");
        assert!(anthropic["cache_control"].get("ttl").is_none());
    }

    #[test]
    fn ordinary_payload_never_adds_cache_controls_without_cache_opt_in() {
        let payload = build_payload(
            &ProbeConfig {
                protocol: Protocol::OpenAiResponses,
                base_url: "https://example.test".to_owned(),
                api_key: "secret".to_owned(),
                auth_style: AuthStyle::Bearer,
                model: "gpt-5.6-sol".to_owned(),
                context_tokens: 8_000,
                reasoning_effort: None,
                max_output_tokens: 16,
                anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
                thinking_budget_tokens: 1_024,
                enable_prompt_cache: false,
                prompt_cache_key: None,
                prompt_marker: Some("run-1".to_owned()),
                disable_implicit_prompt_cache: false,
                openai_prompt_cache_options: true,
                timeout_seconds: 5,
            },
            8_000,
        );
        assert!(payload.get("prompt_cache_options").is_none());
        assert!(payload.get("prompt_cache_key").is_none());
    }

    #[test]
    fn cache_controls_are_omitted_without_relay_confirmation() {
        let payload = build_payload(
            &ProbeConfig {
                protocol: Protocol::OpenAiResponses,
                base_url: "https://example.test".to_owned(),
                api_key: "secret".to_owned(),
                auth_style: AuthStyle::Bearer,
                model: "gpt-5.6-sol".to_owned(),
                context_tokens: 8_000,
                reasoning_effort: None,
                max_output_tokens: 16,
                anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
                thinking_budget_tokens: 1_024,
                enable_prompt_cache: true,
                prompt_cache_key: Some("rate-lens-test".to_owned()),
                prompt_marker: Some("run-1".to_owned()),
                disable_implicit_prompt_cache: false,
                openai_prompt_cache_options: false,
                timeout_seconds: 5,
            },
            8_000,
        );
        assert!(payload.get("prompt_cache_options").is_none());
        assert!(payload.get("prompt_cache_key").is_none());
    }

    #[test]
    fn cache_probe_rejects_unconfirmed_relay_before_network() {
        let result = validate_config(&ProbeConfig {
            protocol: Protocol::OpenAiResponses,
            base_url: "https://example.test".to_owned(),
            api_key: "secret".to_owned(),
            auth_style: AuthStyle::Bearer,
            model: "gpt-5.6-sol".to_owned(),
            context_tokens: 8_000,
            reasoning_effort: None,
            max_output_tokens: 16,
            anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
            thinking_budget_tokens: 1_024,
            enable_prompt_cache: true,
            prompt_cache_key: Some("rate-lens-test".to_owned()),
            prompt_marker: Some("run-1".to_owned()),
            disable_implicit_prompt_cache: false,
            openai_prompt_cache_options: false,
            timeout_seconds: 5,
        });
        assert!(matches!(
            result,
            Err(ProbeError::PromptCacheSupportNotConfirmed)
        ));
    }

    #[test]
    fn lists_models_and_reads_capabilities() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_mock_server(
            vec![(
                "/v1/models?limit=1000",
                json!({
                    "data": [{
                        "id": "claude-sonnet-4-6",
                        "display_name": "Claude Sonnet 4.6",
                        "capabilities": {"max_input_tokens": 1000000, "max_tokens": 64000}
                    }]
                }),
            )],
            Arc::clone(&seen),
        );
        let models = list_models(
            Protocol::AnthropicMessages,
            &base_url,
            "secret",
            AuthStyle::XApiKey,
            5,
        )
        .unwrap();
        assert_eq!(models[0].id, "claude-sonnet-4-6");
        assert_eq!(models[0].max_input_tokens, Some(1_000_000));
        assert_eq!(models[0].max_output_tokens, Some(64_000));
    }

    fn spawn_mock_server(
        responses: Vec<(&'static str, Value)>,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for (expected_path, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_http_request(&mut stream);
                let first_line = request.lines().next().unwrap_or_default();
                assert!(
                    first_line.split_whitespace().nth(1) == Some(expected_path),
                    "expected {expected_path}, got {first_line}"
                );
                seen.lock().unwrap().push(request);
                write_json_response(&mut stream, &response);
            }
        });
        format!("http://{address}")
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4_096];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "connection closed before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(index) = find_bytes(&bytes, b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "connection closed before request body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn write_json_response(stream: &mut TcpStream, response: &Value) {
        let body = serde_json::to_string(response).unwrap();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    fn request_json_body(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
