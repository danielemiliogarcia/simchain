//! Exact Bitcoin amount parsing shared by bootstrap configuration and clients.

const SATOSHIS_PER_BTC: u64 = 100_000_000;

/// Parse a non-negative decimal BTC amount into satoshis without using
/// floating-point arithmetic.
pub fn parse_btc_sats(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("amount must not be empty".to_string());
    }
    if value.starts_with(['+', '-']) {
        return Err("amount must be a non-negative decimal".to_string());
    }
    if value.contains(['e', 'E']) {
        return Err("amount must not use exponent notation".to_string());
    }

    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fractional = parts.next();
    if parts.next().is_some() || whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err("amount must be a decimal number".to_string());
    }
    let fraction = fractional.unwrap_or("");
    if fraction.len() > 8 {
        return Err("amount must have at most eight decimal places".to_string());
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err("amount must be a decimal number".to_string());
    }

    let whole = whole
        .parse::<u64>()
        .map_err(|_| "amount is out of range".to_string())?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        let digits = fraction
            .parse::<u64>()
            .map_err(|_| "amount is out of range".to_string())?;
        digits
            .checked_mul(10_u64.pow((8 - fraction.len()) as u32))
            .ok_or_else(|| "amount is out of range".to_string())?
    };
    whole
        .checked_mul(SATOSHIS_PER_BTC)
        .and_then(|sats| sats.checked_add(fractional))
        .ok_or_else(|| "amount is out of range".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_btc_decimals() {
        assert_eq!(parse_btc_sats("0").unwrap(), 0);
        assert_eq!(
            parse_btc_sats("1btc"),
            Err("amount must be a decimal number".into())
        );
        assert_eq!(parse_btc_sats("1.00000001").unwrap(), 100_000_001);
        assert_eq!(parse_btc_sats("600.5").unwrap(), 60_050_000_000);
        assert!(parse_btc_sats("1.000000001").is_err());
        assert!(parse_btc_sats("-1").is_err());
        assert!(parse_btc_sats("NaN").is_err());
        assert!(parse_btc_sats("1e-8").is_err());
    }
}
