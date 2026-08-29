use crate::{Pricing, Protocol};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use reqwest::redirect::Policy;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const CATALOG_AS_OF: &str = "2026-08-29";
pub const OPENAI_PRICING_SOURCE: &str = "https://developers.openai.com/api/docs/pricing";
pub const ANTHROPIC_PRICING_SOURCE: &str =
    "https://platform.claude.com/docs/en/about-claude/pricing";
const OPENAI_PRICING_MARKDOWN: &str = "https://developers.openai.com/api/docs/pricing.md";
const ANTHROPIC_PRICING_MARKDOWN: &str =
    "https://platform.claude.com/docs/en/about-claude/pricing.md";
const OPENAI_MODEL_MARKDOWN_ROOT: &str = "https://developers.openai.com/api/docs/models";
const CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_PRICING_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_MODEL_DOCUMENT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceTier {
    Auto,
    Standard,
    Long,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PricingSourceMode {
    #[default]
    Auto,
    Live,
    Builtin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSourceKind {
    Live,
    Cache,
    Builtin,
}

impl CatalogSourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Cache => "cache",
            Self::Builtin => "builtin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CatalogLoadOptions {
    pub source: PricingSourceMode,
    pub timeout_seconds: u64,
    pub cache_dir: Option<PathBuf>,
}

impl Default for CatalogLoadOptions {
    fn default() -> Self {
        Self {
            source: PricingSourceMode::Auto,
            timeout_seconds: 20,
            cache_dir: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPricing {
    pub official_model: String,
    pub display_name: String,
    pub pricing: Pricing,
    pub tier: String,
    pub source: String,
    pub source_kind: CatalogSourceKind,
    pub as_of: String,
    pub fetched_at: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub protocol: Protocol,
    pub id: String,
    pub display_name: String,
    pub source_kind: CatalogSourceKind,
    pub source: String,
    pub as_of: String,
    pub fetched_at: Option<String>,
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

#[derive(Debug, Error)]
pub enum CatalogLoadError {
    #[error("无法实时获取 {provider} 官方价格：{reason}")]
    LiveUnavailable {
        provider: &'static str,
        reason: String,
    },
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
        long_threshold: Some(272_000),
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.6-terra",
        display_name: "GPT-5.6 Terra",
        aliases: &["gpt-5.6-terra"],
        standard: openai("2", "0.2", Some("2.5"), "12"),
        long: Some(openai("4", "0.4", Some("5"), "18")),
        long_threshold: Some(272_000),
    },
    Entry {
        protocol: Protocol::OpenAiResponses,
        id: "gpt-5.6-luna",
        display_name: "GPT-5.6 Luna",
        aliases: &["gpt-5.6-luna"],
        standard: openai("0.2", "0.02", Some("0.25"), "1.2"),
        long: Some(openai("0.4", "0.04", Some("0.5"), "1.8")),
        long_threshold: Some(272_000),
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
        long: None,
        long_threshold: None,
    },
    Entry {
        protocol: Protocol::AnthropicMessages,
        id: "claude-sonnet-4",
        display_name: "Claude Sonnet 4",
        aliases: &["claude-sonnet-4", "sonnet-4"],
        standard: anthropic("3", "0.3", "3.75", "6", "15"),
        long: None,
        long_threshold: None,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRates {
    input: Decimal,
    cache_read: Option<Decimal>,
    cache_write: Option<Decimal>,
    cache_write_5m: Option<Decimal>,
    cache_write_1h: Option<Decimal>,
    output: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEntry {
    id: String,
    display_name: String,
    aliases: Vec<String>,
    standard: StoredRates,
    long: Option<StoredRates>,
    long_threshold: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    schema_version: u32,
    provider: String,
    source: String,
    fetched_at_unix: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    entries: Vec<StoredEntry>,
}

#[derive(Debug, Clone)]
struct CatalogMetadata {
    kind: CatalogSourceKind,
    source: String,
    as_of: String,
    fetched_at: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
}

#[derive(Debug, Clone)]
struct OwnedEntry {
    protocol: Protocol,
    id: String,
    display_name: String,
    aliases: Vec<String>,
    standard: StoredRates,
    long: Option<StoredRates>,
    long_threshold: Option<u64>,
    metadata: CatalogMetadata,
}

#[derive(Debug, Clone)]
pub struct PricingCatalog {
    entries: Vec<OwnedEntry>,
    warnings: Vec<String>,
}

impl PricingCatalog {
    pub fn builtin(protocol: Protocol) -> Self {
        Self {
            entries: builtin_entries(protocol),
            warnings: Vec::new(),
        }
    }

    pub fn models(&self) -> Vec<CatalogModel> {
        self.entries
            .iter()
            .map(|entry| CatalogModel {
                protocol: entry.protocol,
                id: entry.id.clone(),
                display_name: entry.display_name.clone(),
                source_kind: entry.metadata.kind,
                source: entry.metadata.source.clone(),
                as_of: entry.metadata.as_of.clone(),
                fetched_at: entry.metadata.fetched_at.clone(),
            })
            .collect()
    }

    pub fn source_summaries(&self) -> Vec<CatalogSourceSummary> {
        let mut seen = BTreeSet::new();
        self.entries
            .iter()
            .filter_map(|entry| {
                let key = format!(
                    "{}\u{0}{}\u{0}{}",
                    entry.metadata.kind.as_str(),
                    entry.metadata.as_of,
                    entry.metadata.source
                );
                seen.insert(key).then(|| CatalogSourceSummary {
                    kind: entry.metadata.kind,
                    source: entry.metadata.source.clone(),
                    as_of: entry.metadata.as_of.clone(),
                    fetched_at: entry.metadata.fetched_at.clone(),
                })
            })
            .collect()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn resolve(
        &self,
        model: &str,
        input_tokens: u64,
        tier: PriceTier,
    ) -> Result<ResolvedPricing, CatalogError> {
        let entry = self
            .find_entry(model)
            .ok_or_else(|| CatalogError::UnknownModel {
                model: model.to_owned(),
            })?;
        let mut warnings = self.warnings.clone();
        let (rates, tier_name) = match tier {
            PriceTier::Standard => (&entry.standard, "standard"),
            PriceTier::Long => (
                entry
                    .long
                    .as_ref()
                    .ok_or_else(|| CatalogError::NoLongTier {
                        model: entry.id.clone(),
                    })?,
                "long",
            ),
            PriceTier::Auto => match (&entry.long, entry.long_threshold) {
                (Some(long), Some(threshold)) if input_tokens > threshold => (long, "long"),
                (Some(_), None) => {
                    warnings.push(format!(
                        "{} 的官方价格页列出了短/长上下文两档，但未能从官方文档确认切换阈值；本次按 standard 档计算。长上下文请求请显式使用 --price-tier long。",
                        entry.display_name
                    ));
                    (&entry.standard, "standard")
                }
                _ => (&entry.standard, "standard"),
            },
        };

        Ok(ResolvedPricing {
            official_model: entry.id.clone(),
            display_name: entry.display_name.clone(),
            pricing: rates.to_pricing(),
            tier: tier_name.to_owned(),
            source: entry.metadata.source.clone(),
            source_kind: entry.metadata.kind,
            as_of: entry.metadata.as_of.clone(),
            fetched_at: entry.metadata.fetched_at.clone(),
            etag: entry.metadata.etag.clone(),
            last_modified: entry.metadata.last_modified.clone(),
            warnings,
        })
    }

    fn find_entry(&self, model: &str) -> Option<&OwnedEntry> {
        let normalized = normalize_model(model.rsplit('/').next().unwrap_or(model));
        self.entries
            .iter()
            .find(|entry| {
                entry
                    .aliases
                    .iter()
                    .any(|alias| normalized == normalize_model(alias))
            })
            .or_else(|| {
                self.entries.iter().find(|entry| {
                    entry
                        .aliases
                        .iter()
                        .any(|alias| is_snapshot_of(&normalized, &normalize_model(alias)))
                })
            })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogSourceSummary {
    pub kind: CatalogSourceKind,
    pub source: String,
    pub as_of: String,
    pub fetched_at: Option<String>,
}

pub fn load_pricing_catalog(
    protocol: Protocol,
    options: &CatalogLoadOptions,
) -> Result<PricingCatalog, CatalogLoadError> {
    if options.source == PricingSourceMode::Builtin {
        return Ok(PricingCatalog::builtin(protocol));
    }

    let cache_path = catalog_cache_path(protocol, options.cache_dir.as_deref());
    let (cached, mut warnings) = match cache_path.as_deref().map(read_cache_file) {
        Some(Ok(cache))
            if cache.as_ref().is_some_and(|cache| {
                cache.provider == provider_key(protocol) && cache.source == pricing_source(protocol)
            }) =>
        {
            (cache, Vec::new())
        }
        Some(Ok(Some(_))) => (
            None,
            vec!["本地官方价格缓存与当前协议不匹配，已忽略。".to_owned()],
        ),
        Some(Ok(None)) => (None, Vec::new()),
        Some(Err(error)) => (None, vec![format!("本地官方价格缓存无效，已忽略：{error}")]),
        None => (None, Vec::new()),
    };
    if cache_path.is_none() {
        warnings.push(
            "无法确定平台缓存目录；本次仍会使用实时价格，但不会保存本地缓存。可通过 --pricing-cache-dir 或 RATE_LENS_CACHE_DIR 指定目录。"
                .to_owned(),
        );
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(options.timeout_seconds.max(1)))
        .user_agent(concat!("rate-lens/", env!("CARGO_PKG_VERSION")))
        .redirect(Policy::none())
        .build();
    let live_result = match client {
        Ok(client) => fetch_live_catalog(protocol, &client, cached.as_ref()),
        Err(error) => Err(format!("无法创建 HTTP 客户端：{error}")),
    };

    match live_result {
        Ok(LiveCatalog::Fresh {
            cache,
            warnings: live_warnings,
        }) => {
            warnings.extend(live_warnings);
            if let Some(path) = cache_path.as_deref()
                && let Err(error) = write_cache_file(path, &cache)
            {
                warnings.push(format!("官方价格已获取，但写入本地缓存失败：{error}"));
            }
            Ok(catalog_from_cache(
                protocol,
                cache,
                CatalogSourceKind::Live,
                warnings,
            ))
        }
        Ok(LiveCatalog::NotModified) => {
            let cache = cached.expect("304 is only accepted when a validated cache exists");
            Ok(catalog_from_cache(
                protocol,
                cache,
                CatalogSourceKind::Cache,
                warnings,
            ))
        }
        Err(reason) if options.source == PricingSourceMode::Live => {
            Err(CatalogLoadError::LiveUnavailable {
                provider: provider_name(protocol),
                reason,
            })
        }
        Err(reason) => {
            if let Some(cache) = cached {
                warnings.push(format!(
                    "实时官方价格获取失败（{reason}）；已使用上次成功缓存。"
                ));
                Ok(catalog_from_cache(
                    protocol,
                    cache,
                    CatalogSourceKind::Cache,
                    warnings,
                ))
            } else {
                warnings.push(format!(
                    "实时官方价格获取失败（{reason}），且没有可用缓存；已回退到内置快照（截至 {CATALOG_AS_OF}）。"
                ));
                let mut catalog = PricingCatalog::builtin(protocol);
                catalog.warnings = warnings;
                Ok(catalog)
            }
        }
    }
}

enum LiveCatalog {
    Fresh {
        cache: CacheFile,
        warnings: Vec<String>,
    },
    NotModified,
}

fn fetch_live_catalog(
    protocol: Protocol,
    client: &Client,
    cached: Option<&CacheFile>,
) -> Result<LiveCatalog, String> {
    let url = pricing_markdown_source(protocol);
    let mut request = client.get(url);
    if let Some(cache) = cached {
        if let Some(etag) = cache.etag.as_deref() {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = cache.last_modified.as_deref() {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
    }
    let mut response = request
        .send()
        .map_err(|error| format!("请求 {url} 失败：{error}"))?;
    if response.status() == StatusCode::NOT_MODIFIED {
        return cached
            .map(|_| LiveCatalog::NotModified)
            .ok_or_else(|| "官方服务器返回 304，但本地没有可用缓存".to_owned());
    }
    if response.status().is_redirection() {
        return Err(format!(
            "{url} 返回 HTTP {} 重定向；为保证价格来源域名不变，已拒绝跟随",
            response.status()
        ));
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = read_limited(&mut response, 1_024).unwrap_or_default();
        return Err(format!("{url} 返回 HTTP {status}：{body}"));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type
        .to_ascii_lowercase()
        .starts_with("text/markdown")
    {
        return Err(format!(
            "{url} 返回了非 Markdown Content-Type `{content_type}`"
        ));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let last_modified = response
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = read_limited(&mut response, MAX_PRICING_DOCUMENT_BYTES)?;
    let mut entries = match protocol {
        Protocol::OpenAiResponses => parse_openai_pricing(&body),
        Protocol::AnthropicMessages => parse_anthropic_pricing(&body),
    }?;
    let warnings = match protocol {
        Protocol::OpenAiResponses => enrich_openai_long_thresholds(client, &mut entries),
        Protocol::AnthropicMessages => Vec::new(),
    };
    validate_remote_entries(protocol, &entries)?;
    Ok(LiveCatalog::Fresh {
        cache: CacheFile {
            schema_version: CACHE_SCHEMA_VERSION,
            provider: provider_key(protocol).to_owned(),
            source: pricing_source(protocol).to_owned(),
            fetched_at_unix: unix_timestamp(),
            etag,
            last_modified,
            entries,
        },
        warnings,
    })
}

fn catalog_from_cache(
    protocol: Protocol,
    cache: CacheFile,
    kind: CatalogSourceKind,
    warnings: Vec<String>,
) -> PricingCatalog {
    let fetched_at = format_unix_timestamp(cache.fetched_at_unix);
    let metadata = CatalogMetadata {
        kind,
        source: cache.source,
        as_of: fetched_at.get(..10).unwrap_or(&fetched_at).to_owned(),
        fetched_at: Some(fetched_at),
        etag: cache.etag,
        last_modified: cache.last_modified,
    };
    let mut warnings = warnings;
    let entries = merge_remote_entries(protocol, cache.entries, &metadata, &mut warnings);
    PricingCatalog { entries, warnings }
}

fn merge_remote_entries(
    protocol: Protocol,
    remote: Vec<StoredEntry>,
    metadata: &CatalogMetadata,
    warnings: &mut Vec<String>,
) -> Vec<OwnedEntry> {
    let mut builtin = builtin_entries(protocol);
    let mut merged = Vec::with_capacity(remote.len().max(builtin.len()));
    for mut entry in remote {
        let remote_key = normalize_model(&entry.id);
        if let Some(index) = builtin.iter().position(|candidate| {
            normalize_model(&candidate.id) == remote_key
                || normalize_model(&candidate.display_name) == remote_key
        }) {
            let existing = builtin.remove(index);
            entry.id = existing.id;
            entry.display_name = existing.display_name;
            entry.aliases.extend(existing.aliases);
            if entry.long_threshold.is_none() {
                entry.long_threshold = existing.long_threshold;
                if entry.long.is_some()
                    && entry.long_threshold.is_some()
                    && !warnings.iter().any(|warning| warning.contains(&entry.id))
                {
                    warnings.push(format!(
                        "{} 的实时价格页列出了长上下文价格，但在线模型页未提供可用阈值；切换阈值沿用内置快照（截至 {CATALOG_AS_OF}）。",
                        entry.id
                    ));
                }
            }
        }
        entry.aliases.push(entry.id.clone());
        deduplicate_aliases(&mut entry.aliases);
        merged.push(OwnedEntry {
            protocol,
            id: entry.id,
            display_name: entry.display_name,
            aliases: entry.aliases,
            standard: entry.standard,
            long: entry.long,
            long_threshold: entry.long_threshold,
            metadata: metadata.clone(),
        });
    }
    merged.extend(builtin);
    merged
}

fn builtin_entries(protocol: Protocol) -> Vec<OwnedEntry> {
    let metadata = CatalogMetadata {
        kind: CatalogSourceKind::Builtin,
        source: pricing_source(protocol).to_owned(),
        as_of: CATALOG_AS_OF.to_owned(),
        fetched_at: None,
        etag: None,
        last_modified: None,
    };
    ENTRIES
        .iter()
        .filter(|entry| entry.protocol == protocol)
        .map(|entry| OwnedEntry {
            protocol: entry.protocol,
            id: entry.id.to_owned(),
            display_name: entry.display_name.to_owned(),
            aliases: entry
                .aliases
                .iter()
                .map(|alias| (*alias).to_owned())
                .collect(),
            standard: StoredRates::from_static(entry.standard),
            long: entry.long.map(StoredRates::from_static),
            long_threshold: entry.long_threshold,
            metadata: metadata.clone(),
        })
        .collect()
}

fn parse_openai_pricing(body: &str) -> Result<Vec<StoredEntry>, String> {
    let standard_section = body
        .split_once("### Standard pricing data")
        .map(|(_, section)| section)
        .ok_or_else(|| "未找到 OpenAI Standard pricing data 段落".to_owned())?;
    let lines = standard_section.lines().collect::<Vec<_>>();
    let header = [
        "Model",
        "Short context input",
        "Short context cached input",
        "Short context cache writes",
        "Short context output",
        "Long context input",
        "Long context cached input",
        "Long context cache writes",
        "Long context output",
    ];
    let rows = find_table(&lines, &header)
        .ok_or_else(|| "未找到 OpenAI Standard pricing data 表".to_owned())?;
    let mut entries = Vec::new();
    for cells in rows {
        let original_model = &cells[0];
        let label = clean_model_label(original_model);
        let id = label
            .split(" (<")
            .next()
            .unwrap_or(&label)
            .trim()
            .to_owned();
        let display_name = id.clone();
        let Some(input) = parse_price(&cells[1])? else {
            continue;
        };
        let Some(output) = parse_price(&cells[4])? else {
            continue;
        };
        let standard = StoredRates {
            input,
            cache_read: parse_price(&cells[2])?,
            cache_write: parse_price(&cells[3])?,
            cache_write_5m: None,
            cache_write_1h: None,
            output,
        };
        let long_input = parse_price(&cells[5])?;
        let long_output = parse_price(&cells[8])?;
        let long = match (long_input, long_output) {
            (Some(input), Some(output)) => Some(StoredRates {
                input,
                cache_read: parse_price(&cells[6])?,
                cache_write: parse_price(&cells[7])?,
                cache_write_5m: None,
                cache_write_1h: None,
                output,
            }),
            (None, None) => None,
            _ => return Err(format!("模型 `{id}` 的 OpenAI 长上下文价格列不完整")),
        };
        entries.push(StoredEntry {
            aliases: vec![id.clone()],
            id,
            display_name,
            standard,
            long,
            long_threshold: parse_context_threshold(original_model),
        });
    }

    if let Some(section) = markdown_section(body, "Specialized models", "Finetuning") {
        let section_lines = section.lines().collect::<Vec<_>>();
        let specialized_header = ["Category", "Model", "Input", "Cached input", "Output"];
        if let Some(rows) = find_table(&section_lines, &specialized_header) {
            for cells in rows {
                let id = clean_model_label(&cells[1]);
                let (Some(input), Some(output)) =
                    (parse_price(&cells[2])?, parse_price(&cells[4])?)
                else {
                    continue;
                };
                if entries
                    .iter()
                    .any(|entry| normalize_model(&entry.id) == normalize_model(&id))
                {
                    continue;
                }
                entries.push(StoredEntry {
                    aliases: vec![id.clone()],
                    id: id.clone(),
                    display_name: id,
                    standard: StoredRates {
                        input,
                        cache_read: parse_price(&cells[3])?,
                        cache_write: None,
                        cache_write_5m: None,
                        cache_write_1h: None,
                        output,
                    },
                    long: None,
                    long_threshold: None,
                });
            }
        }
    }
    Ok(entries)
}

fn enrich_openai_long_thresholds(client: &Client, entries: &mut [StoredEntry]) -> Vec<String> {
    let mut warnings = Vec::new();
    for entry in entries
        .iter_mut()
        .filter(|entry| entry.long.is_some() && entry.long_threshold.is_none())
    {
        let url = format!("{OPENAI_MODEL_MARKDOWN_ROOT}/{}.md", entry.id);
        match fetch_openai_model_threshold(client, &url) {
            Ok(Some(threshold)) => entry.long_threshold = Some(threshold),
            Ok(None) => warnings.push(format!(
                "{} 的官方模型页未找到可确认的长上下文切换阈值；若内置快照也没有该规则，auto 将按 standard 计算。",
                entry.id
            )),
            Err(reason) => warnings.push(format!(
                "{} 的官方模型页阈值获取失败（{reason}）；若内置快照也没有该规则，auto 将按 standard 计算。",
                entry.id
            )),
        }
    }
    warnings
}

fn fetch_openai_model_threshold(client: &Client, url: &str) -> Result<Option<u64>, String> {
    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("请求 {url} 失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} 返回 HTTP {}", response.status()));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !content_type
        .to_ascii_lowercase()
        .starts_with("text/markdown")
    {
        return Err(format!(
            "{url} 返回了非 Markdown Content-Type `{content_type}`"
        ));
    }
    let body = read_limited(&mut response, MAX_MODEL_DOCUMENT_BYTES)?;
    Ok(parse_openai_model_threshold(&body))
}

fn parse_openai_model_threshold(body: &str) -> Option<u64> {
    body.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        let has_pricing_rule = (lower.contains("priced") || lower.contains("pricing"))
            && (lower.contains("input token") || lower.contains("context"));
        if has_pricing_rule {
            parse_k_threshold(line)
        } else {
            None
        }
    })
}

fn parse_anthropic_pricing(body: &str) -> Result<Vec<StoredEntry>, String> {
    let lines = body.lines().collect::<Vec<_>>();
    let header = [
        "Model",
        "Base Input Tokens",
        "5m Cache Writes",
        "1h Cache Writes",
        "Cache Hits & Refreshes",
        "Output Tokens",
    ];
    let rows = find_table(&lines, &header)
        .ok_or_else(|| "未找到 Anthropic Model pricing 表".to_owned())?;
    let mut entries = Vec::new();
    for cells in rows {
        let display_name = clean_model_label(&cells[0]);
        let id = normalize_model(&display_name);
        let (Some(input), Some(output)) = (parse_price(&cells[1])?, parse_price(&cells[5])?) else {
            continue;
        };
        entries.push(StoredEntry {
            aliases: vec![id.clone()],
            id,
            display_name,
            standard: StoredRates {
                input,
                cache_read: parse_price(&cells[4])?,
                cache_write: None,
                cache_write_5m: parse_price(&cells[2])?,
                cache_write_1h: parse_price(&cells[3])?,
                output,
            },
            long: None,
            long_threshold: None,
        });
    }
    Ok(entries)
}

fn find_table(lines: &[&str], expected_header: &[&str]) -> Option<Vec<Vec<String>>> {
    let start = lines.iter().position(|line| {
        let cells = split_markdown_row(line);
        cells.len() == expected_header.len()
            && cells
                .iter()
                .zip(expected_header)
                .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
    })?;
    if lines
        .get(start + 1)
        .is_none_or(|line| !is_markdown_separator(line, expected_header.len()))
    {
        return None;
    }
    let mut rows = Vec::new();
    for line in lines.iter().skip(start + 2) {
        if !line.trim_start().starts_with('|') {
            break;
        }
        let cells = split_markdown_row(line);
        if cells.len() != expected_header.len() {
            break;
        }
        rows.push(cells);
    }
    Some(rows)
}

fn is_markdown_separator(line: &str, columns: usize) -> bool {
    let cells = split_markdown_row(line);
    cells.len() == columns
        && cells.iter().all(|cell| {
            let cell = cell.trim_matches(':').trim();
            cell.len() >= 3 && cell.chars().all(|character| character == '-')
        })
}

fn split_markdown_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

fn clean_model_label(value: &str) -> String {
    value
        .split(" ([")
        .next()
        .unwrap_or(value)
        .trim()
        .trim_matches('`')
        .to_owned()
}

fn parse_price(value: &str) -> Result<Option<Decimal>, String> {
    let value = value.trim();
    if value == "-" || value.is_empty() {
        return Ok(None);
    }
    if value.eq_ignore_ascii_case("free") {
        return Ok(Some(Decimal::ZERO));
    }
    let number = value
        .strip_prefix('$')
        .ok_or_else(|| format!("官方价格单元格缺少美元符号：`{value}`"))?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .replace(',', "");
    let price =
        Decimal::from_str(&number).map_err(|_| format!("无法解析官方价格单元格 `{value}`"))?;
    if price < Decimal::ZERO {
        return Err(format!("官方价格不能为负数：`{value}`"));
    }
    Ok(Some(price))
}

fn parse_context_threshold(value: &str) -> Option<u64> {
    let lower = value.to_ascii_lowercase();
    if !lower.contains("context") {
        return None;
    }
    parse_k_threshold(value)
}

fn parse_k_threshold(value: &str) -> Option<u64> {
    let lower = value.to_ascii_lowercase();
    let k_index = lower.find('k')?;
    let digits = lower[..k_index]
        .chars()
        .rev()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    digits.parse::<u64>().ok()?.checked_mul(1_000)
}

fn markdown_section<'a>(body: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start = body.find(start)?;
    let rest = &body[start..];
    let end = rest.find(end).unwrap_or(rest.len());
    Some(&rest[..end])
}

fn validate_remote_entries(protocol: Protocol, entries: &[StoredEntry]) -> Result<(), String> {
    let minimum = match protocol {
        Protocol::OpenAiResponses => 10,
        Protocol::AnthropicMessages => 8,
    };
    if entries.len() < minimum {
        return Err(format!(
            "官方价格表只解析到 {} 个模型，少于安全阈值 {minimum}，拒绝替换缓存",
            entries.len()
        ));
    }
    let mut ids = BTreeSet::new();
    for entry in entries {
        if entry.id.trim().is_empty() || entry.display_name.trim().is_empty() {
            return Err("官方价格表包含空模型名".to_owned());
        }
        let id = normalize_model(&entry.id);
        if !ids.insert(id) {
            return Err(format!("官方价格表包含重复模型 `{}`", entry.id));
        }
        entry.standard.validate(&entry.id)?;
        if let Some(long) = &entry.long {
            long.validate(&entry.id)?;
        }
    }
    Ok(())
}

fn read_cache_file(path: &Path) -> Result<Option<CacheFile>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata =
        fs::metadata(path).map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    if metadata.len() > MAX_PRICING_DOCUMENT_BYTES as u64 {
        return Err(format!("{} 超过缓存大小上限", path.display()));
    }
    let body = fs::read_to_string(path)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    let cache: CacheFile = serde_json::from_str(&body)
        .map_err(|error| format!("{} 不是有效缓存 JSON：{error}", path.display()))?;
    if cache.schema_version != CACHE_SCHEMA_VERSION {
        return Err(format!(
            "{} 的缓存版本为 {}，当前仅支持 {}",
            path.display(),
            cache.schema_version,
            CACHE_SCHEMA_VERSION
        ));
    }
    let protocol = protocol_from_provider(&cache.provider)
        .ok_or_else(|| format!("{} 的 provider 无效", path.display()))?;
    if cache.source != pricing_source(protocol) {
        return Err(format!("{} 的来源地址不在官方白名单中", path.display()));
    }
    validate_remote_entries(protocol, &cache.entries)?;
    Ok(Some(cache))
}

fn write_cache_file(path: &Path, cache: &CacheFile) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("缓存路径 {} 没有父目录", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("无法创建缓存目录 {}：{error}", parent.display()))?;
    let body = serde_json::to_vec_pretty(cache)
        .map_err(|error| format!("无法序列化官方价格缓存：{error}"))?;
    let temporary = parent.join(format!(
        ".rate-lens-pricing-{}-{}-{}.tmp",
        cache.provider,
        std::process::id(),
        unix_timestamp_nanos()
    ));
    let result = (|| -> Result<(), io::Error> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&body)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("无法原子写入 {}：{error}", path.display()))
}

fn catalog_cache_path(protocol: Protocol, explicit_dir: Option<&Path>) -> Option<PathBuf> {
    let directory = explicit_dir
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("RATE_LENS_CACHE_DIR").map(PathBuf::from))
        .or_else(platform_cache_dir)?;
    Some(directory.join(format!("pricing-{}.json", provider_key(protocol))))
}

fn platform_cache_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|path| path.join("Library/Caches/rate-lens"))
    }
    #[cfg(target_os = "windows")]
    {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("rate-lens"))
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .map(|path| path.join("rate-lens"))
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join(".cache/rate-lens"))
            })
    }
}

fn read_limited(reader: &mut impl io::Read, max_bytes: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取官方价格页面失败：{error}"))?;
    if bytes.len() > max_bytes {
        return Err(format!("官方价格页面超过 {} 字节上限", max_bytes));
    }
    String::from_utf8(bytes).map_err(|error| format!("官方价格页面不是 UTF-8：{error}"))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_timestamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn format_unix_timestamp(timestamp: u64) -> String {
    let days = (timestamp / 86_400) as i64;
    let seconds = timestamp % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

fn deduplicate_aliases(aliases: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    aliases.retain(|alias| seen.insert(normalize_model(alias)));
}

const fn provider_key(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiResponses => "openai",
        Protocol::AnthropicMessages => "anthropic",
    }
}

const fn provider_name(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiResponses => "OpenAI",
        Protocol::AnthropicMessages => "Anthropic",
    }
}

fn protocol_from_provider(value: &str) -> Option<Protocol> {
    match value {
        "openai" => Some(Protocol::OpenAiResponses),
        "anthropic" => Some(Protocol::AnthropicMessages),
        _ => None,
    }
}

const fn pricing_source(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiResponses => OPENAI_PRICING_SOURCE,
        Protocol::AnthropicMessages => ANTHROPIC_PRICING_SOURCE,
    }
}

const fn pricing_markdown_source(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiResponses => OPENAI_PRICING_MARKDOWN,
        Protocol::AnthropicMessages => ANTHROPIC_PRICING_MARKDOWN,
    }
}

pub fn catalog_models(protocol: Protocol) -> Vec<CatalogModel> {
    PricingCatalog::builtin(protocol).models()
}

pub fn resolve_pricing(
    protocol: Protocol,
    model: &str,
    input_tokens: u64,
    tier: PriceTier,
) -> Result<ResolvedPricing, CatalogError> {
    PricingCatalog::builtin(protocol).resolve(model, input_tokens, tier)
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

impl StoredRates {
    fn from_static(rates: Rates) -> Self {
        Self {
            input: decimal(rates.input),
            cache_read: rates.cache_read.map(decimal),
            cache_write: rates.cache_write.map(decimal),
            cache_write_5m: rates.cache_write_5m.map(decimal),
            cache_write_1h: rates.cache_write_1h.map(decimal),
            output: decimal(rates.output),
        }
    }

    fn to_pricing(&self) -> Pricing {
        Pricing {
            uncached_input_per_million: self.input,
            cache_read_per_million: self.cache_read,
            cache_write_per_million: self.cache_write,
            cache_write_5m_per_million: self.cache_write_5m,
            cache_write_1h_per_million: self.cache_write_1h,
            output_per_million: self.output,
            extra_official_cost: Decimal::ZERO,
        }
    }

    fn validate(&self, model: &str) -> Result<(), String> {
        for (label, value) in [
            ("input", Some(self.input)),
            ("cache_read", self.cache_read),
            ("cache_write", self.cache_write),
            ("cache_write_5m", self.cache_write_5m),
            ("cache_write_1h", self.cache_write_1h),
            ("output", Some(self.output)),
        ] {
            if value.is_some_and(|value| value < Decimal::ZERO) {
                return Err(format!("模型 `{model}` 的 {label} 价格为负数"));
            }
        }
        Ok(())
    }
}

fn decimal(value: &str) -> Decimal {
    Decimal::from_str(value).expect("catalog prices are valid decimals")
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_MARKDOWN: &str = r#"
### Standard pricing data

| Model | Short context input | Short context cached input | Short context cache writes | Short context output | Long context input | Long context cached input | Long context cache writes | Long context output |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| gpt-5.6-sol | $4.00 | $0.40 | $5.00 | $20.00 | $8.00 | $0.80 | $10.00 | $30.00 |
| gpt-5.5 (<272K context length) | $5.00 | $0.50 | - | $30.00 | $10.00 | $1.00 | - | $45.00 |
| gpt-5.4 | $2.50 | $0.25 | - | $15.00 | - | - | - | - |

Specialized models

### Grouped Pricing Table data

| Category | Model | Input | Cached input | Output |
| --- | --- | --- | --- | --- |
| Codex | gpt-5.3-codex | $1.75 | $0.175 | $14.00 |

Finetuning
"#;

    const ANTHROPIC_MARKDOWN: &str = r#"
## Model pricing

| Model | Base Input Tokens | 5m Cache Writes | 1h Cache Writes | Cache Hits & Refreshes | Output Tokens |
| --- | --- | --- | --- | --- | --- |
| Claude Opus 5 | $5 / MTok | $6.25 / MTok | $10 / MTok | $0.50 / MTok | $25 / MTok |
| Claude Sonnet 4.5 ([retired](https://example.invalid)) | $3 / MTok | $3.75 / MTok | $6 / MTok | $0.30 / MTok | $15 / MTok |
"#;

    #[test]
    fn parses_openai_standard_and_specialized_tables() {
        let entries = parse_openai_pricing(OPENAI_MARKDOWN).unwrap();
        let sol = entries
            .iter()
            .find(|entry| entry.id == "gpt-5.6-sol")
            .unwrap();
        assert_eq!(sol.standard.input, decimal("4"));
        assert_eq!(sol.standard.cache_write, Some(decimal("5")));
        assert_eq!(sol.long.as_ref().unwrap().output, decimal("30"));
        assert_eq!(sol.long_threshold, None);

        let gpt_55 = entries.iter().find(|entry| entry.id == "gpt-5.5").unwrap();
        assert_eq!(gpt_55.long_threshold, Some(272_000));
        assert!(entries.iter().any(|entry| entry.id == "gpt-5.3-codex"));
    }

    #[test]
    fn parses_openai_threshold_from_model_pricing_text() {
        assert_eq!(
            parse_openai_model_threshold(
                "- Prompts with >272K input tokens are priced at 2x input and 1.5x output for the full request."
            ),
            Some(272_000)
        );
        assert_eq!(
            parse_openai_model_threshold("- 1,050,000 context window"),
            None
        );
    }

    #[test]
    fn parses_anthropic_cache_columns_and_model_links() {
        let entries = parse_anthropic_pricing(ANTHROPIC_MARKDOWN).unwrap();
        let sonnet = entries
            .iter()
            .find(|entry| entry.id == "claude-sonnet-4-5")
            .unwrap();
        assert_eq!(sonnet.display_name, "Claude Sonnet 4.5");
        assert_eq!(sonnet.standard.input, decimal("3"));
        assert_eq!(sonnet.standard.cache_read, Some(decimal("0.3")));
        assert_eq!(sonnet.standard.cache_write_5m, Some(decimal("3.75")));
        assert_eq!(sonnet.standard.cache_write_1h, Some(decimal("6")));
        assert_eq!(sonnet.standard.output, decimal("15"));
    }

    #[test]
    fn rejects_partial_or_non_dollar_prices() {
        assert!(parse_price("12 credits / MTok").is_err());
        assert!(parse_price("-").unwrap().is_none());
        assert_eq!(parse_price("Free").unwrap(), Some(Decimal::ZERO));
    }

    #[test]
    fn cache_round_trip_is_validated() {
        let temporary = std::env::temp_dir().join(format!(
            "rate-lens-catalog-test-{}-{}.json",
            std::process::id(),
            unix_timestamp()
        ));
        let mut entries = parse_openai_pricing(OPENAI_MARKDOWN).unwrap();
        for index in 0..6 {
            let mut entry = entries[0].clone();
            entry.id = format!("test-model-{index}");
            entry.display_name = entry.id.clone();
            entry.aliases = vec![entry.id.clone()];
            entries.push(entry);
        }
        let cache = CacheFile {
            schema_version: CACHE_SCHEMA_VERSION,
            provider: "openai".to_owned(),
            source: OPENAI_PRICING_SOURCE.to_owned(),
            fetched_at_unix: 1_787_968_800,
            etag: Some("test-etag".to_owned()),
            last_modified: None,
            entries,
        };
        write_cache_file(&temporary, &cache).unwrap();
        let restored = read_cache_file(&temporary).unwrap().unwrap();
        assert_eq!(restored.provider, "openai");
        assert_eq!(restored.entries.len(), 10);
        fs::remove_file(temporary).unwrap();
    }

    #[test]
    fn formats_unix_timestamp_without_extra_dependency() {
        assert_eq!(format_unix_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_timestamp(1_787_968_800), "2026-08-29T02:00:00Z");
    }

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
    fn exact_dated_model_wins_over_snapshot_fallback() {
        let metadata = CatalogMetadata {
            kind: CatalogSourceKind::Live,
            source: OPENAI_PRICING_SOURCE.to_owned(),
            as_of: CATALOG_AS_OF.to_owned(),
            fetched_at: None,
            etag: None,
            last_modified: None,
        };
        let entries = merge_remote_entries(
            Protocol::OpenAiResponses,
            vec![
                StoredEntry {
                    id: "gpt-4o".to_owned(),
                    display_name: "gpt-4o".to_owned(),
                    aliases: vec!["gpt-4o".to_owned()],
                    standard: StoredRates {
                        input: decimal("2.5"),
                        cache_read: None,
                        cache_write: None,
                        cache_write_5m: None,
                        cache_write_1h: None,
                        output: decimal("10"),
                    },
                    long: None,
                    long_threshold: None,
                },
                StoredEntry {
                    id: "gpt-4o-2024-05-13".to_owned(),
                    display_name: "gpt-4o-2024-05-13".to_owned(),
                    aliases: vec!["gpt-4o-2024-05-13".to_owned()],
                    standard: StoredRates {
                        input: decimal("5"),
                        cache_read: None,
                        cache_write: None,
                        cache_write_5m: None,
                        cache_write_1h: None,
                        output: decimal("15"),
                    },
                    long: None,
                    long_threshold: None,
                },
            ],
            &metadata,
            &mut Vec::new(),
        );
        let catalog = PricingCatalog {
            entries,
            warnings: Vec::new(),
        };
        let result = catalog
            .resolve("gpt-4o-2024-05-13", 1_000, PriceTier::Auto)
            .unwrap();
        assert_eq!(result.official_model, "gpt-4o-2024-05-13");
        assert_eq!(result.pricing.uncached_input_per_million, decimal("5"));
    }

    #[test]
    fn long_context_tier_starts_above_the_threshold() {
        for (model, standard_rate, long_rate) in [
            ("gpt-5.6-sol", "4", "8"),
            ("gpt-5.6-terra", "2", "4"),
            ("gpt-5.6-luna", "0.2", "0.4"),
            ("gpt-5.5", "5", "10"),
            ("gpt-5.4", "2.5", "5"),
        ] {
            for (input_tokens, expected_tier, expected_input_rate) in [
                (271_999, "standard", standard_rate),
                (272_000, "standard", standard_rate),
                (272_001, "long", long_rate),
            ] {
                let result = resolve_pricing(
                    Protocol::OpenAiResponses,
                    model,
                    input_tokens,
                    PriceTier::Auto,
                )
                .unwrap();
                assert_eq!(result.tier, expected_tier, "model={model}");
                assert_eq!(
                    result.pricing.uncached_input_per_million,
                    decimal(expected_input_rate),
                    "model={model}"
                );
            }
        }
    }

    #[test]
    fn current_anthropic_models_do_not_expose_a_long_price_tier() {
        for model in ["claude-sonnet-4.5", "claude-sonnet-4"] {
            let auto =
                resolve_pricing(Protocol::AnthropicMessages, model, 200_001, PriceTier::Auto)
                    .unwrap();
            assert_eq!(auto.tier, "standard");
            assert_eq!(auto.pricing.uncached_input_per_million, decimal("3"));

            let error =
                resolve_pricing(Protocol::AnthropicMessages, model, 200_001, PriceTier::Long)
                    .unwrap_err();
            assert!(matches!(error, CatalogError::NoLongTier { .. }));
        }
    }
}
