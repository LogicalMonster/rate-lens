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
- 内置官方模型价格，也允许手工覆盖价格
- 保留 JSON、JSONL 和 SSE 的离线分析能力

## 最快使用方式

安装后直接运行，不带参数会进入交互向导：

```bash
cargo install --path .
rate-lens
```

向导会依次询问协议、`base_url`、API Key、模型、官方对照模型、上下文长度、推理深度和扣费币种。API Key 使用隐藏输入，不会写入命令历史。选择扣费币种后，向导会查询带日期的 USD 市场参考汇率并允许覆盖；发送真实请求前还会展示预估输入成本并要求确认。

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

交互向导会在请求结束后询问本次实际扣费，因此能在同一次流程中直接显示倍率。非交互模式通常要等请求结束才能查看账单，建议先保存响应：

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

## 官方价格和长上下文档

查看内置目录：

```bash
rate-lens catalog
rate-lens catalog --protocol openai --json
```

目录中的每条计算结果都会显示价格来源和快照日期。价格会变化，重要结论应再次查阅：

- [OpenAI API Pricing](https://developers.openai.com/api/docs/pricing)
- [Anthropic API Pricing](https://platform.claude.com/docs/en/about-claude/pricing)
- [Anthropic Context Windows](https://platform.claude.com/docs/en/build-with-claude/context-windows)
- [Anthropic Service Tiers](https://platform.claude.com/docs/en/api/service-tiers)

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

交互模式会先选择 `USD/CNY/HKD/TWD/EUR/JPY/GBP/SGD`、自定义 ISO 4217 币种或站内额度，再填写 `1 USD` 对应的扣费币种数量。货币汇率参考来自 [Frankfurter](https://frankfurter.dev/) 的最近可用报价，界面会显示报价日期；它只是市场参考值，中转站结算汇率可能不同，最终值可以手工覆盖。查询失败时向导会自动退回手工输入。

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
