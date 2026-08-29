use crate::NormalizedUsage;
use rust_decimal::Decimal;
use serde::Serialize;
use thiserror::Error;

const ONE_MILLION: u64 = 1_000_000;

/// Prices in a single reference currency. Token prices are per one million
/// tokens, matching the way both providers publish model prices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pricing {
    pub uncached_input_per_million: Decimal,
    pub cache_read_per_million: Option<Decimal>,
    pub cache_write_per_million: Option<Decimal>,
    pub cache_write_5m_per_million: Option<Decimal>,
    pub cache_write_1h_per_million: Option<Decimal>,
    pub output_per_million: Decimal,
    /// Known non-token provider fees in the reference currency.
    pub extra_official_cost: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenCost {
    pub tokens: u64,
    pub rate_per_million: Decimal,
    pub reference_cost: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CostBreakdown {
    pub uncached_input: TokenCost,
    pub cache_read_input: Option<TokenCost>,
    pub cache_write_input: Option<TokenCost>,
    pub cache_write_5m_input: Option<TokenCost>,
    pub cache_write_1h_input: Option<TokenCost>,
    pub output: TokenCost,
    pub extra_official_cost: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Analysis {
    pub usage: NormalizedUsage,
    pub costs: CostBreakdown,
    pub official_cost_reference: Decimal,
    /// Reference-currency cost converted to the relay balance currency.
    pub official_cost_actual_currency: Decimal,
    pub charged_actual_currency: Option<Decimal>,
    /// Actual-currency units per one reference-currency unit.
    pub exchange_rate: Decimal,
    pub observed_multiplier: Option<Decimal>,
    pub difference_actual_currency: Option<Decimal>,
    pub markup_percent: Option<Decimal>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PricingError {
    #[error("{field} 不能为负数")]
    NegativeValue { field: &'static str },
    #[error("汇率必须大于 0")]
    InvalidExchangeRate,
    #[error("存在 {tokens} 个 {bucket}，但没有提供相应价格")]
    MissingRate { bucket: &'static str, tokens: u64 },
    #[error("官方理论成本为 0，无法计算倍率")]
    ZeroOfficialCost,
}

pub fn calculate_multiplier(
    usage: NormalizedUsage,
    pricing: Pricing,
    charged_actual_currency: Decimal,
    exchange_rate: Decimal,
) -> Result<Analysis, PricingError> {
    analyze_usage(usage, pricing, Some(charged_actual_currency), exchange_rate)
}

/// Calculates the official reference cost and, when `charged_actual_currency`
/// is supplied, the relay's observed multiplier.
pub fn analyze_usage(
    usage: NormalizedUsage,
    pricing: Pricing,
    charged_actual_currency: Option<Decimal>,
    exchange_rate: Decimal,
) -> Result<Analysis, PricingError> {
    validate_nonnegative("普通输入价格", pricing.uncached_input_per_million)?;
    validate_optional("缓存读取价格", pricing.cache_read_per_million)?;
    validate_optional("缓存写入价格", pricing.cache_write_per_million)?;
    validate_optional("5 分钟缓存写入价格", pricing.cache_write_5m_per_million)?;
    validate_optional("1 小时缓存写入价格", pricing.cache_write_1h_per_million)?;
    validate_nonnegative("输出价格", pricing.output_per_million)?;
    validate_nonnegative("额外官方费用", pricing.extra_official_cost)?;
    validate_optional("实际扣费", charged_actual_currency)?;
    if exchange_rate <= Decimal::ZERO {
        return Err(PricingError::InvalidExchangeRate);
    }

    let uncached_input = token_cost(
        usage.uncached_input_tokens,
        pricing.uncached_input_per_million,
    );
    let cache_read_input = optional_token_cost(
        "缓存读取输入 token",
        usage.cache_read_input_tokens,
        pricing.cache_read_per_million,
    )?;
    let cache_write_input = optional_token_cost(
        "未细分缓存写入输入 token",
        usage.cache_write_input_tokens,
        pricing.cache_write_per_million,
    )?;
    let cache_write_5m_input = fallback_token_cost(
        "5 分钟缓存写入输入 token",
        usage.cache_write_5m_input_tokens,
        pricing.cache_write_5m_per_million,
        pricing.cache_write_per_million,
    )?;
    let cache_write_1h_input = fallback_token_cost(
        "1 小时缓存写入输入 token",
        usage.cache_write_1h_input_tokens,
        pricing.cache_write_1h_per_million,
        pricing.cache_write_per_million,
    )?;
    let output = token_cost(usage.output_tokens, pricing.output_per_million);

    let token_costs = uncached_input.reference_cost
        + reference_cost(&cache_read_input)
        + reference_cost(&cache_write_input)
        + reference_cost(&cache_write_5m_input)
        + reference_cost(&cache_write_1h_input)
        + output.reference_cost;
    let official_cost_reference = token_costs + pricing.extra_official_cost;
    if charged_actual_currency.is_some() && official_cost_reference <= Decimal::ZERO {
        return Err(PricingError::ZeroOfficialCost);
    }

    let official_cost_actual_currency = official_cost_reference * exchange_rate;
    let observed_multiplier =
        charged_actual_currency.map(|charged| charged / official_cost_actual_currency);
    let difference_actual_currency =
        charged_actual_currency.map(|charged| charged - official_cost_actual_currency);
    let markup_percent =
        observed_multiplier.map(|multiplier| (multiplier - Decimal::ONE) * Decimal::from(100));

    Ok(Analysis {
        usage,
        costs: CostBreakdown {
            uncached_input,
            cache_read_input,
            cache_write_input,
            cache_write_5m_input,
            cache_write_1h_input,
            output,
            extra_official_cost: pricing.extra_official_cost,
        },
        official_cost_reference,
        official_cost_actual_currency,
        charged_actual_currency,
        exchange_rate,
        observed_multiplier,
        difference_actual_currency,
        markup_percent,
    })
}

fn token_cost(tokens: u64, rate_per_million: Decimal) -> TokenCost {
    TokenCost {
        tokens,
        rate_per_million,
        reference_cost: Decimal::from(tokens) * rate_per_million / Decimal::from(ONE_MILLION),
    }
}

fn optional_token_cost(
    bucket: &'static str,
    tokens: u64,
    rate: Option<Decimal>,
) -> Result<Option<TokenCost>, PricingError> {
    if tokens == 0 {
        return Ok(None);
    }
    rate.map(|value| token_cost(tokens, value))
        .map(Some)
        .ok_or(PricingError::MissingRate { bucket, tokens })
}

fn fallback_token_cost(
    bucket: &'static str,
    tokens: u64,
    specific_rate: Option<Decimal>,
    fallback_rate: Option<Decimal>,
) -> Result<Option<TokenCost>, PricingError> {
    optional_token_cost(bucket, tokens, specific_rate.or(fallback_rate))
}

fn reference_cost(cost: &Option<TokenCost>) -> Decimal {
    cost.as_ref()
        .map(|item| item.reference_cost)
        .unwrap_or(Decimal::ZERO)
}

fn validate_optional(field: &'static str, value: Option<Decimal>) -> Result<(), PricingError> {
    if let Some(value) = value {
        validate_nonnegative(field, value)?;
    }
    Ok(())
}

fn validate_nonnegative(field: &'static str, value: Decimal) -> Result<(), PricingError> {
    if value < Decimal::ZERO {
        return Err(PricingError::NegativeValue { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Protocol;
    use std::collections::{BTreeMap, BTreeSet};
    use std::str::FromStr;

    fn decimal(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn usage() -> NormalizedUsage {
        NormalizedUsage {
            protocol: Protocol::OpenAiResponses,
            models: BTreeSet::from(["test-model".to_owned()]),
            service_tiers: BTreeSet::new(),
            requests: 1,
            uncached_input_tokens: 1_000_000,
            cache_read_input_tokens: 500_000,
            cache_write_input_tokens: 0,
            cache_write_5m_input_tokens: 0,
            cache_write_1h_input_tokens: 0,
            output_tokens: 250_000,
            reasoning_output_tokens: 100_000,
            metered_extras: BTreeMap::new(),
        }
    }

    #[test]
    fn calculates_exact_multiplier() {
        let result = calculate_multiplier(
            usage(),
            Pricing {
                uncached_input_per_million: decimal("2"),
                cache_read_per_million: Some(decimal("0.2")),
                cache_write_per_million: None,
                cache_write_5m_per_million: None,
                cache_write_1h_per_million: None,
                output_per_million: decimal("8"),
                extra_official_cost: Decimal::ZERO,
            },
            decimal("43.05"),
            decimal("10.5"),
        )
        .unwrap();

        assert_eq!(result.official_cost_reference, decimal("4.1"));
        assert_eq!(result.official_cost_actual_currency, decimal("43.05"));
        assert_eq!(result.observed_multiplier, Some(Decimal::ONE));
        assert_eq!(result.markup_percent, Some(Decimal::ZERO));
    }

    #[test]
    fn requires_rate_only_for_a_nonempty_bucket() {
        let error = calculate_multiplier(
            usage(),
            Pricing {
                uncached_input_per_million: decimal("2"),
                cache_read_per_million: None,
                cache_write_per_million: None,
                cache_write_5m_per_million: None,
                cache_write_1h_per_million: None,
                output_per_million: decimal("8"),
                extra_official_cost: Decimal::ZERO,
            },
            decimal("1"),
            Decimal::ONE,
        )
        .unwrap_err();

        assert!(matches!(error, PricingError::MissingRate { .. }));
    }

    #[test]
    fn calculates_official_cost_without_a_relay_charge() {
        let result = analyze_usage(
            usage(),
            Pricing {
                uncached_input_per_million: decimal("2"),
                cache_read_per_million: Some(decimal("0.2")),
                cache_write_per_million: None,
                cache_write_5m_per_million: None,
                cache_write_1h_per_million: None,
                output_per_million: decimal("8"),
                extra_official_cost: Decimal::ZERO,
            },
            None,
            Decimal::ONE,
        )
        .unwrap();

        assert_eq!(result.official_cost_reference, decimal("4.1"));
        assert_eq!(result.charged_actual_currency, None);
        assert_eq!(result.observed_multiplier, None);
    }
}
