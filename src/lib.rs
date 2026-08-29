mod catalog;
mod exchange;
mod parser;
mod pricing;
mod probe;

pub use catalog::{
    ANTHROPIC_PRICING_SOURCE, CATALOG_AS_OF, CatalogError, CatalogModel, OPENAI_PRICING_SOURCE,
    PriceTier, ResolvedPricing, catalog_models, resolve_pricing,
};
pub use exchange::{
    EXCHANGE_RATE_SOURCE, ExchangeRateError, ExchangeRateQuote, fetch_usd_exchange_rate,
    normalize_currency,
};
pub use parser::{ParseError, ParseReport, ProtocolHint, parse_usage};
pub use pricing::{
    Analysis, CostBreakdown, Pricing, PricingError, TokenCost, analyze_usage, calculate_multiplier,
};
pub use probe::{
    AnthropicThinkingMode, AuthStyle, DiscoveredModel, ProbeConfig, ProbeError, ProbeResult,
    approximate_official_input_cost, list_models, normalize_api_root, run_probe,
};

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The wire protocol from which usage was extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    AnthropicMessages,
}

impl fmt::Display for Protocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenAiResponses => formatter.write_str("OpenAI Responses"),
            Self::AnthropicMessages => formatter.write_str("Anthropic Messages"),
        }
    }
}

/// Usage normalized into mutually exclusive billing buckets.
///
/// `reasoning_output_tokens` is informational and is already included in
/// `output_tokens`; it must not be billed a second time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedUsage {
    pub protocol: Protocol,
    pub models: BTreeSet<String>,
    pub service_tiers: BTreeSet<String>,
    pub requests: u64,
    pub uncached_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub cache_write_5m_input_tokens: u64,
    pub cache_write_1h_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    /// Provider-metered units which may carry a non-token fee, such as web
    /// search requests or compute units.
    pub metered_extras: BTreeMap<String, u64>,
}

impl NormalizedUsage {
    pub fn total_input_tokens(&self) -> u64 {
        self.uncached_input_tokens
            + self.cache_read_input_tokens
            + self.cache_write_input_tokens
            + self.cache_write_5m_input_tokens
            + self.cache_write_1h_input_tokens
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens() + self.output_tokens
    }
}
