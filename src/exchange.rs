use reqwest::blocking::Client;
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;
use std::time::Duration;
use thiserror::Error;

pub const EXCHANGE_RATE_SOURCE: &str = "https://api.frankfurter.app";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeRateQuote {
    pub base: String,
    pub quote: String,
    pub rate: Decimal,
    pub date: Option<String>,
    pub source: &'static str,
}

#[derive(Debug, Error)]
pub enum ExchangeRateError {
    #[error("币种代码 `{0}` 无效；请输入 ISO 4217 三位代码，例如 USD、CNY、EUR")]
    InvalidCurrency(String),
    #[error("无法构建汇率查询客户端：{0}")]
    Client(String),
    #[error("请求参考汇率失败：{0}")]
    Request(#[from] reqwest::Error),
    #[error("参考汇率服务返回 HTTP {status}：{body}")]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("参考汇率响应中没有 USD/{currency} 汇率")]
    MissingRate { currency: String },
    #[error("参考汇率响应不是有效 JSON：{0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("参考汇率 `{0}` 不是有效十进制数")]
    InvalidRate(String),
}

pub fn normalize_currency(value: &str) -> Result<String, ExchangeRateError> {
    let currency = value.trim().to_ascii_uppercase();
    if currency.len() == 3
        && currency
            .chars()
            .all(|character| character.is_ascii_alphabetic())
    {
        Ok(currency)
    } else {
        Err(ExchangeRateError::InvalidCurrency(value.to_owned()))
    }
}

pub fn fetch_usd_exchange_rate(
    currency: &str,
    timeout_seconds: u64,
) -> Result<ExchangeRateQuote, ExchangeRateError> {
    let currency = normalize_currency(currency)?;
    if currency == "USD" {
        return Ok(ExchangeRateQuote {
            base: "USD".to_owned(),
            quote: currency,
            rate: Decimal::ONE,
            date: None,
            source: EXCHANGE_RATE_SOURCE,
        });
    }

    let url = format!("{EXCHANGE_RATE_SOURCE}/latest?from=USD&to={currency}");
    let client = Client::builder()
        .timeout(Duration::from_secs(timeout_seconds.max(1)))
        .user_agent(concat!("rate-lens/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| ExchangeRateError::Client(error.to_string()))?;
    let response = client.get(&url).send()?;
    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        return Err(ExchangeRateError::Http {
            status,
            body: truncate(&body, 1_024),
        });
    }
    parse_quote(&body, &currency)
}

fn parse_quote(body: &str, currency: &str) -> Result<ExchangeRateQuote, ExchangeRateError> {
    let value: Value = serde_json::from_str(body)?;
    let raw_rate = value
        .get("rates")
        .and_then(|rates| rates.get(currency))
        .ok_or_else(|| ExchangeRateError::MissingRate {
            currency: currency.to_owned(),
        })?;
    let rate_text = match raw_rate {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return Err(ExchangeRateError::InvalidRate(raw_rate.to_string())),
    };
    let rate = Decimal::from_str(&rate_text)
        .map_err(|_| ExchangeRateError::InvalidRate(rate_text.clone()))?;
    if rate <= Decimal::ZERO {
        return Err(ExchangeRateError::InvalidRate(rate_text));
    }
    Ok(ExchangeRateQuote {
        base: "USD".to_owned(),
        quote: currency.to_owned(),
        rate,
        date: value.get("date").and_then(Value::as_str).map(str::to_owned),
        source: EXCHANGE_RATE_SOURCE,
    })
}

fn truncate(value: &str, max_bytes: usize) -> String {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    if end < value.len() {
        format!("{}…", &value[..end])
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_iso_currency_codes() {
        assert_eq!(normalize_currency(" cny ").unwrap(), "CNY");
        assert!(normalize_currency("人民币").is_err());
        assert!(normalize_currency("USDT").is_err());
    }

    #[test]
    fn usd_quote_does_not_need_network() {
        let quote = fetch_usd_exchange_rate("usd", 1).unwrap();
        assert_eq!(quote.rate, Decimal::ONE);
        assert_eq!(quote.quote, "USD");
    }

    #[test]
    fn parses_decimal_quote_and_date() {
        let quote = parse_quote(
            r#"{"amount":1.0,"base":"USD","date":"2026-08-28","rates":{"CNY":6.7209}}"#,
            "CNY",
        )
        .unwrap();
        assert_eq!(quote.rate, Decimal::from_str("6.7209").unwrap());
        assert_eq!(quote.date.as_deref(), Some("2026-08-28"));
    }
}
