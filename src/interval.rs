use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Interval {
    amount: u64,
    unit: char,
}

impl Interval {
    pub const fn new(amount: u64, unit: char) -> Self {
        Self { amount, unit }
    }

    pub fn duration(&self) -> Duration {
        let unit_secs = match self.unit {
            's' => 1,
            'm' => 60,
            'h' => 60 * 60,
            _ => 24 * 60 * 60,
        };
        Duration::from_secs(self.amount.saturating_mul(unit_secs))
    }
}

pub fn parse(s: &str) -> Result<Interval, String> {
    let s = s.trim();
    let invalid = || {
        format!("invalid interval '{s}': expected a number followed by s, m, h or d (e.g. 30s, 5m)")
    };
    let unit = s.chars().last().ok_or_else(invalid)?;
    if !"smhd".contains(unit) {
        return Err(invalid());
    }
    let digits = &s[..s.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let amount: u64 = digits.parse().map_err(|_| invalid())?;
    if amount == 0 {
        return Err(format!("invalid interval '{s}': must be greater than zero"));
    }
    Ok(Interval { amount, unit })
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.amount, self.unit)
    }
}

impl TryFrom<String> for Interval {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        parse(&s)
    }
}

impl From<Interval> for String {
    fn from(i: Interval) -> Self {
        i.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_unit() {
        assert_eq!(parse("30s").unwrap().duration(), Duration::from_secs(30));
        assert_eq!(parse("5m").unwrap().duration(), Duration::from_secs(300));
        assert_eq!(parse("2h").unwrap().duration(), Duration::from_secs(7200));
        assert_eq!(parse("1d").unwrap().duration(), Duration::from_secs(86400));
    }

    #[test]
    fn round_trips_through_display() {
        assert_eq!(parse("15m").unwrap().to_string(), "15m");
    }

    #[test]
    fn rejects_bad_input() {
        for s in ["", "5", "m", "5x", "-5m", "+5m", "0s", "1.5h", "5 m", "5mm"] {
            assert!(parse(s).is_err(), "{s:?} should be rejected");
        }
    }
}
