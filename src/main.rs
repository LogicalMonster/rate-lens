use clap::{Args, Parser, Subcommand, ValueEnum};
use rate_lens::{
    Analysis, AnthropicThinkingMode, AuthStyle, CatalogLoadOptions, CatalogPricingProfile,
    CatalogSourceKind, DiscoveredModel, ParseReport, PriceTier, Pricing, PricingCatalog,
    PricingSourceMode, ProbeConfig, ProbeResult, Protocol, ProtocolHint, ResolvedPricing,
    TokenCost, analyze_usage, fetch_usd_exchange_rate, list_models, load_pricing_catalog,
    normalize_currency, parse_usage, run_probe,
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
use std::time::{SystemTime, UNIX_EPOCH};

const WIZARD_CONNECTIVITY_TOKENS: u64 = 1_000;
const WIZARD_STANDARD_TOKENS: u64 = 8_000;
const WIZARD_CACHE_TOKENS: u64 = 8_000;
const WIZARD_PRECHECK_OUTPUT_TOKENS: u64 = 16;
const WIZARD_DEFAULT_OUTPUT_TOKENS: u64 = 64;
const WIZARD_LONG_MARGIN_MIN: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardTestKind {
    Connectivity,
    Standard,
    Long,
    TierBoundary,
    Cache,
    Custom,
}

impl WizardTestKind {
    fn label(self) -> &'static str {
        match self {
            Self::Connectivity => "连通性预检",
            Self::Standard => "常规档倍率",
            Self::Long => "长上下文档倍率",
            Self::TierBoundary => "分档边界对照",
            Self::Cache => "缓存倍率",
            Self::Custom => "自定义",
        }
    }
}

#[derive(Debug, Clone)]
struct WizardProbeSpec {
    label: String,
    context_tokens: u64,
    max_output_tokens: u64,
    reasoning_effort: Option<String>,
    enable_prompt_cache: bool,
    prompt_cache_key: Option<String>,
    prompt_marker: Option<String>,
    disable_implicit_prompt_cache: bool,
}

#[derive(Debug, Clone)]
struct WizardTestPlan {
    kind: WizardTestKind,
    specs: Vec<WizardProbeSpec>,
    long_threshold: Option<u64>,
    includes_precheck: bool,
    relay_cache_supported: bool,
}

#[derive(Debug, Clone)]
struct WizardProbeRecord {
    spec: WizardProbeSpec,
    counted_input_tokens: Option<u64>,
    analysis: Analysis,
    pricing: Pricing,
    pricing_metadata: PricingMetadata,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
enum WizardChargeInput {
    None,
    Total(Decimal),
    PerRequest(Vec<Option<Decimal>>),
}

#[derive(Debug, Clone)]
enum WizardChargeCapture {
    None,
    Total,
    Balance { before: Decimal },
    PerRequest,
}

#[derive(Debug, Parser)]
#[command(
    name = "rate-lens",
    version,
    about = "探测 API 中转站并按官方定价计算请求成本与倍率",
    long_about = "直接调用 OpenAI Responses 或 Anthropic Messages 兼容端点，\n按真实 usage 计算官方理论成本；提供中转扣费后再计算观测倍率。"
)]
struct Cli {
    /// 官方价格来源：auto 实时刷新并回退缓存/内置快照，live 禁止回退，builtin 完全离线
    #[arg(long, value_enum, global = true, default_value_t = PricingSourceArg::Auto)]
    pricing_source: PricingSourceArg,

    /// 获取官方价格页面的 HTTP 超时（秒）
    #[arg(long, global = true, default_value_t = 20)]
    pricing_timeout: u64,

    /// 官方价格缓存目录；也可设置 RATE_LENS_CACHE_DIR
    #[arg(long, global = true)]
    pricing_cache_dir: Option<PathBuf>,

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
    /// 列出当前官方价格目录
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
enum PricingSourceArg {
    #[default]
    Auto,
    Live,
    Builtin,
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
    source_kind: CatalogSourceKind,
    as_of: String,
    fetched_at: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
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
    let catalog_options = CatalogLoadOptions {
        source: cli.pricing_source.into(),
        timeout_seconds: cli.pricing_timeout,
        cache_dir: cli.pricing_cache_dir,
    };
    match cli.command {
        Some(Command::Probe(args)) => run_probe_command(args, &catalog_options),
        Some(Command::Analyze(args)) => run_analyze(args, &catalog_options),
        Some(Command::Models(args)) => run_models(args),
        Some(Command::Catalog(args)) => run_catalog(args, &catalog_options),
        None => run_wizard(&catalog_options),
    }
}

fn run_probe_command(
    args: ProbeArgs,
    catalog_options: &CatalogLoadOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let protocol = Protocol::from(args.protocol);
    let catalog = load_pricing_catalog(protocol, catalog_options)?;
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
        &catalog,
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
        enable_prompt_cache: false,
        prompt_cache_key: None,
        prompt_marker: None,
        disable_implicit_prompt_cache: false,
        openai_prompt_cache_options: false,
        timeout_seconds: args.timeout,
    })?;

    if let Some(path) = args.save_response.as_ref() {
        let body = serde_json::to_string_pretty(&result.response)?;
        fs::write(path, body)?;
    }

    let (pricing, pricing_metadata, mut warnings) = resolve_requested_pricing(
        &catalog,
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

fn run_analyze(
    args: AnalyzeArgs,
    catalog_options: &CatalogLoadOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let input = read_input(&args.input)?;
    let ParseReport {
        usage,
        mut warnings,
    } = parse_usage(&input, args.protocol.into())?;
    append_usage_warnings(&usage, &mut warnings);

    let protocol = usage.protocol;
    let catalog = load_pricing_catalog(protocol, catalog_options)?;
    let inferred_model = (usage.models.len() == 1)
        .then(|| usage.models.iter().next().cloned())
        .flatten();
    let catalog_model = args
        .official_model
        .as_deref()
        .or(inferred_model.as_deref())
        .unwrap_or("");
    let (pricing, pricing_metadata, catalog_warnings) = resolve_requested_pricing(
        &catalog,
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

fn run_catalog(
    args: CatalogArgs,
    catalog_options: &CatalogLoadOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let protocols = match args.protocol {
        Some(protocol) => vec![Protocol::from(protocol)],
        None => vec![Protocol::OpenAiResponses, Protocol::AnthropicMessages],
    };
    let catalogs = protocols
        .into_iter()
        .map(|protocol| load_pricing_catalog(protocol, catalog_options))
        .collect::<Result<Vec<_>, _>>()?;
    let entries = catalogs
        .iter()
        .flat_map(PricingCatalog::models)
        .collect::<Vec<_>>();
    if args.json {
        let values = entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "protocol": entry.protocol,
                    "id": entry.id,
                    "display_name": entry.display_name,
                    "source_kind": entry.source_kind,
                    "source": entry.source,
                    "as_of": entry.as_of,
                    "fetched_at": entry.fetched_at,
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&values)?);
        for catalog in &catalogs {
            for warning in catalog.warnings() {
                eprintln!("注意：{warning}");
            }
        }
    } else {
        println!("官方价格目录");
        for catalog in &catalogs {
            for source in catalog.source_summaries() {
                let fetched = source
                    .fetched_at
                    .as_deref()
                    .map(|value| format!("，获取于 {value}"))
                    .unwrap_or_default();
                println!(
                    "来源：{}（{}；截至 {}{}）",
                    source.source,
                    source.kind.as_str(),
                    source.as_of,
                    fetched
                );
            }
        }
        let mut last_protocol = None;
        for entry in entries {
            if last_protocol != Some(entry.protocol) {
                println!();
                println!("{}", entry.protocol);
                last_protocol = Some(entry.protocol);
            }
            println!("  {:<24} {}", entry.id, entry.display_name);
        }
        println!();
        for catalog in &catalogs {
            for warning in catalog.warnings() {
                println!("注意：{warning}");
            }
        }
    }
    Ok(())
}

fn run_wizard(catalog_options: &CatalogLoadOptions) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err("无参数交互向导需要终端；脚本中请使用 `rate-lens probe --help`".into());
    }
    eprintln!("rate-lens API 中转倍率探测向导");
    eprintln!("本向导将发起一个或多个真实、可能产生费用的请求。\n");
    let protocol = match prompt("协议 [1=OpenAI Responses, 2=Anthropic Messages]", "1")?.as_str()
    {
        "2" => Protocol::AnthropicMessages,
        _ => Protocol::OpenAiResponses,
    };
    eprintln!("正在刷新官方价格目录……");
    let catalog = load_pricing_catalog(protocol, catalog_options)?;
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
    let profile = catalog.pricing_profile(&official_model)?;
    let discovered_model = models.iter().find(|candidate| candidate.id == model);
    let plan = prompt_wizard_test_plan(protocol, &profile, discovered_model)?;
    let actual_currency = prompt_actual_currency()?;
    let exchange_rate = prompt_exchange_rate(&actual_currency)?;
    print_wizard_plan_summary(&plan, &profile, exchange_rate, &actual_currency)?;
    confirm_wizard_plan(&plan)?;
    let charge_capture = prompt_wizard_charge_capture(plan.specs.len(), &actual_currency)?;

    let mut records = Vec::with_capacity(plan.specs.len());
    let mut stopped_after_precheck = false;
    for (index, spec) in plan.specs.iter().enumerate() {
        if plan.includes_precheck && index == 1 {
            eprintln!("\n预检通过。即将开始高成本请求。上一步产生的扣费也应计入总扣费。");
            let confirmed = prompt("确认继续高成本请求？[y/N]", "n")?;
            if !is_yes(&confirmed) {
                eprintln!("已停止高成本部分；下面仍会结算已经完成的预检。");
                stopped_after_precheck = true;
                break;
            }
        }
        eprintln!(
            "\n[{}/{}] {}：目标输入 {} token……",
            index + 1,
            plan.specs.len(),
            spec.label,
            format_integer(spec.context_tokens)
        );
        let result = run_probe(&ProbeConfig {
            protocol,
            base_url: base_url.clone(),
            api_key: api_key.clone(),
            auth_style,
            model: model.clone(),
            context_tokens: spec.context_tokens,
            reasoning_effort: spec.reasoning_effort.clone(),
            max_output_tokens: spec.max_output_tokens,
            anthropic_thinking_mode: AnthropicThinkingMode::Adaptive,
            thinking_budget_tokens: 1_024,
            enable_prompt_cache: spec.enable_prompt_cache,
            prompt_cache_key: spec.prompt_cache_key.clone(),
            prompt_marker: spec.prompt_marker.clone(),
            disable_implicit_prompt_cache: spec.disable_implicit_prompt_cache,
            openai_prompt_cache_options: plan.relay_cache_supported,
            timeout_seconds: 600,
        })?;
        let mut record = build_wizard_record(
            spec.clone(),
            result,
            &catalog,
            &official_model,
            exchange_rate,
        )?;
        let invalid_reason = validate_wizard_record(&plan, index, &record);
        if let Some(reason) = invalid_reason.as_deref() {
            record.warnings.push(reason.to_owned());
        }
        eprintln!(
            "完成：实际输入 {} token，官方理论成本 {} USD（{} 档）",
            format_integer(record.analysis.usage.total_input_tokens()),
            money(record.analysis.official_cost_reference),
            record.pricing_metadata.tier
        );
        records.push(record);
        if let Some(reason) = invalid_reason {
            eprintln!("注意：{reason}");
            break;
        }
    }

    if stopped_after_precheck && records.is_empty() {
        return Err("已停止，未完成任何请求".into());
    }
    append_wizard_mode_warnings(&plan, &mut records);
    let charge_input = finish_wizard_charge_capture(charge_capture, &records, &actual_currency)?;
    print_wizard_results(&records, charge_input, exchange_rate, &actual_currency)?;
    Ok(())
}

fn prompt_wizard_test_plan(
    protocol: Protocol,
    profile: &CatalogPricingProfile,
    discovered_model: Option<&DiscoveredModel>,
) -> Result<WizardTestPlan, Box<dyn std::error::Error>> {
    let long_available = profile.long.is_some() && profile.long_threshold.is_some();
    eprintln!("\n官方计费资料：{}", profile.display_name);
    eprintln!(
        "  standard：输入 {} / 缓存读取 {} / 输出 {} USD/1M token",
        compact(profile.standard.uncached_input_per_million, 8),
        optional_rate(profile.standard.cache_read_per_million),
        compact(profile.standard.output_per_million, 8)
    );
    if let (Some(long), Some(threshold)) = (&profile.long, profile.long_threshold) {
        eprintln!(
            "  long：    输入 {} / 缓存读取 {} / 输出 {} USD/1M token",
            compact(long.uncached_input_per_million, 8),
            optional_rate(long.cache_read_per_million),
            compact(long.output_per_million, 8)
        );
        eprintln!(
            "  切换条件：实际输入 > {} token 时整次请求按 long 档计算",
            format_integer(threshold)
        );
    } else {
        eprintln!("  独立长上下文价格档：无");
    }
    let max_input = effective_max_input_tokens(profile, discovered_model);
    if let Some(limit) = max_input {
        eprintln!("  最大输入：{} token", format_integer(limit));
    }
    eprintln!("\n测试方案：");
    eprintln!("  非缓存方案不会发送缓存控制参数；缓存方案需单独确认中转站兼容性。");
    eprintln!("  1. 连通性预检       约 1K，1 次请求；只验证端点和 usage");
    eprintln!("  2. 常规档倍率       约 8K，1 次请求（推荐）");
    if long_available {
        eprintln!("  3. 长上下文档倍率   1K 预检 + 1 次阈值以上请求");
        eprintln!("  4. 分档边界对照     1K 预检 + 阈值下/上各 1 次");
        eprintln!("  5. 缓存倍率         两次相同的约 8K 请求（需确认中转站支持缓存参数）");
        eprintln!("  6. 自定义           手工设置输入、输出和推理参数");
    } else {
        eprintln!("  3. 缓存倍率         两次相同的约 8K 请求（需确认中转站支持缓存参数）");
        eprintln!("  4. 自定义           手工设置输入、输出和推理参数");
    }

    loop {
        let answer = prompt("选择编号", "2")?;
        let kind = match (long_available, answer.as_str()) {
            (_, "1") => WizardTestKind::Connectivity,
            (_, "2") => WizardTestKind::Standard,
            (true, "3") => WizardTestKind::Long,
            (true, "4") => WizardTestKind::TierBoundary,
            (true, "5") | (false, "3") => WizardTestKind::Cache,
            (true, "6") | (false, "4") => WizardTestKind::Custom,
            _ => {
                eprintln!("请输入列表中的编号。");
                continue;
            }
        };
        let relay_cache_supported = if kind == WizardTestKind::Cache {
            if !prompt_relay_cache_support(protocol)? {
                eprintln!("未确认中转站支持缓存参数；本次不会发送缓存测试请求，请选择其他方案。\n");
                continue;
            }
            true
        } else {
            false
        };
        return build_wizard_test_plan(kind, protocol, profile, max_input, relay_cache_supported);
    }
}

fn prompt_relay_cache_support(protocol: Protocol) -> Result<bool, Box<dyn std::error::Error>> {
    let fields = match protocol {
        Protocol::OpenAiResponses => "prompt_cache_options 和 prompt_cache_key",
        Protocol::AnthropicMessages => "cache_control（ephemeral）",
    };
    eprintln!(
        "\n缓存测试会发送 {fields}。请先查阅中转站文档或完成兼容性确认；官方模型支持不代表中转站支持。"
    );
    let answer = prompt("已明确确认中转站支持这些缓存参数？[y/N]", "n")?;
    Ok(is_yes(&answer))
}

fn build_wizard_test_plan(
    kind: WizardTestKind,
    protocol: Protocol,
    profile: &CatalogPricingProfile,
    max_input_tokens: Option<u64>,
    relay_cache_supported: bool,
) -> Result<WizardTestPlan, Box<dyn std::error::Error>> {
    if kind == WizardTestKind::Cache && !relay_cache_supported {
        return Err("缓存测试需要先明确确认中转站支持缓存参数".into());
    }
    let preset_reasoning = preset_reasoning_effort(protocol, profile);
    let make_spec =
        |label: &str, context_tokens, max_output_tokens, enable_prompt_cache| WizardProbeSpec {
            label: label.to_owned(),
            context_tokens,
            max_output_tokens,
            reasoning_effort: preset_reasoning.clone(),
            enable_prompt_cache,
            prompt_cache_key: None,
            prompt_marker: (protocol == Protocol::OpenAiResponses && !enable_prompt_cache)
                .then(wizard_cache_key),
            disable_implicit_prompt_cache: false,
        };
    let long_threshold = profile.long_threshold;
    let (specs, includes_precheck) = match kind {
        WizardTestKind::Connectivity => (
            vec![make_spec(
                "连通性预检",
                WIZARD_CONNECTIVITY_TOKENS,
                WIZARD_PRECHECK_OUTPUT_TOKENS,
                false,
            )],
            false,
        ),
        WizardTestKind::Standard => (
            vec![make_spec(
                "常规档倍率",
                WIZARD_STANDARD_TOKENS,
                WIZARD_DEFAULT_OUTPUT_TOKENS,
                false,
            )],
            false,
        ),
        WizardTestKind::Long => {
            let threshold = long_threshold.ok_or("官方目录没有长上下文切换阈值")?;
            let target = suggested_long_target(threshold, max_input_tokens)?;
            (
                vec![
                    make_spec(
                        "低成本预检",
                        WIZARD_CONNECTIVITY_TOKENS,
                        WIZARD_PRECHECK_OUTPUT_TOKENS,
                        false,
                    ),
                    make_spec(
                        "长上下文档倍率",
                        target,
                        WIZARD_DEFAULT_OUTPUT_TOKENS,
                        false,
                    ),
                ],
                true,
            )
        }
        WizardTestKind::TierBoundary => {
            let threshold = long_threshold.ok_or("官方目录没有长上下文切换阈值")?;
            let (below, above) = suggested_boundary_targets(threshold, max_input_tokens)?;
            (
                vec![
                    make_spec(
                        "低成本预检",
                        WIZARD_CONNECTIVITY_TOKENS,
                        WIZARD_PRECHECK_OUTPUT_TOKENS,
                        false,
                    ),
                    make_spec("阈值以下样本", below, WIZARD_DEFAULT_OUTPUT_TOKENS, false),
                    make_spec("阈值以上样本", above, WIZARD_DEFAULT_OUTPUT_TOKENS, false),
                ],
                true,
            )
        }
        WizardTestKind::Cache => {
            let cache_key = wizard_cache_key();
            let prompt_marker = format!("cache-{cache_key}");
            let mut first = make_spec(
                "缓存写入样本",
                WIZARD_CACHE_TOKENS,
                WIZARD_DEFAULT_OUTPUT_TOKENS,
                true,
            );
            first.prompt_cache_key = Some(cache_key.clone());
            first.prompt_marker = Some(prompt_marker.clone());
            let mut second = make_spec(
                "缓存复用样本",
                WIZARD_CACHE_TOKENS,
                WIZARD_DEFAULT_OUTPUT_TOKENS,
                true,
            );
            second.prompt_cache_key = Some(cache_key);
            second.prompt_marker = Some(prompt_marker);
            (vec![first, second], false)
        }
        WizardTestKind::Custom => {
            let context_tokens = parse_prompt::<u64>("目标输入 token", "1000")?;
            let reasoning = prompt_reasoning_effort(protocol)?;
            let max_output_tokens =
                parse_prompt::<u64>("最大输出 token", &WIZARD_DEFAULT_OUTPUT_TOKENS.to_string())?;
            (
                vec![WizardProbeSpec {
                    label: "自定义测试".to_owned(),
                    context_tokens,
                    max_output_tokens,
                    reasoning_effort: reasoning,
                    enable_prompt_cache: false,
                    prompt_cache_key: None,
                    prompt_marker: None,
                    disable_implicit_prompt_cache: false,
                }],
                false,
            )
        }
    };
    Ok(WizardTestPlan {
        kind,
        specs,
        long_threshold,
        includes_precheck,
        relay_cache_supported: kind == WizardTestKind::Cache && relay_cache_supported,
    })
}

fn effective_max_input_tokens(
    profile: &CatalogPricingProfile,
    discovered_model: Option<&DiscoveredModel>,
) -> Option<u64> {
    match (
        profile.max_input_tokens,
        discovered_model.and_then(|model| model.max_input_tokens),
    ) {
        (Some(official), Some(relay)) => Some(official.min(relay)),
        (Some(official), None) => Some(official),
        (None, Some(relay)) => Some(relay),
        (None, None) => None,
    }
}

fn preset_reasoning_effort(protocol: Protocol, profile: &CatalogPricingProfile) -> Option<String> {
    if protocol != Protocol::OpenAiResponses {
        return None;
    }
    let model = profile.official_model.to_ascii_lowercase();
    ["gpt-5.6", "gpt-5.5", "gpt-5.4"]
        .iter()
        .any(|prefix| model == *prefix || model.starts_with(&format!("{prefix}-")))
        .then(|| "none".to_owned())
}

fn suggested_long_target(
    threshold: u64,
    max_input_tokens: Option<u64>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let raw_target = threshold
        .checked_add((threshold / 10).max(WIZARD_LONG_MARGIN_MIN))
        .ok_or("长上下文目标超出整数范围")?;
    let target = round_up_tokens(raw_target, 10_000)?;
    if max_input_tokens.is_some_and(|limit| target > limit) {
        return Err(format!(
            "模型最大输入不足以在 {} token 阈值上方保留安全余量",
            format_integer(threshold)
        )
        .into());
    }
    Ok(target)
}

fn suggested_boundary_targets(
    threshold: u64,
    max_input_tokens: Option<u64>,
) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let raw_margin = (threshold / 10).max(WIZARD_LONG_MARGIN_MIN);
    let below = round_down_tokens(threshold.saturating_sub(raw_margin).max(1), 10_000).max(1);
    let above = round_up_tokens(
        threshold
            .checked_add(raw_margin)
            .ok_or("长上下文目标超出整数范围")?,
        10_000,
    )?;
    if max_input_tokens.is_some_and(|limit| above > limit) {
        return Err(format!(
            "模型最大输入不足以在 {} token 阈值两侧保留安全余量",
            format_integer(threshold)
        )
        .into());
    }
    Ok((below, above))
}

fn round_up_tokens(value: u64, quantum: u64) -> Result<u64, Box<dyn std::error::Error>> {
    value
        .checked_add(quantum - 1)
        .map(|value| value / quantum * quantum)
        .ok_or_else(|| "token 目标超出整数范围".into())
}

fn round_down_tokens(value: u64, quantum: u64) -> u64 {
    value / quantum * quantum
}

fn wizard_cache_key() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("rate-lens-{nonce}")
}

fn optional_rate(rate: Option<Decimal>) -> String {
    rate.map(|value| compact(value, 8))
        .unwrap_or_else(|| "未列出".to_owned())
}

fn print_wizard_plan_summary(
    plan: &WizardTestPlan,
    profile: &CatalogPricingProfile,
    exchange_rate: Decimal,
    actual_currency: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("\n即将执行：{}", plan.kind.label());
    let mut total = Decimal::ZERO;
    for (index, spec) in plan.specs.iter().enumerate() {
        let pricing = pricing_for_target(profile, spec.context_tokens);
        let estimate = estimate_wizard_token_cost(spec, &pricing);
        total += estimate;
        eprintln!(
            "  {}. {:<16} 输入约 {}，输出上限 {}，token 成本上界约 {} USD",
            index + 1,
            spec.label,
            format_integer(spec.context_tokens),
            format_integer(spec.max_output_tokens),
            money(estimate)
        );
    }
    eprintln!("  请求数：{}", plan.specs.len());
    eprintln!("  token 成本上界合计：约 {} USD", money(total));
    if actual_currency != "USD" || exchange_rate != Decimal::ONE {
        eprintln!(
            "  按当前汇率折算：约 {} {}",
            money(total * exchange_rate),
            actual_currency
        );
    }
    let reasoning = plan
        .specs
        .iter()
        .find_map(|spec| spec.reasoning_effort.as_deref())
        .map(|effort| format!("reasoning.effort = {effort}"))
        .unwrap_or_else(|| "不发送 reasoning/thinking 参数".to_owned());
    eprintln!("  推理：{reasoning}；最终费用始终按响应 usage 重新计算");
    if plan.kind == WizardTestKind::Cache {
        eprintln!("  缓存测试按最坏情况把两次输入都按缓存写入价估算。");
    }
    Ok(())
}

fn pricing_for_target(profile: &CatalogPricingProfile, input_tokens: u64) -> Pricing {
    if let (Some(long), Some(threshold)) = (&profile.long, profile.long_threshold)
        && input_tokens > threshold
    {
        return long.clone();
    }
    profile.standard.clone()
}

fn estimate_wizard_token_cost(spec: &WizardProbeSpec, pricing: &Pricing) -> Decimal {
    let rate = if spec.enable_prompt_cache {
        pricing
            .cache_write_per_million
            .or(pricing.cache_write_5m_per_million)
            .unwrap_or(pricing.uncached_input_per_million)
    } else {
        pricing.uncached_input_per_million
    };
    rate_lens::approximate_official_input_cost(spec.context_tokens, rate)
        + rate_lens::approximate_official_input_cost(
            spec.max_output_tokens,
            pricing.output_per_million,
        )
}

fn confirm_wizard_plan(plan: &WizardTestPlan) -> Result<(), Box<dyn std::error::Error>> {
    let confirmed = prompt("确认发起上述真实请求？[y/N]", "n")?;
    if !is_yes(&confirmed) {
        return Err("已取消，未发起推理请求".into());
    }
    if plan.includes_precheck {
        eprintln!("高成本部分会在低成本预检成功后再次确认。");
    }
    Ok(())
}

fn build_wizard_record(
    spec: WizardProbeSpec,
    result: ProbeResult,
    catalog: &PricingCatalog,
    official_model: &str,
    exchange_rate: Decimal,
) -> Result<WizardProbeRecord, Box<dyn std::error::Error>> {
    let (pricing, metadata, mut warnings) = resolve_requested_pricing(
        catalog,
        official_model,
        result.report.usage.total_input_tokens(),
        PriceTier::Auto,
        ManualRates::empty(),
    )?;
    let pricing = pricing.ok_or("无法按实际 usage 确定官方价格")?;
    let metadata = metadata.ok_or("官方价格元数据缺失")?;
    warnings.extend(result.warnings);
    append_usage_warnings(&result.report.usage, &mut warnings);
    let analysis = analyze_usage(result.report.usage, pricing.clone(), None, exchange_rate)?;
    Ok(WizardProbeRecord {
        spec,
        counted_input_tokens: result.counted_input_tokens,
        analysis,
        pricing,
        pricing_metadata: metadata,
        warnings,
    })
}

fn validate_wizard_record(
    plan: &WizardTestPlan,
    index: usize,
    record: &WizardProbeRecord,
) -> Option<String> {
    let threshold = plan.long_threshold?;
    let actual = record.analysis.usage.total_input_tokens();
    match plan.kind {
        WizardTestKind::Long if index == 1 && actual <= threshold => Some(format!(
            "长档样本实际输入为 {} token，未超过官方阈值 {}；不能把该结果当作长档倍率",
            format_integer(actual),
            format_integer(threshold)
        )),
        WizardTestKind::TierBoundary if index == 1 && actual > threshold => Some(format!(
            "阈值以下样本实际输入为 {} token，已超过官方阈值 {}；边界对照无效",
            format_integer(actual),
            format_integer(threshold)
        )),
        WizardTestKind::TierBoundary if index == 2 && actual <= threshold => Some(format!(
            "阈值以上样本实际输入为 {} token，未超过官方阈值 {}；边界对照无效",
            format_integer(actual),
            format_integer(threshold)
        )),
        _ => None,
    }
}

fn append_wizard_mode_warnings(plan: &WizardTestPlan, records: &mut [WizardProbeRecord]) {
    match plan.kind {
        WizardTestKind::Connectivity => records[0].warnings.push(
            "连通性预检样本很小，只适合验证端点和 usage，不建议据此判断稳定倍率。".to_owned(),
        ),
        WizardTestKind::Cache if records.len() == 2 => {
            let cache_writes_are_metered = records[0].pricing.cache_write_per_million.is_some()
                || records[0].pricing.cache_write_5m_per_million.is_some()
                || records[0].pricing.cache_write_1h_per_million.is_some();
            let wrote = records[0].analysis.usage.cache_write_input_tokens
                + records[0].analysis.usage.cache_write_5m_input_tokens
                + records[0].analysis.usage.cache_write_1h_input_tokens
                > 0;
            let hit = records[1].analysis.usage.cache_read_input_tokens > 0;
            if cache_writes_are_metered && !wrote {
                records[0].warnings.push(
                    "首个请求没有报告缓存写入 token；中转站可能未实现或未透传缓存计量。".to_owned(),
                );
            }
            if !hit {
                records[1].warnings.push(
                    "第二个相同请求没有报告缓存读取 token；缓存倍率测试未命中，不能据此判断缓存价。".to_owned(),
                );
            }
        }
        _ => {}
    }
}

fn prompt_wizard_charge_capture(
    request_count: usize,
    actual_currency: &str,
) -> Result<WizardChargeCapture, Box<dyn std::error::Error>> {
    eprintln!("\n中转扣费记录方式（原始单位：{actual_currency}）：");
    eprintln!("  1. 不记录，只查看官方理论价");
    eprintln!("  2. 请求结束后输入本轮总扣费");
    eprintln!("  3. 现在记录请求前余额，结束后输入请求后余额（推荐）");
    if request_count > 1 {
        eprintln!("  4. 结束后逐请求输入扣费");
    }
    loop {
        let answer = prompt("选择编号", "3")?;
        match answer.as_str() {
            "1" => return Ok(WizardChargeCapture::None),
            "2" => return Ok(WizardChargeCapture::Total),
            "3" => {
                let before =
                    parse_prompt::<Decimal>(&format!("请求前余额（{actual_currency}）"), "")?;
                ensure_nonnegative_decimal(before, "请求前余额")?;
                return Ok(WizardChargeCapture::Balance { before });
            }
            "4" if request_count > 1 => return Ok(WizardChargeCapture::PerRequest),
            _ => eprintln!("请输入列表中的编号。"),
        }
    }
}

fn finish_wizard_charge_capture(
    capture: WizardChargeCapture,
    records: &[WizardProbeRecord],
    actual_currency: &str,
) -> Result<WizardChargeInput, Box<dyn std::error::Error>> {
    match capture {
        WizardChargeCapture::None => Ok(WizardChargeInput::None),
        WizardChargeCapture::Total => {
            let total = parse_prompt::<Decimal>(&format!("本轮总扣费（{actual_currency}）"), "")?;
            ensure_nonnegative_decimal(total, "本轮总扣费")?;
            Ok(WizardChargeInput::Total(total))
        }
        WizardChargeCapture::Balance { before } => {
            let after = parse_prompt::<Decimal>(&format!("请求后余额（{actual_currency}）"), "")?;
            ensure_nonnegative_decimal(after, "请求后余额")?;
            if after > before {
                return Err("请求后余额大于请求前余额；请确认是否填反或期间发生充值".into());
            }
            let charged = before - after;
            eprintln!("自动计算本轮扣费：{} {actual_currency}", money(charged));
            Ok(WizardChargeInput::Total(charged))
        }
        WizardChargeCapture::PerRequest => {
            let mut values = Vec::with_capacity(records.len());
            for record in records {
                let value = prompt(
                    &format!("{}扣费（{actual_currency}；可留空）", record.spec.label),
                    "",
                )?;
                let value = parse_optional_decimal(&value)?;
                if let Some(value) = value {
                    ensure_nonnegative_decimal(value, "逐请求扣费")?;
                }
                values.push(value);
            }
            Ok(WizardChargeInput::PerRequest(values))
        }
    }
}

fn ensure_nonnegative_decimal(
    value: Decimal,
    label: &'static str,
) -> Result<(), Box<dyn std::error::Error>> {
    if value < Decimal::ZERO {
        Err(format!("{label}不能为负数").into())
    } else {
        Ok(())
    }
}

fn print_wizard_results(
    records: &[WizardProbeRecord],
    charge_input: WizardChargeInput,
    exchange_rate: Decimal,
    actual_currency: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let per_request = match &charge_input {
        WizardChargeInput::PerRequest(values) => Some(values.as_slice()),
        _ => None,
    };
    for (index, record) in records.iter().enumerate() {
        let charged = match &charge_input {
            WizardChargeInput::Total(value) if records.len() == 1 => Some(*value),
            _ => per_request
                .and_then(|values| values.get(index))
                .copied()
                .flatten(),
        };
        let analysis = analyze_usage(
            record.analysis.usage.clone(),
            record.pricing.clone(),
            charged,
            exchange_rate,
        )?;
        let mut warnings = record.warnings.clone();
        if let Some(multiplier) = analysis.observed_multiplier
            && let Some(warning) = suspicious_multiplier_warning(multiplier, actual_currency)
        {
            warnings.push(warning);
        }
        println!("\n=== {} ===", record.spec.label);
        println!(
            "目标上下文    {} token",
            format_integer(record.spec.context_tokens)
        );
        if let Some(counted) = record.counted_input_tokens {
            println!("计数端点结果  {} token", format_integer(counted));
        }
        print_human(
            &analysis,
            &warnings,
            Some(&record.pricing_metadata),
            "USD",
            actual_currency,
        );
    }

    if records.len() > 1 {
        let official_total = records
            .iter()
            .map(|record| record.analysis.official_cost_reference)
            .sum::<Decimal>();
        let charged_total = match charge_input {
            WizardChargeInput::None => None,
            WizardChargeInput::Total(value) => Some(value),
            WizardChargeInput::PerRequest(values) => {
                values.into_iter().try_fold(Decimal::ZERO, |total, value| {
                    value.map(|value| total + value)
                })
            }
        };
        println!("\n=== 本轮合计 ===");
        println!("请求数        {}", records.len());
        println!("官方理论成本  {} USD", money(official_total));
        let official_actual = official_total * exchange_rate;
        if actual_currency != "USD" || exchange_rate != Decimal::ONE {
            println!(
                "折算理论成本  {} {}",
                money(official_actual),
                actual_currency
            );
        }
        if let Some(charged) = charged_total {
            let multiplier = safe_multiplier(charged, official_actual)?;
            println!("中转实际扣费  {} {}", money(charged), actual_currency);
            println!("合计观测倍率  {}×", fixed(multiplier, 4));
            print_suspicious_multiplier_warning(multiplier, actual_currency);
        } else {
            println!("合计观测倍率  未计算（未提供完整的本轮扣费）");
        }
    }
    Ok(())
}

fn print_suspicious_multiplier_warning(multiplier: Decimal, actual_currency: &str) {
    if let Some(warning) = suspicious_multiplier_warning(multiplier, actual_currency) {
        println!("注意          {warning}");
    }
}

fn suspicious_multiplier_warning(multiplier: Decimal, actual_currency: &str) -> Option<String> {
    (multiplier < Decimal::new(1, 1) || multiplier > Decimal::TEN).then(|| {
        format!(
            "倍率超出 0.1×–10× 常见检查区间；请确认账单数值确实以 {actual_currency} 计价，并排除免费额度、返利、退款和并发请求。"
        )
    })
}

fn safe_multiplier(
    charged: Decimal,
    official_cost: Decimal,
) -> Result<Decimal, Box<dyn std::error::Error>> {
    if official_cost == Decimal::ZERO {
        Err("官方理论成本为 0，无法计算倍率".into())
    } else {
        Ok(charged / official_cost)
    }
}

fn is_yes(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "y" | "yes")
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
    catalog: &PricingCatalog,
    model: &str,
    input_tokens: u64,
    tier: PriceTier,
    manual: ManualRates,
) -> Result<PricingResolution, Box<dyn std::error::Error>> {
    let resolved = catalog.resolve(model, input_tokens, tier);
    match resolved {
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
                "未使用官方价格目录（{error}）；本次按手工价格计算。"
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
            "官方对照模型  {}（{} 档；价格截至 {}）",
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
        println!(
            "价格来源      {}（{}）",
            metadata.source,
            metadata.source_kind.as_str()
        );
        if let Some(fetched_at) = metadata.fetched_at.as_deref() {
            println!("获取时间      {fetched_at}");
        }
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
        source_kind: resolved.source_kind,
        as_of: resolved.as_of.to_owned(),
        fetched_at: resolved.fetched_at.clone(),
        etag: resolved.etag.clone(),
        last_modified: resolved.last_modified.clone(),
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
    eprintln!("\n中转站后台显示的余额/账单原始单位：");
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
    loop {
        let rate = parse_prompt::<Decimal>(&format!("1 USD 对应多少 {actual_currency}"), &default)?;
        if rate > Decimal::ZERO {
            return Ok(rate);
        }
        eprintln!("汇率必须大于 0。");
    }
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

impl From<PricingSourceArg> for PricingSourceMode {
    fn from(value: PricingSourceArg) -> Self {
        match value {
            PricingSourceArg::Auto => Self::Auto,
            PricingSourceArg::Live => Self::Live,
            PricingSourceArg::Builtin => Self::Builtin,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn profile() -> CatalogPricingProfile {
        CatalogPricingProfile {
            official_model: "gpt-5.6-sol".to_owned(),
            display_name: "GPT-5.6 Sol".to_owned(),
            standard: Pricing {
                uncached_input_per_million: Decimal::from(4),
                cache_read_per_million: Some(Decimal::new(4, 1)),
                cache_write_per_million: Some(Decimal::from(5)),
                cache_write_5m_per_million: None,
                cache_write_1h_per_million: None,
                output_per_million: Decimal::from(20),
                extra_official_cost: Decimal::ZERO,
            },
            long: Some(Pricing {
                uncached_input_per_million: Decimal::from(8),
                cache_read_per_million: Some(Decimal::new(8, 1)),
                cache_write_per_million: Some(Decimal::from(10)),
                cache_write_5m_per_million: None,
                cache_write_1h_per_million: None,
                output_per_million: Decimal::from(30),
                extra_official_cost: Decimal::ZERO,
            }),
            long_threshold: Some(272_000),
            max_input_tokens: Some(922_000),
            source: "https://developers.openai.com/api/docs/pricing".to_owned(),
            source_kind: CatalogSourceKind::Builtin,
            as_of: "2026-08-29".to_owned(),
            fetched_at: None,
        }
    }

    fn record(actual_input_tokens: u64) -> WizardProbeRecord {
        let pricing = profile().standard;
        let analysis = analyze_usage(
            rate_lens::NormalizedUsage {
                protocol: Protocol::OpenAiResponses,
                models: BTreeSet::from(["gpt-5.6-sol".to_owned()]),
                service_tiers: BTreeSet::new(),
                requests: 1,
                uncached_input_tokens: actual_input_tokens,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                cache_write_5m_input_tokens: 0,
                cache_write_1h_input_tokens: 0,
                output_tokens: 1,
                reasoning_output_tokens: 0,
                metered_extras: BTreeMap::new(),
            },
            pricing.clone(),
            None,
            Decimal::ONE,
        )
        .unwrap();
        WizardProbeRecord {
            spec: WizardProbeSpec {
                label: "样本".to_owned(),
                context_tokens: actual_input_tokens,
                max_output_tokens: 64,
                reasoning_effort: None,
                enable_prompt_cache: false,
                prompt_cache_key: None,
                prompt_marker: None,
                disable_implicit_prompt_cache: true,
            },
            counted_input_tokens: Some(actual_input_tokens),
            analysis,
            pricing,
            pricing_metadata: PricingMetadata {
                official_model: "gpt-5.6-sol".to_owned(),
                display_name: "GPT-5.6 Sol".to_owned(),
                tier: "standard".to_owned(),
                source: "test".to_owned(),
                source_kind: CatalogSourceKind::Builtin,
                as_of: "2026-08-29".to_owned(),
                fetched_at: None,
                etag: None,
                last_modified: None,
            },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn long_presets_keep_a_safe_margin_around_threshold() {
        assert_eq!(
            suggested_long_target(272_000, Some(922_000)).unwrap(),
            300_000
        );
        assert_eq!(
            suggested_boundary_targets(272_000, Some(922_000)).unwrap(),
            (240_000, 300_000)
        );
    }

    #[test]
    fn cache_plan_uses_same_run_unique_marker_for_both_requests() {
        let plan = build_wizard_test_plan(
            WizardTestKind::Cache,
            Protocol::OpenAiResponses,
            &profile(),
            Some(922_000),
            true,
        )
        .unwrap();
        assert_eq!(plan.specs.len(), 2);
        assert_eq!(plan.specs[0].prompt_marker, plan.specs[1].prompt_marker);
        assert_eq!(
            plan.specs[0].prompt_cache_key,
            plan.specs[1].prompt_cache_key
        );
        assert!(plan.specs[0].enable_prompt_cache);
        assert!(plan.relay_cache_supported);
    }

    #[test]
    fn ordinary_presets_do_not_infer_or_send_relay_cache_controls() {
        let plan = build_wizard_test_plan(
            WizardTestKind::Standard,
            Protocol::OpenAiResponses,
            &profile(),
            Some(922_000),
            false,
        )
        .unwrap();
        assert!(!plan.relay_cache_supported);
        assert!(!plan.specs[0].enable_prompt_cache);
        assert!(!plan.specs[0].disable_implicit_prompt_cache);
        assert!(plan.specs[0].prompt_marker.is_some());
    }

    #[test]
    fn cache_plan_requires_explicit_relay_support_confirmation() {
        assert!(
            build_wizard_test_plan(
                WizardTestKind::Cache,
                Protocol::OpenAiResponses,
                &profile(),
                Some(922_000),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn actual_usage_must_land_on_requested_side_of_pricing_boundary() {
        let long_plan = WizardTestPlan {
            kind: WizardTestKind::Long,
            specs: Vec::new(),
            long_threshold: Some(272_000),
            includes_precheck: true,
            relay_cache_supported: false,
        };
        assert!(validate_wizard_record(&long_plan, 1, &record(272_000)).is_some());
        assert!(validate_wizard_record(&long_plan, 1, &record(272_001)).is_none());

        let boundary_plan = WizardTestPlan {
            kind: WizardTestKind::TierBoundary,
            specs: Vec::new(),
            long_threshold: Some(272_000),
            includes_precheck: true,
            relay_cache_supported: false,
        };
        assert!(validate_wizard_record(&boundary_plan, 1, &record(272_001)).is_some());
        assert!(validate_wizard_record(&boundary_plan, 2, &record(272_000)).is_some());
    }

    #[test]
    fn suspicious_multiplier_bounds_are_safe() {
        assert_eq!(
            safe_multiplier(Decimal::from(2), Decimal::from(4)).unwrap(),
            Decimal::new(5, 1)
        );
        assert!(safe_multiplier(Decimal::ONE, Decimal::ZERO).is_err());
    }

    #[test]
    fn known_gpt_presets_explicitly_disable_reasoning() {
        assert_eq!(
            preset_reasoning_effort(Protocol::OpenAiResponses, &profile()).as_deref(),
            Some("none")
        );
        assert_eq!(
            preset_reasoning_effort(Protocol::AnthropicMessages, &profile()),
            None
        );
    }
}
