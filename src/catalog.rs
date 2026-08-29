use crate::{Pricing, Protocol};
use rust_decimal::Decimal;
use std::str::FromStr;
use thiserror::Error;

pub const CATALOG_AS_OF: &str = "2026-08-29";
pub const OPENAI_PRICING_SOURCE: &str = "https://developers.openai.com/api/docs/pricing";
pub const ANTHROPIC_PRICING_SOURCE: &str =
    "https://platform.claude.com/docs/en/about-claude/pricing";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceTier {
    Auto,
    Standard,
    Long,
}

#[derive(Debug, Clone)]
pub struct ResolvedPricing {
    pub official_model: &'static str,
    pub display_name: &'static str,
    pub pricing: Pricing,
    pub tier: &'static str,
    pub source: &'static str,
    pub as_of: &'static str,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct CatalogModel {
    pub protocol: Protocol,
    pub id: &'static str,
    pub display_name: &'static str,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CatalogError {
    #[error(
        "官方价格目录中没有模型 `{model}`；请使用 --official-model 指定对照模型，或手动提供 --input-rate 和 --output-rate"
    )]
    UnknownModel { model: String },
    #[error("模型 `{model}` 没有独立的长上下文价格档")]
    NoLongTier { model: String },
}

#[derive(Clone, Copy)]
struct Rates {
    input: &'static str,
    cache_read: Option<&'static str>,
    cache_write: Option<&'static str>,
    cache_write_5m: Option<&'static str>,
    cache_write_1h: Option<&'static str>,
    output: &'static str,
}

#[derive(Clone, Copy)]
struct Entry {
    protocol: Protocol,
    id: &'static str,
    display_name: &'static str,
    aliases: &'static [&'static str],
    standard: Rates,
    long: Option<Rates>,
    long_threshold: Option<u64>,
}

const fn openai(
    input: &'static str,
    cache_read: &'static str,
    cache_write: Option<&'static str>,
    output: &'static str,
) -> Rates {
    Rates {
        input,
        cache_read: Some(cache_read),
        cache_write,
        cache_write_5m: None,
        cache_write_1h: None,
        output,
    }
}

const fn anthropic(
    input: &'static str,
    cache_read: &'static str,
    cache_write_5m: &'static str,
    cache_write_1h: &'static str,
    output: &'static str,
) -> Rates {
    Rates {
        input,
        cache_read: Some(cache_read),
        cache_write: None,
        cache_write_5m: Some(cache_write_5m),
        cache_write_1h: Some(cache_write_1h),
        output,
    }
}

const ENTRIES: &[Entry] = &[
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.6-sol",
        display_name: "GPT-5.6 Sol",
        aliases: &["gpt-5.6-sol"],
        standard: openai("4", "0.4", Some("5"), "20"),
        long: Some(openai("8", "0.8", Some("10"), "30")),
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.6-terra",
        display_name: "GPT-5.6 Terra",
        aliases: &["gpt-5.6-terra"],
        standard: openai("2", "0.2", Some("2.5"), "12"),
        long: Some(openai("4", "0.4", Some("5"), "18")),
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.6-luna",
        display_name: "GPT-5.6 Luna",
        aliases: &["gpt-5.6-luna"],
        standard: openai("0.2", "0.02", Some("0.25"), "1.2"),
        long: Some(openai("0.4", "0.04", Some("0.5"), "1.8")),
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.5",
        display_name: "GPT-5.5",
        aliases: &["gpt-5.5"],
        standard: openai("5", "0.5", None, "30"),
        long: Some(openai("10", "1", None, "45")),
        long_threshold: Some(272_000),
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.4-mini",
        display_name: "GPT-5.4 mini",
        aliases: &["gpt-5.4-mini"],
        standard: openai("0.75", "0.075", None, "4.5"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.4-nano",
        display_name: "GPT-5.4 nano",
        aliases: &["gpt-5.4-nano"],
        standard: openai("0.2", "0.02", None, "1.25"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.4",
        display_name: "GPT-5.4",
        aliases: &["gpt-5.4"],
        standard: openai("2.5", "0.25", None, "15"),
        long: Some(openai("5", "0.5", None, "22.5")),
        long_threshold: Some(272_000),
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.3-codex",
        display_name: "GPT-5.3 Codex",
        aliases: &["gpt-5.3-codex"],
        standard: openai("1.75", "0.175", None, "14"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.2",
        display_name: "GPT-5.2",
        aliases: &["gpt-5.2"],
        standard: openai("1.75", "0.175", None, "14"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.1",
        display_name: "GPT-5.1",
        aliases: &["gpt-5.1"],
        standard: openai("1.25", "0.125", None, "10"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5-mini",
        display_name: "GPT-5 mini",
        aliases: &["gpt-5-mini"],
        standard: openai("0.25", "0.025", None, "2"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5-nano",
        display_name: "GPT-5 nano",
        aliases: &["gpt-5-nano"],
        standard: openai("0.05", "0.005", None, "0.4"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5",
        display_name: "GPT-5",
        aliases: &["gpt-5"],
        standard: openai("1.25", "0.125", None, "10"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-4.1-mini",
        display_name: "GPT-4.1 mini",
        aliases: &["gpt-4.1-mini"],
        standard: openai("0.4", "0.1", None, "1.6"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-4.1-nano",
        display_name: "GPT-4.1 nano",
        aliases: &["gpt-4.1-nano"],
        standard: openai("0.1", "0.025", None, "0.4"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-4.1",
        display_name: "GPT-4.1",
        aliases: &["gpt-4.1"],
        standard: openai("2", "0.5", None, "8"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-4o-mini",
        display_name: "GPT-4o mini",
        aliases: &["gpt-4o-mini"],
        standard: openai("0.15", "0.075", None, "0.6"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-4o",
        display_name: "GPT-4o",
        aliases: &["gpt-4o"],
        standard: openai("2.5", "1.25", None, "10"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "o4-mini",
        display_name: "o4-mini",
        aliases: &["o4-mini"],
        standard: openai("1.1", "0.275", None, "4.4"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "o3",
        display_name: "o3",
        aliases: &["o3"],
        standard: openai("2", "0.5", None, "8"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "o1",
        display_name: "o1",
        aliases: &["o1"],
        standard: openai("15", "7.5", None, "60"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-fable-5",
        display_name: "Claude Fable 5",
        aliases: &["claude-fable-5", "fable-5"],
        standard: anthropic("10", "1", "12.5", "20", "50"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-mythos-5",
        display_name: "Claude Mythos 5",
        aliases: &["claude-mythos-5", "mythos-5"],
        standard: anthropic("10", "1", "12.5", "20", "50"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-5",
        display_name: "Claude Opus 5",
        aliases: &["claude-opus-5", "opus-5"],
        standard: anthropic("5", "0.5", "6.25", "10", "25"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-4.8",
        display_name: "Claude Opus 4.8",
        aliases: &["claude-opus-4.8", "claude-opus-4-8", "opus-4.8"],
        standard: anthropic("5", "0.5", "6.25", "10", "25"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-4.7",
        display_name: "Claude Opus 4.7",
        aliases: &["claude-opus-4.7", "claude-opus-4-7", "opus-4.7"],
        standard: anthropic("5", "0.5", "6.25", "10", "25"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-4.6",
        display_name: "Claude Opus 4.6",
        aliases: &["claude-opus-4.6", "claude-opus-4-6", "opus-4.6"],
        standard: anthropic("5", "0.5", "6.25", "10", "25"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-4.5",
        display_name: "Claude Opus 4.5",
        aliases: &["claude-opus-4.5", "claude-opus-4-5", "opus-4.5"],
        standard: anthropic("5", "0.5", "6.25", "10", "25"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        aliases: &["claude-sonnet-5", "sonnet-5"],
        standard: anthropic("2", "0.2", "2.5", "4", "10"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-sonnet-4.6",
        display_name: "Claude Sonnet 4.6",
        aliases: &["claude-sonnet-4.6", "claude-sonnet-4-6", "sonnet-4.6"],
        standard: anthropic("3", "0.3", "3.75", "6", "15"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-sonnet-4.5",
        display_name: "Claude Sonnet 4.5",
        aliases: &["claude-sonnet-4.5", "claude-sonnet-4-5", "sonnet-4.5"],
        standard: anthropic("3", "0.3", "3.75", "6", "15"),
        long: Some(anthropic("6", "0.6", "7.5", "12", "22.5")),
        long_threshold: Some(200_000),
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-sonnet-4",
        display_name: "Claude Sonnet 4",
        aliases: &["claude-sonnet-4", "sonnet-4"],
        standard: anthropic("3", "0.3", "3.75", "6", "15"),
        long: Some(anthropic("6", "0.6", "7.5", "12", "22.5")),
        long_threshold: Some(200_000),
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-haiku-4.5",
        display_name: "Claude Haiku 4.5",
        aliases: &["claude-haiku-4.5", "claude-haiku-4-5", "haiku-4.5"],
        standard: anthropic("1", "0.1", "1.25", "2", "5"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-haiku-3.5",
        display_name: "Claude Haiku 3.5",
        aliases: &["claude-haiku-3.5", "claude-3-5-haiku", "haiku-3.5"],
        standard: anthropic("0.8", "0.08", "1", "1.6", "4"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-haiku-3",
        display_name: "Claude Haiku 3",
        aliases: &["claude-haiku-3", "claude-3-haiku", "haiku-3"],
        standard: anthropic("0.25", "0.025", "0.3125", "0.5", "1.25"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-sonnet-3.7",
        display_name: "Claude Sonnet 3.7",
        aliases: &["claude-sonnet-3.7", "claude-3-7-sonnet", "sonnet-3.7"],
        standard: anthropic("3", "0.3", "3.75", "6", "15"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-sonnet-3.5",
        display_name: "Claude Sonnet 3.5",
        aliases: &["claude-sonnet-3.5", "claude-3-5-sonnet", "sonnet-3.5"],
        standard: anthropic("3", "0.3", "3.75", "6", "15"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-4.1",
        display_name: "Claude Opus 4.1",
        aliases: &["claude-opus-4.1", "claude-opus-4-1", "opus-4.1"],
        standard: anthropic("15", "1.5", "18.75", "30", "75"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-4",
        display_name: "Claude Opus 4",
        aliases: &["claude-opus-4", "opus-4"],
        standard: anthropic("15", "1.5", "18.75", "30", "75"),
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-opus-3",
        display_name: "Claude Opus 3",
        aliases: &["claude-opus-3", "claude-3-opus", "opus-3"],
        standard: anthropic("15", "1.5", "18.75", "30", "75"),
        long: None,
        long_threshold: None,
    },
];

pub fn catalog_models(protocol: Protocol) -> Vec<CatalogModel> {
    ENTRIES
        .iter()
        .filter(|entry| entry.protocol == protocol)
        .map(|entry| CatalogModel {
            protocol: entry.protocol,
            id: entry.id,
            display_name: entry.display_name,
        })
        .collect()
}

pub fn resolve_pricing(
    protocol: Protocol,
    model: &str,
    input_tokens: u64,
    tier: PriceTier,
) -> Result<ResolvedPricing, CatalogError> {
    let entry = find_entry(protocol, model).ok_or_else(|| CatalogError::UnknownModel {
        model: model.to_owned(),
    })?;
    let mut warnings = Vec::new();
    let (rates, tier_name) = match tier {
        PriceTier::Standard => (entry.standard, "standard"),
        PriceTier::Long => (
            entry.long.ok_or_else(|| CatalogError::NoLongTier {
                model: entry.id.to_owned(),
            })?,
            "long",
        ),
        PriceTier::Auto => match (entry.long, entry.long_threshold) {
            (Some(long), Some(threshold)) if input_tokens >= threshold => (long, "long"),
            (Some(_), None) => {
                warnings.push(format!(
                    "{} 的官方价格页列出了短/长上下文两档，但目录未确认切换阈值；本次按 standard 档计算。长上下文请求请显式使用 --price-tier long。",
                    entry.display_name
                ));
                (entry.standard, "standard")
            }
            _ => (entry.standard, "standard"),
        },
    };

    Ok(ResolvedPricing {
        official_model: entry.id,
        display_name: entry.display_name,
        pricing: rates.into_pricing(),
        tier: tier_name,
        source: match protocol {
            Protocol::OpenAiResponses => OPENAI_PRICING_SOURCE,
            Protocol::AnthropicMessages => ANTHROPIC_PRICING_SOURCE,
        },
        as_of: CATALOG_AS_OF,
        warnings,
    })
}

fn find_entry(protocol: Protocol, model: &str) -> Option<&'static Entry> {
    let normalized = normalize_model(model.rsplit('/').next().unwrap_or(model));
    ENTRIES
        .iter()
        .filter(|entry| entry.protocol == protocol)
        .find(|entry| {
            entry.aliases.iter().any(|alias| {
                let alias = normalize_model(alias);
                normalized == alias || is_snapshot_of(&normalized, &alias)
            })
        })
}

fn is_snapshot_of(model: &str, alias: &str) -> bool {
    model
        .strip_prefix(alias)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '-')
        })
}

fn normalize_model(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut last_dash = false;
    for character in value.trim().chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            last_dash = false;
        } else if !last_dash && !result.is_empty() {
            result.push('-');
            last_dash = true;
        }
    }
    while result.ends_with('-') {
        result.pop();
    }
    result
}

impl Rates {
    fn into_pricing(self) -> Pricing {
        Pricing {
            uncached_input_per_million: decimal(self.input),
            cache_read_per_million: self.cache_read.map(decimal),
            cache_write_per_million: self.cache_write.map(decimal),
            cache_write_5m_per_million: self.cache_write_5m.map(decimal),
            cache_write_1h_per_million: self.cache_write_1h.map(decimal),
            output_per_million: decimal(self.output),
            extra_official_cost: Decimal::ZERO,
        }
    }
}

fn decimal(value: &str) -> Decimal {
    Decimal::from_str(value).expect("catalog prices are valid decimals")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_dated_and_namespaced_model_aliases() {
        let result = resolve_pricing(
            Protocol::AnthropicMessages,
            "anthropic/claude-sonnet-4-5-20250929",
            1_000,
            PriceTier::Auto,
        )
        .unwrap();
        assert_eq!(result.official_model, "claude-sonnet-4.5");
        assert_eq!(result.pricing.output_per_million, decimal("15"));
    }

    #[test]
    fn does_not_confuse_family_variants() {
        let result = resolve_pricing(
            Protocol::OpenAiResponses,
            "gpt-5-mini",
            1_000,
            PriceTier::Auto,
        )
        .unwrap();
        assert_eq!(result.official_model, "gpt-5-mini");
        assert_eq!(result.pricing.uncached_input_per_million, decimal("0.25"));
    }

    #[test]
    fn selects_known_long_context_tier() {
        let result = resolve_pricing(
            Protocol::OpenAiResponses,
            "gpt-5.4",
            300_000,
            PriceTier::Auto,
        )
        .unwrap();
        assert_eq!(result.tier, "long");
        assert_eq!(result.pricing.uncached_input_per_million, decimal("5"));
    }
}
