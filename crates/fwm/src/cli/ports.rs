//! Bounded parsing of comma-separated ports and inclusive port ranges.
use anyhow::{Result, bail};
use std::collections::BTreeSet;

pub const MAX_PORTS: usize = 512;

pub fn single(value: &str) -> Result<u16> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("expected one port between 1 and 65535, got {value:?}");
    }
    match value.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => bail!("port must be between 1 and 65535, got {value:?}"),
    }
}

pub fn expand(value: &str) -> Result<Vec<u16>> {
    let mut ports = BTreeSet::new();
    for item in value.split(',') {
        let item = item.trim();
        let (first, last) = if let Some((first, last)) = item.split_once('-') {
            (single(first)?, single(last)?)
        } else {
            let port = single(item)?;
            (port, port)
        };
        if first > last {
            bail!("port range must be ascending, got {item:?}");
        }
        for port in first..=last {
            ports.insert(port);
            if ports.len() > MAX_PORTS {
                bail!("at most {MAX_PORTS} distinct ports can be added at once");
            }
        }
    }
    Ok(ports.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_inclusive_ranges_and_deduplicates_in_sorted_order() {
        assert_eq!(
            expand("8080,3000-3003,3001,3003-3004").unwrap(),
            [3000, 3001, 3002, 3003, 3004, 8080]
        );
        assert_eq!(expand("65534-65535,1,001").unwrap(), [1, 65534, 65535]);
        assert_eq!(expand(" 42 - 42, 42 ").unwrap(), [42]);
    }

    #[test]
    fn rejects_malformed_and_out_of_range_ports() {
        for value in [
            "",
            "0",
            "65536",
            "-1",
            "+1",
            "3002-3000",
            "1-2-3",
            "1,",
            ",1",
            "1,,2",
            "-2",
            "2-",
            "one",
            "1:2",
            "1 2",
        ] {
            assert!(expand(value).is_err(), "{value:?}");
        }
        assert!(single("80,81").is_err());
        assert!(single("80-81").is_err());
    }

    #[test]
    fn caps_distinct_ports_not_duplicate_mentions() {
        assert_eq!(expand("1-512,1-512").unwrap().len(), MAX_PORTS);
        assert!(expand("1-513").is_err());
        assert!(expand("1-65535").is_err());
    }
}
