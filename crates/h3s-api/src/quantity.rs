/// Exact fixed-point quantity conversion; round fractional units up like Kubernetes.
pub fn quantity(s: &str, cpu: bool) -> Option<i64> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (n, suffix) = s.split_at(split);
    let (whole, frac) = n.split_once('.').unwrap_or((n, ""));
    if whole.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
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
    let numerator = base
        .checked_mul(mul)
        .and_then(|x| x.checked_mul(if cpu { 1000 } else { 1 }))?;
    let denominator = scale * div;
    i64::try_from(numerator / denominator + i128::from(numerator % denominator != 0)).ok()
}
