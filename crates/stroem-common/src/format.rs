//! Human-readable formatting helpers shared across crates (server + CLI).

/// `262144` → `256.0 KiB`.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_uses_iec_units_with_one_decimal() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(262_144), "256.0 KiB");
        assert_eq!(human_bytes(87_325_871), "83.3 MiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
