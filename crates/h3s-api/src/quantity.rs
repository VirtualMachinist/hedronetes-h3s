//! Kubernetes apimachinery-shaped resource quantities.
use std::fmt;

/// A validated resource quantity string (`resource.Quantity` wire form).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quantity {
    raw: String,
}

impl Quantity {
    /// Parse and validate apimachinery quantity syntax.
    pub fn parse(s: &str) -> Option<Self> {
        if !syntax_valid(s) {
            return None;
        }
        Some(Self { raw: s.into() })
    }

    /// CPU semantics: bare numbers are cores; `m`/`u`/`n` scale to millicores.
    pub fn as_milli_cpu(&self) -> Option<i64> {
        scaled(&self.raw, true)
    }

    /// Memory and storage semantics: bare numbers are bytes.
    pub fn as_bytes(&self) -> Option<i64> {
        scaled(&self.raw, false)
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn syntax_valid(s: &str) -> bool {
    if s.is_empty() || s.starts_with('-') {
        return false;
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (n, suffix) = s.split_at(split);
    if n.is_empty() {
        return false;
    }
    let (_, frac) = n.split_once('.').unwrap_or((n, ""));
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    matches!(
        suffix,
        "" | "m" | "u" | "n" | "Ki" | "Mi" | "Gi" | "Ti" | "k" | "K" | "M" | "G"
    )
}

/// Exact fixed-point conversion; round fractional units up like Kubernetes.
fn scaled(s: &str, cpu: bool) -> Option<i64> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (n, suffix) = s.split_at(split);
    let (whole, frac) = n.split_once('.').unwrap_or((n, ""));
    let scale = 10i128.pow(frac.len() as u32);
    let base = whole
        .parse::<i128>()
        .ok()
        .and_then(|x| x.checked_mul(scale))
        .and_then(|x| {
            x.checked_add(if frac.is_empty() {
                0
            } else {
                frac.parse::<i128>().ok()?
            })
        })?;
    let (mul, div) = match suffix {
        "" => (1, 1),
        "m" => (1, 1000),
        "u" => (1, 1_000_000),
        "n" => (1, 1_000_000_000),
        "Ki" => (1024, 1),
        "Mi" => (1024 * 1024, 1),
        "Gi" => (1024 * 1024 * 1024, 1),
        "Ti" => (1024i128.pow(4), 1),
        "k" | "K" => (1000, 1),
        "M" => (1_000_000, 1),
        "G" => (1_000_000_000, 1),
        _ => return None,
    };
    if cpu && matches!(suffix, "Ki" | "Mi" | "Gi" | "Ti" | "k" | "K" | "M" | "G") {
        return None;
    }
    if !cpu && matches!(suffix, "u" | "n") {
        return None;
    }
    let numerator = base
        .checked_mul(mul)
        .and_then(|x| x.checked_mul(if cpu { 1000 } else { 1 }))?;
    let denominator = scale * div;
    i64::try_from(numerator / denominator + i128::from(numerator % denominator != 0)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_invalid_syntax() {
        for s in ["", "-1", "not-a-number", "1.2.3", "1Wi"] {
            assert!(Quantity::parse(s).is_none(), "{s}");
        }
        assert!(Quantity::parse("100m").is_some());
        assert!(Quantity::parse("64Mi").is_some());
    }

    #[test]
    fn cpu_millicores_round_up_without_float_drift() {
        for (s, expected) in [
            ("100m", 100),
            ("0.1", 100),
            ("0.0001", 1),
            ("1", 1000),
            ("2500m", 2500),
        ] {
            let q = Quantity::parse(s).unwrap();
            assert_eq!(q.as_milli_cpu(), Some(expected), "{s}");
        }
        assert!(Quantity::parse("64Mi").unwrap().as_milli_cpu().is_none());
    }

    #[test]
    fn memory_bytes_round_up_without_float_drift() {
        for (s, expected) in [
            ("64Mi", 67108864),
            ("1.5Gi", 1610612736),
            ("1000m", 1),
            ("100m", 1),
            ("1", 1),
            ("1k", 1000),
        ] {
            let q = Quantity::parse(s).unwrap();
            assert_eq!(q.as_bytes(), Some(expected), "{s}");
        }
        assert!(Quantity::parse("64Mi").unwrap().as_milli_cpu().is_none());
    }

    #[test]
    fn overflow_returns_none() {
        let q = Quantity::parse("999999999999999999999999999999999999999Gi").unwrap();
        assert!(q.as_bytes().is_none());
        assert!(q.as_milli_cpu().is_none());
    }
}
