use clap::{Args, Parser, Subcommand, ValueEnum};
use rate_lens::{
    Analysis, AnthropicThinkingMode, AuthStyle, CATALOG_AS_OF, ParseReport, PriceTier, Pricing,
    ProbeConfig, Protocol, ProtocolHint, ResolvedPricing, TokenCost, analyze_usage, catalog_models,
    fetch_usd_exchange_rate, list_models, normalize_currency, parse_usage, resolve_pricing,
    run_probe,
};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

#[derive(Debug, Parser)]
#[command(
    name = "rate-lens",
    version,
    about = "探测 API 中转站并按官方定价计算请求成本与倍率",
    long_about = "直接调用 OpenAI Responses 或 Anthropic Messages 兼容端点，\n按真实 usage 计算官方理论成本；提供中转扣费后再计算观测倍率。"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 发起一次真实 API 请求并计算其官方理论成本
    Probe(ProbeArgs),
    /// 读取已有 JSON/SSE 响应，离线计算成本或倍率
    Analyze(AnalyzeArgs),
    /// 从中转站的 /v1/models 获取可用模型
    Models(ModelsArgs),
    /// 列出内置的官方价格目录
    Catalog(CatalogArgs),
}

#[derive(Debug, Args)]
struct ProbeArgs {
    /// API 协议
    #[arg(long, value_enum)]
    protocol: ProbeProtocolArg,

    /// 中转站地址；可填主机、/v1 或完整推理端点
    #[arg(long)]
    base_url: String,

    /// 中转站模型 ID
    #[arg(long)]
    model: String,

    /// 官方对照模型；省略时尝试用中转模型 ID 自动匹配
    #[arg(long)]
    official_model: Option<String>,

    /// 目标输入上下文长度；会优先通过 token-count 端点校准
    #[arg(long, default_value_t = 1_000)]
    context_tokens: u64,

    /// 推理深度，原样发给服务端；OpenAI 可见 none/minimal/low/medium/high/xhigh/max
    #[arg(long)]
    reasoning: Option<String>,

    /// 最大输出 token，默认只留少量输出以减少测试成本
    #[arg(long, default_value_t = 64)]
    max_output_tokens: u64,

    /// Anthropic thinking 形式：新模型用 adaptive，旧模型可用 enabled
    #[arg(long, value_enum, default_value_t = ThinkingModeArg::Adaptive)]
    anthropic_thinking: ThinkingModeArg,

    /// Anthropic enabled thinking 的预算
    #[arg(long, default_value_t = 1_024)]
    thinking_budget_tokens: u64,

    /// 认证方式；默认按协议选择（OpenAI bearer，Anthropic x-api-key）
    #[arg(long, value_enum)]
    auth_style: Option<AuthStyleArg>,

    /// 从指定环境变量读取 API Key
    #[arg(long, default_value = "API_KEY")]
    api_key_env: String,

    /// API Key；不推荐，可能被 shell 历史和进程列表记录
    #[arg(long, hide = true)]
    api_key: Option<String>,

    /// 官方价格档；auto 会在已知阈值处自动切换
    #[arg(long, value_enum, default_value_t = PriceTierArg::Auto)]
    price_tier: PriceTierArg,

    /// 手工覆盖普通输入价格（USD/1M token）
    #[arg(long, allow_hyphen_values = true)]
    input_rate: Option<Decimal>,

    /// 手工覆盖缓存读取价格（USD/1M token）
    #[arg(long, allow_hyphen_values = true)]
    cache_read_rate: Option<Decimal>,

    /// 手工覆盖通用缓存写入价格（USD/1M token）
    #[arg(long, allow_hyphen_values = true)]
    cache_write_rate: Option<Decimal>,

    /// 手工覆盖 Anthropic 5m 缓存写入价格
    #[arg(long, allow_hyphen_values = true)]
    cache_write_5m_rate: Option<Decimal>,

    /// 手工覆盖 Anthropic 1h 缓存写入价格
    #[arg(long, allow_hyphen_values = true)]
    cache_write_1h_rate: Option<Decimal>,

    /// 手工覆盖输出价格（USD/1M token）
    #[arg(long, allow_hyphen_values = true)]
    output_rate: Option<Decimal>,

    /// 中转站对本次请求的实际扣费；有值时才计算倍率
    #[arg(long, allow_hyphen_values = true)]
    charged: Option<Decimal>,

    #[command(flatten)]
    accounting: AccountingArgs,

    /// 非交互模式下确认发起可能产生费用的请求
    #[arg(long)]
    yes: bool,

    /// HTTP 超时时间（秒）
    #[arg(long, default_value_t = 600)]
    timeout: u64,

    /// 输出机器可读 JSON
    #[arg(long)]
    json: bool,

    /// 将原始响应 JSON 保存到文件（不会保存 API Key）
    #[arg(long)]
    save_response: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AnalyzeArgs {
    /// 响应 JSON、JSON 数组、JSONL 或 SSE；使用 - 从 stdin 读取
    #[arg(value_name = "INPUT", default_value = "-")]
    input: PathBuf,

    /// 输入协议；默认自动识别
    #[arg(long, value_enum, default_value_t = ProtocolArg::Auto)]
    protocol: ProtocolArg,

    /// 官方对照模型；提供后自动使用内置官方价格
    #[arg(long)]
    official_model: Option<String>,

    /// 官方价格档
    #[arg(long, value_enum, default_value_t = PriceTierArg::Auto)]
    price_tier: PriceTierArg,

    /// 中转站实际总扣费；省略时只输出官方理论成本
    #[arg(long, allow_hyphen_values = true)]
    charged: Option<Decimal>,

    /// 普通输入价格（USD/1M token）；未使用价格目录时必填
    #[arg(long, allow_hyphen_values = true)]
    input_rate: Option<Decimal>,

    /// 输出价格（USD/1M token）；未使用价格目录时必填
    #[arg(long, allow_hyphen_values = true)]
    output_rate: Option<Decimal>,

    #[arg(long, allow_hyphen_values = true)]
    cache_read_rate: Option<Decimal>,

    #[arg(long, allow_hyphen_values = true)]
    cache_write_rate: Option<Decimal>,

    #[arg(long, allow_hyphen_values = true)]
    cache_write_5m_rate: Option<Decimal>,

    #[arg(long, allow_hyphen_values = true)]
    cache_write_1h_rate: Option<Decimal>,

    #[command(flatten)]
    accounting: AccountingArgs,

    /// 输出机器可读 JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ModelsArgs {
    #[arg(long, value_enum)]
    protocol: ProbeProtocolArg,

    #[arg(long)]
    base_url: String,

    #[arg(long, value_enum)]
    auth_style: Option<AuthStyleArg>,

    #[arg(long, default_value = "API_KEY")]
    api_key_env: String,

    #[arg(long, hide = true)]
    api_key: Option<String>,

    #[arg(long, default_value_t = 60)]
    timeout: u64,

    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CatalogArgs {
    #[arg(long, value_enum)]
    protocol: Option<ProbeProtocolArg>,

    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AccountingArgs {
    /// 非 token 项目的官方总费用
    #[arg(long, default_value = "0", allow_hyphen_values = true)]
    extra_official_cost: Decimal,

    /// 1 USD 对应多少单位中转扣费币种
    #[arg(long, default_value = "1", allow_hyphen_values = true)]
    exchange_rate: Decimal,

    #[arg(long, default_value = "USD")]
    reference_currency: String,

    #[arg(long, default_value = "USD")]
    actual_currency: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProbeProtocolArg {
    Openai,
    Anthropic,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum ProtocolArg {
    #[default]
    Auto,
    Openai,
    Anthropic,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AuthStyleArg {
    Bearer,
    XApiKey,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum PriceTierArg {
    #[default]
    Auto,
    Standard,
    Long,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum ThinkingModeArg {
    #[default]
    Adaptive,
    Enabled,
}

#[derive(Debug, Clone, Serialize)]
struct PricingMetadata {
    official_model: String,
    display_name: String,
    tier: String,
    source: String,
    as_of: String,
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    pricing: Option<&'a PricingMetadata>,
    reference_currency: &'a str,
    actual_currency: &'a str,
    warnings: &'a [String],
    analysis: &'a Analysis,
}

#[derive(Serialize)]
struct ProbeJsonOutput<'a> {
    target_context_tokens: u64,
    counted_input_tokens: Option<u64>,
    pricing: Option<&'a PricingMetadata>,
    reference_currency: &'a str,
    actual_currency: &'a str,
    warnings: &'a [String],
    analysis: &'a Analysis,
    response: &'a Value,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Probe(args)) => run_probe_command(args),
        Some(Command::Analyze(args)) => run_analyze(args),
        Some(Command::Models(args)) => run_models(args),
        Some(Command::Catalog(args)) => run_catalog(args),
        None => run_wizard(),
    }
}

fn run_probe_command(args: ProbeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let protocol = Protocol::from(args.protocol);
    let api_key = resolve_api_key(args.api_key.clone(), &args.api_key_env)?;
    let auth_style = args
        .auth_style
        .map(AuthStyle::from)
        .unwrap_or_else(|| default_auth_style(protocol));
    let pricing_model = args.official_model.as_deref().unwrap_or(&args.model);
    let manual_rates = ManualRates {
        input: args.input_rate,
        output: args.output_rate,
        cache_read: args.cache_read_rate,
        cache_write: args.cache_write_rate,
        cache_write_5m: args.cache_write_5m_rate,
        cache_write_1h: args.cache_write_1h_rate,
        extra: args.accounting.extra_official_cost,
    };
    let (estimated_pricing, _, _) = resolve_requested_pricing(
        protocol,
        pricing_model,
        args.context_tokens,
        args.price_tier.into(),
        manual_rates,
    )?;

    confirm_probe(&args, estimated_pricing.as_ref())?;
    let result = run_probe(&ProbeConfig {
        protocol,
        base_url: args.base_url.clone(),
        api_key,
        auth_style,
        model: args.model.clone(),
        context_tokens: args.context_tokens,
        reasoning_effort: normalize_reasoning(protocol, args.reasoning.clone()),
        max_output_tokens: args.max_output_tokens,
        anthropic_thinking_mode: args.anthropic_thinking.into(),
        thinking_budget_tokens: args.thinking_budget_tokens,
        timeout_seconds: args.timeout,
    })?;

    if let Some(path) = args.save_response.as_ref() {
        let body = serde_json::to_string_pretty(&result.response)?;
        fs::write(path, body)?;
    }

    let (pricing, pricing_metadata, mut warnings) = resolve_requested_pricing(
        protocol,
        pricing_model,
        result.report.usage.total_input_tokens(),
        args.price_tier.into(),
        manual_rates,
    )?;
    warnings.extend(result.warnings.clone());
    append_usage_warnings(&result.report.usage, &mut warnings);
    let pricing = pricing.ok_or(
        "无法确定官方价格：请使用 --official-model 指定目录模型，或同时提供 --input-rate 与 --output-rate",
    )?;
    let analysis = analyze_usage(
        result.report.usage.clone(),
        pricing,
        args.charged,
        args.accounting.exchange_rate,
    )?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&ProbeJsonOutput {
                target_context_tokens: result.target_context_tokens,
                counted_input_tokens: result.counted_input_tokens,
                pricing: pricing_metadata.as_ref(),
                reference_currency: &args.accounting.reference_currency,
                actual_currency: &args.accounting.actual_currency,
                warnings: &warnings,
                analysis: &analysis,
                response: &result.response,
            })?
        );
    } else {
        println!("探测目标      {}", args.base_url);
        println!(
            "目标上下文    {} token",
            format_integer(args.context_tokens)
        );
        if let Some(counted) = result.counted_input_tokens {
            println!("计数端点结果  {} token", format_integer(counted));
        }
        print_human(
            &analysis,
            &warnings,
            pricing_metadata.as_ref(),
            &args.accounting.reference_currency,
            &args.accounting.actual_currency,
        );
    }
    Ok(())
}

fn run_analyze(args: AnalyzeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let input = read_input(&args.input)?;
    let ParseReport {
        usage,
        mut warnings,
    } = parse_usage(&input, args.protocol.into())?;
    append_usage_warnings(&usage, &mut warnings);

    let protocol = usage.protocol;
    let inferred_model = (usage.models.len() == 1)
        .then(|| usage.models.iter().next().cloned())
        .flatten();
    let catalog_model = args
        .official_model
        .as_deref()
        .or(inferred_model.as_deref())
        .unwrap_or("");
    let (pricing, pricing_metadata, catalog_warnings) = resolve_requested_pricing(
        protocol,
        catalog_model,
        usage.total_input_tokens(),
        args.price_tier.into(),
        ManualRates {
            input: args.input_rate,
            output: args.output_rate,
            cache_read: args.cache_read_rate,
            cache_write: args.cache_write_rate,
            cache_write_5m: args.cache_write_5m_rate,
            cache_write_1h: args.cache_write_1h_rate,
            extra: args.accounting.extra_official_cost,
        },
    )?;
    warnings.extend(catalog_warnings);
    let pricing = pricing.ok_or(
        "无法确定官方价格：请使用 --official-model，或同时提供 --input-rate 与 --output-rate",
    )?;
    let analysis = analyze_usage(usage, pricing, args.charged, args.accounting.exchange_rate)?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&JsonOutput {
                pricing: pricing_metadata.as_ref(),
                reference_currency: &args.accounting.reference_currency,
                actual_currency: &args.accounting.actual_currency,
                warnings: &warnings,
                analysis: &analysis,
            })?
        );
    } else {
        print_human(
            &analysis,
            &warnings,
            pricing_metadata.as_ref(),
            &args.accounting.reference_currency,
            &args.accounting.actual_currency,
        );
    }
    Ok(())
}

fn run_models(args: ModelsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let protocol = Protocol::from(args.protocol);
    let api_key = resolve_api_key(args.api_key, &args.api_key_env)?;
    let models = list_models(
        protocol,
        &args.base_url,
        &api_key,
        args.auth_style
            .map(AuthStyle::from)
            .unwrap_or_else(|| default_auth_style(protocol)),
        args.timeout,
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&models)?);
    } else if models.is_empty() {
        println!("中转站返回了空模型列表。");
    } else {
        for model in models {
            let limits = match (model.max_input_tokens, model.max_output_tokens) {
                (Some(input), Some(output)) => format!("（输入≤{input}，输出≤{output}）"),
                (Some(input), None) => format!("（输入≤{input}）"),
                (None, Some(output)) => format!("（输出≤{output}）"),
                (None, None) => String::new(),
            };
            println!("{} {limits}", model.id);
        }
    }
    Ok(())
}

fn run_catalog(args: CatalogArgs) -> Result<(), Box<dyn std::error::Error>> {
    let protocols = match args.protocol {
        Some(protocol) => vec![Protocol::from(protocol)],
        None => vec![Protocol::OpenAiResponses, Protocol::AnthropicMessages],
    };
    let entries = protocols
        .into_iter()
        .flat_map(catalog_models)
        .collect::<Vec<_>>();
    if args.json {
        let values = entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "protocol": entry.protocol,
                    "id": entry.id,
                    "display_name": entry.display_name,
                    "as_of": CATALOG_AS_OF,
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        println!("内置官方价格目录（截至 {CATALOG_AS_OF}）");
        let mut last_protocol = None;
        for entry in entries {
            if last_protocol != Some(entry.protocol) {
                println!();
                println!("{}", entry.protocol);
                last_protocol = Some(entry.protocol);
            }
            println!("  {:<24} {}", entry.id, entry.display_name);
        }
    }
    Ok(())
}

fn run_wizard() -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("无参数交互向导需要终端；脚本中请使用 `rate-lens probe --help`".into());
    }
    eprintln!("rate-lens API 中转倍率探测向导");
    eprintln!("本向导将发起一次真实、可能产生费用的请求。\n");
    let protocol = match prompt("协议 [1=OpenAI Responses, 2=Anthropic Messages]", "1")?.as_str()
    {
        "2" => Protocol::AnthropicMessages,
        _ => Protocol::OpenAiResponses,
    };
    let default_url = match protocol {
        Protocol::OpenAiResponses => "https://api.openai.com/v1",
        Protocol::AnthropicMessages => "https://api.anthropic.com/v1",
    };
    let base_url = prompt("Base URL", default_url)?;
    let api_key = rpassword::prompt_password("API Key（输入隐藏）: ")?;
    let auth_style = default_auth_style(protocol);
    let models = match list_models(protocol, &base_url, &api_key, auth_style, 60) {
        Ok(models) => models,
        Err(error) => {
            eprintln!("模型列表获取失败：{error}");
            Vec::new()
        }
    };
    let model = if models.is_empty() {
        prompt("模型 ID", "")?
    } else {
        eprintln!("\n可用模型：");
        for (index, model) in models.iter().take(50).enumerate() {
            eprintln!("  {:>2}. {}", index + 1, model.id);
        }
        let answer = prompt("选择编号，或直接输入模型 ID", "1")?;
        answer
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| models.get(index))
            .map(|model| model.id.clone())
            .unwrap_or(answer)
    };
    let official_model = prompt("官方对照模型（回车沿用中转模型 ID）", &model)?;
    let context_tokens = parse_prompt::<u64>("目标输入 token", "1000")?;
    let reasoning = prompt_reasoning_effort(protocol)?;
    let max_output_tokens = parse_prompt::<u64>("最大输出 token", "64")?;
    let actual_currency = prompt_actual_currency()?;
    let exchange_rate = prompt_exchange_rate(&actual_currency)?;

    let (estimated_pricing, _, _) = resolve_requested_pricing(
        protocol,
        &official_model,
        context_tokens,
        PriceTier::Auto,
        ManualRates::empty(),
    )?;
    let estimated_pricing =
        estimated_pricing.ok_or("内置目录无法匹配官方模型；请改用 probe 命令并手工传入价格")?;
    eprintln!(
        "\n预计输入成本约 {} USD（最终按响应 usage 计算）。",
        money(rate_lens::approximate_official_input_cost(
            context_tokens,
            estimated_pricing.uncached_input_per_million
        ))
    );
    let confirmed = prompt("确认发起请求？[y/N]", "n")?;
    if !matches!(confirmed.to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err("已取消，未发起推理请求".into());
    }

    let result = run_probe(&ProbeConfig {
        protocol,
        base_url,
        api_key,
        auth_style,
        model,
        context_tokens,
        reasoning_effort: reasoning,
        max_output_tokens,
        anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
        thinking_budget_tokens: 1_024,
        timeout_seconds: 600,
    })?;
    let charged_text = prompt("本次中转实际扣费（可留空，只查看官方价）", "")?;
    let charged = parse_optional_decimal(&charged_text)?;
    let (pricing, metadata, mut all_warnings) = resolve_requested_pricing(
        protocol,
        &official_model,
        result.report.usage.total_input_tokens(),
        PriceTier::Auto,
        ManualRates::empty(),
    )?;
    let pricing = pricing.ok_or("无法按实际 usage 确定官方价格")?;
    all_warnings.extend(result.warnings);
    append_usage_warnings(&result.report.usage, &mut all_warnings);
    let analysis = analyze_usage(result.report.usage, pricing, charged, exchange_rate)?;
    print_human(
        &analysis,
        &all_warnings,
        metadata.as_ref(),
        "USD",
        &actual_currency,
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ManualRates {
    input: Option<Decimal>,
    output: Option<Decimal>,
    cache_read: Option<Decimal>,
    cache_write: Option<Decimal>,
    cache_write_5m: Option<Decimal>,
    cache_write_1h: Option<Decimal>,
    extra: Decimal,
}

impl ManualRates {
    fn empty() -> Self {
        Self {
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            cache_write_5m: None,
            cache_write_1h: None,
            extra: Decimal::ZERO,
        }
    }

    fn is_partial(self) -> bool {
        self.input.is_some() ^ self.output.is_some()
    }

    fn build(self) -> Option<Pricing> {
        Some(Pricing {
            uncached_input_per_million: self.input?,
            cache_read_per_million: self.cache_read,
            cache_write_per_million: self.cache_write,
            cache_write_5m_per_million: self.cache_write_5m,
            cache_write_1h_per_million: self.cache_write_1h,
            output_per_million: self.output?,
            extra_official_cost: self.extra,
        })
    }

    fn overlay(self, mut pricing: Pricing) -> Pricing {
        if let Some(value) = self.input {
            pricing.uncached_input_per_million = value;
        }
        if let Some(value) = self.output {
            pricing.output_per_million = value;
        }
        if self.cache_read.is_some() {
            pricing.cache_read_per_million = self.cache_read;
        }
        if self.cache_write.is_some() {
            pricing.cache_write_per_million = self.cache_write;
        }
        if self.cache_write_5m.is_some() {
            pricing.cache_write_5m_per_million = self.cache_write_5m;
        }
        if self.cache_write_1h.is_some() {
            pricing.cache_write_1h_per_million = self.cache_write_1h;
        }
        pricing.extra_official_cost = self.extra;
        pricing
    }
}

type PricingResolution = (Option<Pricing>, Option<PricingMetadata>, Vec<String>);

fn resolve_requested_pricing(
    protocol: Protocol,
    model: &str,
    input_tokens: u64,
    tier: PriceTier,
    manual: ManualRates,
) -> Result<PricingResolution, Box<dyn std::error::Error>> {
    let catalog = resolve_pricing(protocol, model, input_tokens, tier);
    match catalog {
        Ok(resolved) if manual.input.is_some() == manual.output.is_some() => {
            let metadata = pricing_metadata(&resolved);
            Ok((
                Some(manual.overlay(resolved.pricing)),
                Some(metadata),
                resolved.warnings,
            ))
        }
        Ok(_) => Err("手工价格覆盖必须同时提供 --input-rate 和 --output-rate".into()),
        Err(error) if manual.is_partial() => {
            Err(format!("{error}；手工价格必须同时提供 --input-rate 和 --output-rate").into())
        }
        Err(error) if manual.build().is_some() => {
            let mut warnings = vec![format!(
                "未使用内置价格目录（{error}）；本次按手工价格计算。"
            )];
            if !model.trim().is_empty() {
                warnings.push("请自行确认手工价格与官方对照模型、价格档一致。".to_owned());
            }
            Ok((manual.build(), None, warnings))
        }
        Err(error) => Err(error.into()),
    }
}

fn confirm_probe(
    args: &ProbeArgs,
    pricing: Option<&Pricing>,
) -> Result<(), Box<dyn std::error::Error>> {
    if args.yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("probe 会产生真实 API 费用；非交互运行必须显式传入 --yes".into());
    }
    eprintln!("即将向 {} 发起一次真实请求：", args.base_url);
    eprintln!("  模型：{}", args.model);
    eprintln!("  目标输入：{} token", format_integer(args.context_tokens));
    eprintln!(
        "  最大输出：{} token",
        format_integer(args.max_output_tokens)
    );
    if let Some(pricing) = pricing {
        let estimate = rate_lens::approximate_official_input_cost(
            args.context_tokens,
            pricing.uncached_input_per_million,
        );
        eprintln!("  仅输入的官方价预估：约 {} USD", money(estimate));
    }
    let answer = prompt("确认继续？[y/N]", "n")?;
    if matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err("已取消，未发起推理请求".into())
    }
}

fn resolve_api_key(
    explicit: Option<String>,
    environment_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some(value) = explicit.filter(|value| !value.is_empty()) {
        return Ok(value);
    }
    if let Ok(value) = env::var(environment_name)
        && !value.is_empty()
    {
        return Ok(value);
    }
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        let value = rpassword::prompt_password(format!(
            "API Key（环境变量 {environment_name} 未设置，输入隐藏）: "
        ))?;
        if !value.is_empty() {
            return Ok(value);
        }
    }
    Err(format!("未找到 API Key；请设置环境变量 {environment_name}").into())
}

fn read_input(path: &PathBuf) -> Result<String, io::Error> {
    if path.as_os_str() != "-" {
        fs::read_to_string(path)
    } else {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        Ok(input)
    }
}

fn append_usage_warnings(usage: &rate_lens::NormalizedUsage, warnings: &mut Vec<String>) {
    if usage.models.len() > 1 {
        warnings.push(format!(
            "输入包含多个模型（{}）；只有它们适用相同价格时，合并结果才准确。",
            join_values(&usage.models)
        ));
    }
    if usage.service_tiers.len() > 1 {
        warnings.push(format!(
            "输入包含多个服务层级（{}）；请确认价格与这些层级一致。",
            join_values(&usage.service_tiers)
        ));
    }
    for tier in &usage.service_tiers {
        let is_standard = match usage.protocol {
            Protocol::OpenAiResponses => matches!(tier.as_str(), "default" | "auto"),
            Protocol::AnthropicMessages => matches!(tier.as_str(), "standard" | "auto"),
        };
        if !is_standard {
            warnings.push(format!(
                "响应使用服务层级 `{tier}`；内置目录是标准同步价格，请确认该层级没有不同定价。"
            ));
        }
    }
    if usage.reasoning_output_tokens > 0 {
        warnings.push("推理/思考 token 已包含在 output_tokens 中，没有重复计费。".to_owned());
    }
}

fn print_human(
    analysis: &Analysis,
    warnings: &[String],
    pricing_metadata: Option<&PricingMetadata>,
    reference_currency: &str,
    actual_currency: &str,
) {
    let usage = &analysis.usage;
    println!("协议          {}", usage.protocol);
    println!(
        "实际模型      {}",
        if usage.models.is_empty() {
            "响应未提供".to_owned()
        } else {
            join_values(&usage.models)
        }
    );
    if let Some(metadata) = pricing_metadata {
        println!(
            "官方对照模型  {}（{} 档；目录截至 {}）",
            metadata.display_name, metadata.tier, metadata.as_of
        );
    } else {
        println!("官方对照模型  手工价格");
    }
    if !usage.service_tiers.is_empty() {
        println!("服务层级      {}", join_values(&usage.service_tiers));
    }
    println!("请求数        {}", usage.requests);
    println!();
    println!("Token 用量与理论成本（{reference_currency}/1M token）");
    print_cost(
        "普通输入",
        &analysis.costs.uncached_input,
        reference_currency,
    );
    print_optional_cost(
        "缓存读取",
        &analysis.costs.cache_read_input,
        reference_currency,
    );
    print_optional_cost(
        "缓存写入",
        &analysis.costs.cache_write_input,
        reference_currency,
    );
    print_optional_cost(
        "缓存写入 5m",
        &analysis.costs.cache_write_5m_input,
        reference_currency,
    );
    print_optional_cost(
        "缓存写入 1h",
        &analysis.costs.cache_write_1h_input,
        reference_currency,
    );
    print_cost("输出", &analysis.costs.output, reference_currency);
    if usage.reasoning_output_tokens > 0 {
        println!(
            "  └ 推理/思考 {:>12} token（已包含在输出中）",
            format_integer(usage.reasoning_output_tokens)
        );
    }
    if analysis.costs.extra_official_cost != Decimal::ZERO {
        println!(
            "额外官方费用  {:>12} {reference_currency}",
            money(analysis.costs.extra_official_cost)
        );
    }
    if !usage.metered_extras.is_empty() {
        println!("额外计量      {}", format_extras(&usage.metered_extras));
    }

    println!();
    println!(
        "官方理论成本  {} {reference_currency}",
        money(analysis.official_cost_reference)
    );
    if reference_currency != actual_currency || analysis.exchange_rate != Decimal::ONE {
        println!(
            "折算理论成本  {} {}  （1 {} = {} {}）",
            money(analysis.official_cost_actual_currency),
            actual_currency,
            reference_currency,
            compact(analysis.exchange_rate, 8),
            actual_currency
        );
    }
    if let (Some(charged), Some(multiplier), Some(markup_percent), Some(difference)) = (
        analysis.charged_actual_currency,
        analysis.observed_multiplier,
        analysis.markup_percent,
        analysis.difference_actual_currency,
    ) {
        println!("中转实际扣费  {} {}", money(charged), actual_currency);
        println!("观测倍率      {}×", fixed(multiplier, 4));
        println!(
            "相对官方价    {:+}%  （差额 {:+} {}）",
            markup_percent.round_dp(2),
            difference.round_dp(8),
            actual_currency
        );
    } else {
        println!("观测倍率      未计算（中转实际扣费未提供）");
        println!("              可将上面的官方理论成本与中转账单自行比较");
    }

    if let Some(metadata) = pricing_metadata {
        println!();
        println!("价格来源      {}", metadata.source);
    }
    if !warnings.is_empty() {
        println!();
        println!("注意");
        for warning in warnings {
            println!("- {warning}");
        }
    }
}

fn pricing_metadata(resolved: &ResolvedPricing) -> PricingMetadata {
    PricingMetadata {
        official_model: resolved.official_model.to_owned(),
        display_name: resolved.display_name.to_owned(),
        tier: resolved.tier.to_owned(),
        source: resolved.source.to_owned(),
        as_of: resolved.as_of.to_owned(),
    }
}

fn print_cost(label: &str, cost: &TokenCost, currency: &str) {
    println!(
        "{label:<14}{:>12} token × {:>10} = {:>12} {currency}",
        format_integer(cost.tokens),
        compact(cost.rate_per_million, 8),
        money(cost.reference_cost),
    );
}

fn print_optional_cost(label: &str, cost: &Option<TokenCost>, currency: &str) {
    if let Some(cost) = cost {
        print_cost(label, cost, currency);
    }
}

fn prompt(label: &str, default: &str) -> Result<String, io::Error> {
    if default.is_empty() {
        eprint!("{label}: ");
    } else {
        eprint!("{label} [{default}]: ");
    }
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

fn parse_prompt<T>(label: &str, default: &str) -> Result<T, Box<dyn std::error::Error>>
where
    T: FromStr,
    T::Err: std::error::Error + 'static,
{
    Ok(prompt(label, default)?.parse()?)
}

fn parse_optional_decimal(value: &str) -> Result<Option<Decimal>, rust_decimal::Error> {
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Decimal::from_str(value).map(Some)
    }
}

fn prompt_reasoning_effort(
    protocol: Protocol,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let (title, options, default) = match protocol {
        Protocol::OpenAiResponses => (
            "OpenAI 推理深度",
            vec![
                ("不发送 reasoning 参数", None),
                ("none", Some("none")),
                ("minimal", Some("minimal")),
                ("low", Some("low")),
                ("medium", Some("medium")),
                ("high", Some("high")),
                ("xhigh", Some("xhigh")),
                ("max", Some("max")),
            ],
            "1",
        ),
        Protocol::AnthropicMessages => (
            "Anthropic adaptive thinking effort",
            vec![
                ("关闭 thinking", None),
                ("low", Some("low")),
                ("medium", Some("medium")),
                ("high", Some("high")),
                ("max", Some("max")),
            ],
            "1",
        ),
    };
    eprintln!("\n{title}（具体支持范围由模型决定）：");
    for (index, (label, _)) in options.iter().enumerate() {
        eprintln!("  {}. {label}", index + 1);
    }
    eprintln!("  {}. 自定义值", options.len() + 1);
    loop {
        let answer = prompt("选择编号", default)?;
        if let Ok(index) = answer.parse::<usize>() {
            if let Some((_, value)) = index.checked_sub(1).and_then(|index| options.get(index)) {
                return Ok(value.map(str::to_owned));
            }
            if index == options.len() + 1 {
                let custom = prompt("自定义 effort", "")?;
                if !custom.trim().is_empty() {
                    return Ok(Some(custom.trim().to_owned()));
                }
            }
        }
        eprintln!("请输入列表中的编号。");
    }
}

fn prompt_actual_currency() -> Result<String, Box<dyn std::error::Error>> {
    const CURRENCIES: &[(&str, &str)] = &[
        ("USD", "美元"),
        ("CNY", "人民币"),
        ("HKD", "港币"),
        ("TWD", "新台币"),
        ("EUR", "欧元"),
        ("JPY", "日元"),
        ("GBP", "英镑"),
        ("SGD", "新加坡元"),
    ];
    eprintln!("\n中转站扣费币种：");
    for (index, (code, name)) in CURRENCIES.iter().enumerate() {
        eprintln!("  {}. {code}（{name}）", index + 1);
    }
    eprintln!("  {}. 自定义 ISO 4217 币种", CURRENCIES.len() + 1);
    eprintln!("  {}. 站内额度/非货币单位", CURRENCIES.len() + 2);
    loop {
        let answer = prompt("选择编号", "1")?;
        if let Ok(index) = answer.parse::<usize>() {
            if let Some((code, _)) = index.checked_sub(1).and_then(|index| CURRENCIES.get(index)) {
                return Ok((*code).to_owned());
            }
            if index == CURRENCIES.len() + 1 {
                let currency = prompt("三位币种代码", "")?;
                match normalize_currency(&currency) {
                    Ok(currency) => return Ok(currency),
                    Err(error) => eprintln!("{error}"),
                }
                continue;
            }
            if index == CURRENCIES.len() + 2 {
                let unit = prompt("额度单位名称", "QUOTA")?;
                if !unit.trim().is_empty() {
                    return Ok(unit.trim().to_ascii_uppercase());
                }
            }
        }
        eprintln!("请输入列表中的编号。");
    }
}

fn prompt_exchange_rate(actual_currency: &str) -> Result<Decimal, Box<dyn std::error::Error>> {
    if actual_currency == "USD" {
        eprintln!("1 USD = 1 USD。");
        return Ok(Decimal::ONE);
    }

    let reference = if normalize_currency(actual_currency).is_ok() {
        eprintln!("正在查询 USD/{actual_currency} 市场参考汇率……");
        match fetch_usd_exchange_rate(actual_currency, 10) {
            Ok(quote) => {
                let date = quote
                    .date
                    .as_deref()
                    .map(|date| format!("，日期 {date}"))
                    .unwrap_or_default();
                eprintln!(
                    "参考：1 USD ≈ {} {}（{}{}）",
                    compact(quote.rate, 8),
                    quote.quote,
                    quote.source,
                    date
                );
                Some(quote.rate)
            }
            Err(error) => {
                eprintln!("参考汇率获取失败：{error}");
                None
            }
        }
    } else {
        eprintln!("站内额度没有公开市场汇率，请按中转站规则填写。");
        None
    };
    eprintln!("市场参考值不一定等于中转站结算汇率，可直接覆盖。");
    let default = reference
        .map(|rate| compact(rate, 8))
        .unwrap_or_else(|| "1".to_owned());
    parse_prompt::<Decimal>(&format!("1 USD 对应多少 {actual_currency}"), &default)
}

fn normalize_reasoning(protocol: Protocol, value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        if value.is_empty() || (protocol == Protocol::AnthropicMessages && value == "none") {
            None
        } else {
            Some(value)
        }
    })
}

fn default_auth_style(protocol: Protocol) -> AuthStyle {
    match protocol {
        Protocol::OpenAiResponses => AuthStyle::Bearer,
        Protocol::AnthropicMessages => AuthStyle::XApiKey,
    }
}

fn join_values(values: &std::collections::BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(", ")
}

fn format_extras(values: &std::collections::BTreeMap<String, u64>) -> String {
    values
        .iter()
        .filter(|(_, value)| **value > 0)
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn money(value: Decimal) -> String {
    fixed(value, 8)
}

fn fixed(value: Decimal, places: u32) -> String {
    format!("{value:.places$}", places = places as usize)
}

fn compact(value: Decimal, places: u32) -> String {
    value.round_dp(places).normalize().to_string()
}

impl From<ProbeProtocolArg> for Protocol {
    fn from(value: ProbeProtocolArg) -> Self {
        match value {
            ProbeProtocolArg::Openai => Self::OpenAiResponses,
            ProbeProtocolArg::Anthropic => Self::AnthropicMessages,
        }
    }
}

impl From<ProtocolArg> for ProtocolHint {
    fn from(value: ProtocolArg) -> Self {
        match value {
            ProtocolArg::Auto => Self::Auto,
            ProtocolArg::Openai => Self::OpenAiResponses,
            ProtocolArg::Anthropic => Self::AnthropicMessages,
        }
    }
}

impl From<AuthStyleArg> for AuthStyle {
    fn from(value: AuthStyleArg) -> Self {
        match value {
            AuthStyleArg::Bearer => Self::Bearer,
            AuthStyleArg::XApiKey => Self::XApiKey,
        }
    }
}

impl From<PriceTierArg> for PriceTier {
    fn from(value: PriceTierArg) -> Self {
        match value {
            PriceTierArg::Auto => Self::Auto,
            PriceTierArg::Standard => Self::Standard,
            PriceTierArg::Long => Self::Long,
        }
    }
}

impl From<ThinkingModeArg> for AnthropicThinkingMode {
    fn from(value: ThinkingModeArg) -> Self {
        match value {
            ThinkingModeArg::Adaptive => Self::Adaptive,
            ThinkingModeArg::Enabled => Self::Enabled,
        }
    }
}
