# rate-lens

`rate-lens` 用真实 API `usage` 计算一次请求在官方定价下的理论成本，并在你提供中转站实际扣费时计算观测倍率。

```text
观测倍率 = 中转实际扣费 ÷（官方理论成本 × 汇率）
```

支持：

- OpenAI Responses API：`POST /v1/responses`
- Anthropic Messages API：`POST /v1/messages`
- 从 `GET /v1/models` 发现中转站模型
- 用官方兼容的 token-count 端点校准目标上下文
- 选择推理深度或 Anthropic thinking 模式
- 启动时自动获取官方模型价格，支持缓存、内置回退和手工覆盖
- 保留 JSON、JSONL 和 SSE 的离线分析能力

## 最快使用方式

安装后直接运行，不带参数会进入交互向导：

```bash
cargo install --path .
rate-lens
```

向导会依次询问协议、`base_url`、API Key、模型、官方对照模型、测试方案和中转站账单原始单位。API Key 使用隐藏输入，不会写入命令历史。选择货币后，向导会查询带日期的 USD 市场参考汇率并允许覆盖；发送真实请求前会展示请求数、各请求 token、费用上界并要求确认。

测试方案不再要求普通用户先猜 token 数：

- **连通性预检**：约 1K 输入、16 输出、关闭推理；只验证端点和 `usage`，不建议作为稳定倍率结论。
- **常规档倍率（默认）**：约 8K 输入、64 输出、关闭推理。
- **长上下文档倍率**：仅在官方目录同时确认长档价格和切换阈值时显示；先发 1K 预检，再二次确认高成本请求。GPT-5.6/GPT-5.5/GPT-5.4 当前自动建议约 300K 输入。
- **分档边界对照**：先预检，再分别发送阈值以下和以上的样本；最终按真实响应 `usage` 检查样本是否位于预期一侧。
- **缓存倍率**：两次发送带本轮唯一标记的相同约 8K 请求，并根据响应中的缓存写入/读取 token 判断是否真正命中。
- **自定义**：保留手工输入上下文、输出上限和推理深度。

对于官方文档明确支持 `none` 的 GPT-5.6/5.5/5.4，预设测试会发送 `reasoning.effort = none`；其他模型在目录无法确认支持范围时不发送 reasoning/thinking 参数，确认页会明确显示实际选择。所有非缓存预设不会根据官方模型名发送 `prompt_cache_options`；OpenAI 样本会使用每次不同的批次标记，降低既有隐式缓存命中的可能，防止缓存读写改变本来要测的常规输入价格。选择缓存方案时，向导会先要求确认中转站已支持对应字段，未确认就不会发起缓存测试；OpenAI 使用 `prompt_cache_options`/`prompt_cache_key`，Anthropic 使用 `cache_control`。长上下文预检成功后仍会再次确认，避免误发高成本请求。

脚本或非交互环境使用 `probe`，并显式传入 `--yes`：

```bash
export RELAY_API_KEY='你的中转站密钥'

rate-lens probe \
  --protocol openai \
  --base-url https://relay.example.com \
  --api-key-env RELAY_API_KEY \
  --model gpt-5.4 \
  --context-tokens 10000 \
  --reasoning high \
  --max-output-tokens 64 \
  --yes
```

输出至少包含真实输入/输出 token、所用官方价格档和本次请求的官方理论成本。标准模型响应不包含中转站实际扣费，因此没有扣费数据时倍率会显示为“未计算”。

交互向导会在请求前选择扣费记录方式。推荐先输入请求前余额，请求完成后再输入请求后余额，工具自动计算本轮差值；也可以在结束后输入总扣费或逐请求扣费。非交互模式通常要等请求结束才能查看账单，建议先保存响应：

```bash
rate-lens probe \
  --protocol openai \
  --base-url https://relay.example.com/v1 \
  --api-key-env RELAY_API_KEY \
  --model relay-gpt \
  --official-model gpt-5.4 \
  --context-tokens 10000 \
  --save-response response.json \
  --yes
```

查到这次请求的扣费后，对保存的同一响应计算倍率，不会再发模型请求：

```bash
rate-lens analyze response.json \
  --official-model gpt-5.4 \
  --charged 0.12
```

如果中转站能在发送前给出确定的本次扣费，也可以直接给 `probe` 传 `--charged`。

`--model` 是发给中转站的模型 ID，`--official-model` 是价格对照模型。中转站使用别名时务必分别填写；工具无法从 API 响应证明中转站内部实际调用的是哪个上游模型。

## 获取中转站模型

```bash
rate-lens models \
  --protocol anthropic \
  --base-url https://relay.example.com \
  --api-key-env RELAY_API_KEY
```

OpenAI 默认使用 `Authorization: Bearer`；Anthropic 默认使用 `x-api-key` 和 `anthropic-version: 2023-06-01`。如果中转站对 Anthropic 兼容端点也要求 Bearer，可加：

```bash
--auth-style bearer
```

某些中转站没有实现 `/v1/models`，此时 `probe --model ...` 仍可直接手填模型。

## 上下文长度和推理深度

`--context-tokens N` 不是简单把字符数当成 token。工具会生成确定性测试文本，并尝试调用：

- OpenAI：`POST /v1/responses/input_tokens`
- Anthropic：`POST /v1/messages/count_tokens`

它会迭代调整文本，使计数接近目标值。中转站没有实现计数端点时，会明确提示并退回近似文本；最终费用仍始终按推理响应里的真实 `usage` 计算。

校准可能调用多次计数端点，工具会在发生时提示。官方计数端点通常不是模型生成请求，但中转站可能自行计费；计算余额差时应先确认其规则。

OpenAI 推理示例：

```bash
--reasoning none
--reasoning minimal
--reasoning low
--reasoning medium
--reasoning high
--reasoning xhigh
--reasoning max
```

交互向导会列出 `none/minimal/low/medium/high/xhigh/max`，也允许输入未来新增的自定义值。工具会原样发送 `reasoning.effort`；不是每个模型都支持每个档位，服务端拒绝时不会静默降级。

Anthropic 新模型默认发送 adaptive thinking：

```json
{
  "thinking": { "type": "adaptive" },
  "output_config": { "effort": "high" }
}
```

交互向导为 adaptive thinking 提供关闭、`low`、`medium`、`high`、`max` 和自定义值。具体支持范围同样由所选模型决定。

旧模型可改用：

```bash
--anthropic-thinking enabled \
--thinking-budget-tokens 4096 \
--max-output-tokens 4160
```

`enabled` 模式要求 thinking budget 至少为 1024，而且 `max-output-tokens` 必须大于 budget。

## 官方价格自动更新和长上下文档

默认价格来源模式是 `auto`。每次运行需要计价的命令时，工具会读取官方 Markdown 价格页；成功后把解析并校验过的目录写入本地缓存。OpenAI 支持 `ETag`/`Last-Modified` 条件请求，页面未变化时直接复用缓存；Anthropic 没有稳定的条件缓存头时会重新下载。

如果官方页面暂时不可访问或解析失败，`auto` 会依次回退到上次成功缓存和编译时内置快照，并在输出中明确标出 `live`、`cache` 或 `builtin` 及回退原因。一次程序运行只加载一次所选协议的目录，不会在预估和最终结算阶段重复联网。

查看当前目录：

```bash
rate-lens catalog
rate-lens catalog --protocol openai --json
```

可显式选择来源模式：

```bash
# 默认：尝试实时刷新，失败时安全回退
rate-lens --pricing-source auto catalog

# 必须使用本次实时获取结果；网络或解析失败即报错
rate-lens --pricing-source live catalog --protocol openai

# 完全离线，只使用编译时快照
rate-lens --pricing-source builtin analyze response.json --official-model gpt-5.4
```

获取超时可用 `--pricing-timeout 30` 调整；缓存目录可用 `--pricing-cache-dir PATH` 或 `RATE_LENS_CACHE_DIR` 指定。默认缓存位置遵循平台约定：macOS 为 `~/Library/Caches/rate-lens`，Linux 为 `$XDG_CACHE_HOME/rate-lens`（未设置时 `~/.cache/rate-lens`），Windows 为 `%LOCALAPPDATA%\\rate-lens`。

联网请求遵循 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 和对应小写变量。例如：

```bash
export http_proxy=http://127.0.0.1:10808
export https_proxy=http://127.0.0.1:10808
rate-lens --pricing-source live catalog
```

两家都没有公开稳定的定价 JSON API，因此自动更新解析的是以下官方 Markdown 文档，而 `/v1/models` 不参与价格获取：

- [OpenAI API Pricing Markdown](https://developers.openai.com/api/docs/pricing.md)
- [Anthropic API Pricing Markdown](https://platform.claude.com/docs/en/about-claude/pricing.md)
- [Anthropic Context Windows](https://platform.claude.com/docs/en/build-with-claude/context-windows)
- [Anthropic Service Tiers](https://platform.claude.com/docs/en/api/service-tiers)

远程结果只有在来源地址、Content-Type、模型数量、模型唯一性和所有价格字段通过严格校验后才会替换缓存。OpenAI 价格表列出长上下文价格但没有给出阈值时，工具还会读取该模型的官方 Markdown 页面提取阈值；模型页不可用时才保留已核对的内置阈值。未知新模型仍无法确认阈值时会要求显式选择 `--price-tier long` 并给出警告，避免自行猜测。

默认 `--price-tier auto`。对已知阈值的模型会自动选择长上下文价格；如果官方列出长/短两档但目录无法确认阈值，工具会按 standard 计算并警告，可显式指定：

```bash
--price-tier long
```

当前已核对的规则如下：

- GPT-5.4、GPT-5.5 和 GPT-5.6 Sol/Terra/Luna 的长上下文高价档都按单次请求的输入 token 判断：`input_tokens > 272_000` 时，整次请求使用长上下文价格；恰好 272,000 仍按 standard。GPT-5.6 的长档相当于输入、缓存读取和缓存写入 2×，输出 1.5×。
- Anthropic 当前文档明确：Claude 4.6 及以后支持 1M 上下文的模型，全窗口使用标准价，无长上下文溢价；Claude Sonnet 4.5、Sonnet 4 等其余模型只有 200K 上下文，不能把超过 200K 当作可计费的长档请求。
- Anthropic `inference_geo: "us"` 对 Claude 4.6 及以后所有 token 类别使用 1.1×；Fast mode 目前仅 Opus 5/4.8，输入/输出为 10/50 USD/MTok；Batch 输入和输出为标准价 50%，缓存倍率与地域倍率可叠加。
- Anthropic Priority Tier 是既有容量承诺，不是另一套公开的按 token 单价；`service_tier: "auto"` 可能使用 Priority 容量，也可能回退 Standard。

Fast mode、regional/inference geo、Batch 或 Priority 等非标准价格不会被自动猜测。可用 `--input-rate`、`--output-rate` 和缓存价格参数手工覆盖；手工定价时输入、输出价格必须成对提供。

## 离线分析和示例 JSON 的来源

`examples/openai-response.json` 与 `examples/anthropic-message.json` 不是从某个真实中转站下载的。它们是按照两家官方响应结构手工构造的测试夹具，数字特意设置得较大，便于人工核对分桶和倍率；模型名也是 `example-*` 占位符。

真实 JSON 有两种获得方式：

1. 使用 `probe --save-response response.json`，工具会保存本次原始响应。
2. 在你自己的客户端中记录 API 返回体。不要把 API Key 或包含敏感提示词的响应提交到公开仓库。

离线计算官方成本，无需提供扣费：

```bash
rate-lens analyze examples/openai-response.json \
  --protocol openai \
  --input-rate 2 \
  --cache-read-rate 0.2 \
  --output-rate 8
```

该 fixture 的官方理论成本为 `2.44 USD`。加入扣费后计算倍率：

```bash
rate-lens analyze examples/openai-response.json \
  --protocol openai \
  --input-rate 2 \
  --cache-read-rate 0.2 \
  --output-rate 8 \
  --charged 3.66
```

结果为 `1.5×`。

已有真实响应且模型能被目录匹配时，只需指定对照模型：

```bash
rate-lens analyze response.json --official-model gpt-5.4
```

也支持 JSON 数组、JSONL、OpenAI `response.completed` SSE 和 Anthropic Messages SSE。相同 OpenAI `response.id` 会去重；Anthropic 累计 usage 不会重复相加。

## 人民币、额度和扣费来源

标准 OpenAI/Anthropic 响应没有中转站本次实际扣费字段。`--charged` 必须来自中转站账单、请求日志，或请求前后的余额差。若扣费单位为 CNY，并假设 `1 USD = 7.2 CNY`：

```bash
--charged 0.72 \
--exchange-rate 7.2 \
--actual-currency CNY
```

使用站内额度时可把 `--actual-currency` 写成 `QUOTA`，并把每 USD 对应的额度作为汇率。

交互模式会先选择中转站后台显示的余额/账单**原始单位**：`USD/CNY/HKD/TWD/EUR/JPY/GBP/SGD`、自定义 ISO 4217 币种或站内额度，再填写 `1 USD` 对应的扣费单位数量。货币汇率参考来自 [Frankfurter](https://frankfurter.dev/) 的最近可用报价，界面会显示报价日期；它只是市场参考值，中转站结算汇率可能不同，最终值可以手工覆盖。查询失败时向导会自动退回手工输入。

如果观测倍率低于 `0.1×` 或高于 `10×`，向导会提示再次核对账单原始单位、汇率，以及免费额度、返利、退款和并发请求。该提示不是对中转站价格真伪的自动裁决。

要得到可信倍率，建议确保余额差对应且只对应本次请求；排除并发请求、返利、免费额度、失败退款、固定手续费和服务端工具调用。结果是“观测有效倍率”，不一定等于中转站后台某一个单独配置值。

## 计费口径

OpenAI：

```text
普通输入 = input_tokens - cached_tokens - cache_write_tokens
官方成本 = 普通输入 + 缓存读取 + 缓存写入 + 输出 + 额外费用
```

Anthropic：

```text
官方成本 = input_tokens
         + cache_read_input_tokens
         + cache_creation_input_tokens（5m/1h 可分别定价）
         + output_tokens
         + 额外费用
```

reasoning/thinking token 已包含在 `output_tokens` 中，只展示、不重复计费。Web Search、代码执行等非 token 项目需要通过 `--extra-official-cost` 补录。

## 安全与自动化

- 优先使用环境变量或交互隐藏输入，不建议把 API Key 放进命令行。
- `probe` 非交互运行必须加 `--yes`，防止脚本误发高成本请求。
- 目标上下文上限为 1,000,000 token。
- HTTP 错误正文会截断，工具不会打印 API Key。
- `--json` 输出机器可读结果；`--save-response` 只保存服务端响应。

## 开发验证

```bash
env -u RUSTC_WRAPPER cargo fmt -- --check
env -u RUSTC_WRAPPER cargo test
env -u RUSTC_WRAPPER cargo clippy --all-targets -- -D warnings
git diff --check
```

端到端测试使用本机临时 mock 服务，不会访问真实 API 或消耗额度。
