#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeDecision {
    Full,
    Partial { start: u64, end_inclusive: u64 },
    Unsatisfiable { len: u64 },
}

/// Parses the MVP's single HTTP byte range.
///
/// Malformed ranges, unsupported units, and multi-range requests are ignored
/// as required by Cellar's download contract. Syntactically valid ranges that
/// cannot select any byte are reported separately for a `416` response.
#[must_use]
pub fn decide_range(header: Option<&str>, len: u64) -> RangeDecision {
    let Some(header) = header else {
        return RangeDecision::Full;
    };
    let Some((unit, value)) = header.split_once('=') else {
        return RangeDecision::Full;
    };
    if !unit.eq_ignore_ascii_case("bytes") || value.is_empty() || value.contains(',') {
        return RangeDecision::Full;
    }
    let Some((first, last)) = value.split_once('-') else {
        return RangeDecision::Full;
    };
    if first.is_empty() {
        let Some(suffix) = parse_decimal(last) else {
            return RangeDecision::Full;
        };
        if suffix == 0 || len == 0 {
            return RangeDecision::Unsatisfiable { len };
        }
        let selected = suffix.min(len);
        return RangeDecision::Partial {
            start: len - selected,
            end_inclusive: len - 1,
        };
    }
    let Some(start) = parse_decimal(first) else {
        return RangeDecision::Full;
    };
    if last.is_empty() {
        return if start >= len {
            RangeDecision::Unsatisfiable { len }
        } else {
            RangeDecision::Partial {
                start,
                end_inclusive: len - 1,
            }
        };
    }
    let Some(requested_end) = parse_decimal(last) else {
        return RangeDecision::Full;
    };
    if start > requested_end || start >= len {
        return RangeDecision::Unsatisfiable { len };
    }
    RangeDecision::Partial {
        start,
        end_inclusive: requested_end.min(len - 1),
    }
}

fn parse_decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.parse().unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_distinguishes_full_partial_and_unsatisfiable() {
        assert_eq!(decide_range(None, 100), RangeDecision::Full);
        assert_eq!(decide_range(Some("garbage"), 100), RangeDecision::Full);
        assert_eq!(
            decide_range(Some("bytes=0-1,4-5"), 100),
            RangeDecision::Full
        );
        assert_eq!(
            decide_range(Some("bytes=-10"), 100),
            RangeDecision::Partial {
                start: 90,
                end_inclusive: 99
            }
        );
        assert_eq!(
            decide_range(Some("bytes=100-"), 100),
            RangeDecision::Unsatisfiable { len: 100 }
        );
    }
}
