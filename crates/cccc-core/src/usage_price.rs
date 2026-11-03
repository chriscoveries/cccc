//! Operator-provided, time-bounded prices. No built-in model prices.
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::io;

const DENOMINATOR: u128 = 1_000_000_000_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub version: String,
    pub currency: String,
    pub minor_scale: u8,
    pub entries: Vec<PriceEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceEntry {
    pub model: String,
    pub effective_from: String,
    pub effective_until: Option<String>,
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write: String,
}

impl PriceTable {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > 256 * 1024 {
            return Err(invalid("price table exceeds 256 KiB"));
        }
        let table: Self = serde_json::from_slice(bytes).map_err(io::Error::other)?;
        if table.version.is_empty()
            || table.version.len() > 128
            || table.minor_scale > 6
            || table.currency.len() != 3
            || !table.currency.bytes().all(|b| b.is_ascii_uppercase())
            || table.entries.len() > 1024
        {
            return Err(invalid("invalid price table header"));
        }
        for (i, entry) in table.entries.iter().enumerate() {
            if entry.model.is_empty() || entry.model.len() > 256 {
                return Err(invalid("invalid model"));
            }
            let start = timestamp(&entry.effective_from)?;
            let end = entry
                .effective_until
                .as_deref()
                .map(timestamp)
                .transpose()?;
            if end.is_some_and(|end| end <= start) {
                return Err(invalid("empty price interval"));
            }
            entry.rates()?;
            for other in &table.entries[..i] {
                if entry.model == other.model {
                    let other_start = timestamp(&other.effective_from)?;
                    let other_end = other
                        .effective_until
                        .as_deref()
                        .map(timestamp)
                        .transpose()?;
                    if end.is_none_or(|end| other_start < end)
                        && other_end.is_none_or(|end| start < end)
                    {
                        return Err(invalid("overlapping price intervals"));
                    }
                }
            }
        }
        Ok(table)
    }

    /// A missing model, counter, start time, price, or checked arithmetic is unknown.
    pub fn numerator(
        &self,
        model: &str,
        started_at: &str,
        tokens: [Option<u64>; 4],
    ) -> Option<u128> {
        let started = timestamp(started_at).ok()?;
        let mut matches = self.entries.iter().filter(|entry| {
            entry.model == model
                && timestamp(&entry.effective_from).is_ok_and(|from| from <= started)
                && entry
                    .effective_until
                    .as_deref()
                    .is_none_or(|until| timestamp(until).is_ok_and(|until| started < until))
        });
        let entry = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let rates = entry.rates().ok()?;
        tokens
            .into_iter()
            .zip(rates)
            .try_fold(0_u128, |sum, (tokens, rate)| {
                sum.checked_add(u128::from(tokens?).checked_mul(rate)?)
            })
    }

    /// Final round-half-even in the operator's chosen currency minor units.
    pub fn minor_units(&self, numerator: u128) -> Option<u128> {
        self.mean_minor_units(numerator, 1)
    }

    /// Round only after dividing the exact numerator by the task count.
    pub fn mean_minor_units(&self, numerator: u128, count: u64) -> Option<u128> {
        if count == 0 {
            return None;
        }
        let scaled = numerator.checked_mul(10_u128.pow(u32::from(self.minor_scale)))?;
        let denominator = DENOMINATOR.checked_mul(u128::from(count))?;
        let quotient = scaled / denominator;
        let remainder = scaled % denominator;
        let above_half = remainder > denominator / 2;
        // The fixed scale is even, so every nonzero multiple is even.
        let tie_odd = remainder == denominator / 2 && quotient % 2 == 1;
        quotient.checked_add(if above_half || tie_odd { 1 } else { 0 })
    }
}

impl PriceEntry {
    fn rates(&self) -> io::Result<[u128; 4]> {
        Ok([
            decimal(&self.input)?,
            decimal(&self.output)?,
            decimal(&self.cache_read)?,
            decimal(&self.cache_write)?,
        ])
    }
}

/// A decimal currency rate per million tokens, in integer billionths.
fn decimal(text: &str) -> io::Result<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || whole.len() > 12
        || fraction.len() > 9
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (text.contains('.') && fraction.is_empty())
    {
        return Err(invalid("invalid decimal price"));
    }
    let integer = whole.parse::<u128>().map_err(io::Error::other)?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().map_err(io::Error::other)?
    };
    integer
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(fractional * 10_u128.pow((9 - fraction.len()) as u32)))
        .ok_or_else(|| invalid("price overflow"))
}

fn timestamp(text: &str) -> io::Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(text).map_err(io::Error::other)?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(invalid("timestamp must be UTC"));
    }
    Ok(parsed.with_timezone(&Utc))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn captured_turn() -> Value {
        let capture: Vec<Value> = serde_json::from_str(include_str!("usage_report_captured.json"))
            .expect("captured test data and setup");
        capture[0]["data"].clone()
    }

    // Illustrative operator-supplied rates, applied to counters from a real turn.
    fn table() -> Value {
        json!({"version":"example-1","currency":"USD","minor_scale":2,"entries":[{
            "model":"claude-opus-5-5","effective_from":"2026-01-01T00:00:00Z",
            "effective_until":"2027-01-01T00:00:00Z","input":"1.5","output":"2",
            "cache_read":"0.125","cache_write":"0"}]})
    }

    #[test]
    fn captured_counters_and_supplied_rates_use_exact_arithmetic() {
        let turn = captured_turn();
        let parsed =
            PriceTable::parse(&serde_json::to_vec(&table()).expect("captured test data and setup"))
                .expect("captured test data and setup");
        let counts = [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
        ]
        .map(|key| turn[key].as_u64());
        let amount = parsed
            .numerator(
                turn["model_id"]
                    .as_str()
                    .expect("captured test data and setup"),
                turn["started_at"]
                    .as_str()
                    .expect("captured test data and setup"),
                counts,
            )
            .expect("captured test data and setup");
        assert_eq!(amount, 171_674_875_000_000);
        assert_eq!(parsed.minor_units(amount), Some(17));
        assert!(
            parsed
                .numerator(
                    "unknown",
                    turn["started_at"]
                        .as_str()
                        .expect("captured test data and setup"),
                    counts
                )
                .is_none()
        );
        assert!(
            parsed
                .numerator(
                    turn["model_id"]
                        .as_str()
                        .expect("captured test data and setup"),
                    "2027-01-01T00:00:00Z",
                    counts
                )
                .is_none()
        );
        assert!(
            parsed
                .numerator(
                    turn["model_id"]
                        .as_str()
                        .expect("captured test data and setup"),
                    turn["started_at"]
                        .as_str()
                        .expect("captured test data and setup"),
                    [None, counts[1], counts[2], counts[3]]
                )
                .is_none()
        );
    }

    #[test]
    fn invalid_prices_overlap_and_half_even_are_explicit() {
        let mut input = table();
        input["entries"][0]["input"] = json!(1.0);
        assert!(
            PriceTable::parse(&serde_json::to_vec(&input).expect("captured test data and setup"))
                .is_err()
        );
        input = table();
        input["entries"][0]["input"] = json!("1.0000000001");
        assert!(
            PriceTable::parse(&serde_json::to_vec(&input).expect("captured test data and setup"))
                .is_err()
        );
        input = table();
        let duplicate = input["entries"][0].clone();
        input["entries"]
            .as_array_mut()
            .expect("captured test data and setup")
            .push(duplicate);
        assert!(
            PriceTable::parse(&serde_json::to_vec(&input).expect("captured test data and setup"))
                .is_err()
        );
        let parsed =
            PriceTable::parse(&serde_json::to_vec(&table()).expect("captured test data and setup"))
                .expect("captured test data and setup");
        assert_eq!(parsed.minor_units(5_000_000_000_000), Some(0));
        assert_eq!(parsed.minor_units(15_000_000_000_000), Some(2));
        assert_eq!(parsed.mean_minor_units(10_000_000_000_000, 2), Some(0));
        assert_eq!(parsed.mean_minor_units(30_000_000_000_000, 2), Some(2));
        assert_eq!(parsed.mean_minor_units(10_000_000_000_000, 0), None);
        assert!(parsed.minor_units(u128::MAX).is_none());
    }
    #[test]
    fn adjacent_intervals_and_overflow_use_captured_counters() {
        let turn = captured_turn();
        let counts = [
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
        ]
        .map(|key| turn[key].as_u64());
        let mut input = table();
        input["entries"][0]["effective_until"] = json!("2026-11-02T18:27:09.484407Z");
        let mut next = input["entries"][0].clone();
        next["effective_from"] = input["entries"][0]["effective_until"].clone();
        next["effective_until"] = json!("2027-01-01T00:00:00Z");
        next["input"] = json!("0");
        next["output"] = json!("0");
        next["cache_read"] = json!("0");
        next["cache_write"] = json!("0");
        input["entries"]
            .as_array_mut()
            .expect("captured test data and setup")
            .push(next);
        let parsed =
            PriceTable::parse(&serde_json::to_vec(&input).expect("captured test data and setup"))
                .expect("captured test data and setup");
        assert_eq!(
            parsed.numerator(
                turn["model_id"]
                    .as_str()
                    .expect("captured test data and setup"),
                turn["started_at"]
                    .as_str()
                    .expect("captured test data and setup"),
                counts
            ),
            Some(0)
        );
        let mut large = table();
        for kind in ["input", "output", "cache_read", "cache_write"] {
            large["entries"][0][kind] = json!("999999999999.999999999");
        }
        let parsed =
            PriceTable::parse(&serde_json::to_vec(&large).expect("captured test data and setup"))
                .expect("captured test data and setup");
        assert!(
            parsed
                .numerator(
                    turn["model_id"]
                        .as_str()
                        .expect("captured test data and setup"),
                    turn["started_at"]
                        .as_str()
                        .expect("captured test data and setup"),
                    [Some(u64::MAX); 4]
                )
                .is_none()
        );
    }
}
