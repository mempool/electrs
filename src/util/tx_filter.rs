use std::collections::HashMap;

use crate::errors::*;
use crate::util::BlockId;

pub const LOCKTIME_THRESHOLD: u64 = 500_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeBound {
    Height(u32),
    Time(u32),
}

impl TimeBound {
    fn parse(name: &str, raw: &str) -> Result<Self> {
        let value = raw.parse::<u64>().chain_err(|| {
            format!(
                "Invalid {}, expected a block height or unix timestamp",
                name
            )
        })?;

        if value > u64::from(u32::MAX) {
            bail!("Invalid {}, out of range", name);
        }

        Ok(if value < LOCKTIME_THRESHOLD {
            TimeBound::Height(value as u32)
        } else {
            TimeBound::Time(value as u32)
        })
    }

    fn is_height(&self) -> bool {
        matches!(self, TimeBound::Height(_))
    }

    fn value(&self) -> u64 {
        match self {
            TimeBound::Height(value) | TimeBound::Time(value) => u64::from(*value),
        }
    }

    fn block_value(&self, blockid: &BlockId) -> u64 {
        match self {
            TimeBound::Height(_) => blockid.height as u64,
            TimeBound::Time(_) => u64::from(blockid.time),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxHistoryFilter {
    direction: Option<TxDirection>,
    min_amount: Option<u64>,
    max_amount: Option<u64>,
    from: Option<TimeBound>,
    to: Option<TimeBound>,
}

impl TxHistoryFilter {
    pub fn from_query_params(query_params: &HashMap<String, String>) -> Result<Self> {
        let direction = match query_params.get("direction").map(String::as_str) {
            None => None,
            Some("incoming") => Some(TxDirection::Incoming),
            Some("outgoing") => Some(TxDirection::Outgoing),
            Some(_) => bail!("Invalid direction, expected 'incoming' or 'outgoing'"),
        };

        let min_amount = parse_amount("min_amount", query_params)?;
        let max_amount = parse_amount("max_amount", query_params)?;

        let from = query_params
            .get("from")
            .map(|raw| TimeBound::parse("from", raw))
            .transpose()?;
        let to = query_params
            .get("to")
            .map(|raw| TimeBound::parse("to", raw))
            .transpose()?;

        #[cfg(feature = "liquid")]
        if direction.is_some() || min_amount.is_some() || max_amount.is_some() {
            bail!("The direction and amount filters are not supported on Liquid");
        }

        let filter = TxHistoryFilter {
            direction,
            min_amount,
            max_amount,
            from,
            to,
        };
        filter.validate()?;
        Ok(filter)
    }

    fn validate(&self) -> Result<()> {
        if let (Some(min), Some(max)) = (self.min_amount, self.max_amount) {
            if min > max {
                bail!("Invalid amount range, min_amount is greater than max_amount");
            }
        }

        if let (Some(from), Some(to)) = (self.from, self.to) {
            if from.is_height() != to.is_height() {
                bail!("Invalid time range, from and to must both be block heights or both be unix timestamps");
            }
            if from.value() > to.value() {
                bail!("Invalid time range, from is greater than to");
            }
        }

        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        *self == TxHistoryFilter::default()
    }

    pub fn excludes_mempool(&self) -> bool {
        self.from.is_some() || self.to.is_some()
    }

    pub fn matches(&self, net: i64, blockid: Option<&BlockId>) -> bool {
        if let Some(direction) = self.direction {
            let matches_direction = match direction {
                TxDirection::Incoming => net > 0,
                TxDirection::Outgoing => net < 0,
            };
            if !matches_direction {
                return false;
            }
        }

        let amount = net.unsigned_abs();
        if self.min_amount.is_some_and(|min| amount < min) {
            return false;
        }
        if self.max_amount.is_some_and(|max| amount > max) {
            return false;
        }

        if self.from.is_some() || self.to.is_some() {
            let blockid = match blockid {
                Some(blockid) => blockid,
                None => return false,
            };
            if let Some(from) = self.from {
                if from.block_value(blockid) < from.value() {
                    return false;
                }
            }
            if let Some(to) = self.to {
                if to.block_value(blockid) > to.value() {
                    return false;
                }
            }
        }

        true
    }

    pub fn height_range<I>(&self, mut block_times: I) -> Option<(Option<u32>, Option<u32>)>
    where
        I: DoubleEndedIterator<Item = u32> + ExactSizeIterator + Clone,
    {
        let lowest = match self.from {
            None => None,
            Some(TimeBound::Height(height)) => Some(height),
            Some(TimeBound::Time(time)) => {
                Some(block_times.clone().position(|t| t >= time)? as u32)
            }
        };
        let highest = match self.to {
            None => None,
            Some(TimeBound::Height(height)) => Some(height),
            Some(TimeBound::Time(time)) => Some(block_times.rposition(|t| t <= time)? as u32),
        };

        if let (Some(lowest), Some(highest)) = (lowest, highest) {
            if lowest > highest {
                return None;
            }
        }
        Some((lowest, highest))
    }
}

fn parse_amount(name: &str, query_params: &HashMap<String, String>) -> Result<Option<u64>> {
    query_params
        .get(name)
        .map(|raw| {
            raw.parse::<u64>()
                .chain_err(|| format!("Invalid {}, expected an amount in satoshis", name))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::BlockHash;
    use bitcoin::hashes::Hash;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn parse(pairs: &[(&str, &str)]) -> Result<TxHistoryFilter> {
        TxHistoryFilter::from_query_params(&params(pairs))
    }

    fn block(height: usize, time: u32) -> BlockId {
        BlockId {
            height,
            time,
            hash: BlockHash::all_zeros(),
        }
    }

    #[test]
    fn height_range_passes_height_bounds_through() {
        let times = [0u32; 10];
        let range =
            |pairs: &[(&str, &str)]| parse(pairs).unwrap().height_range(times.iter().copied());

        assert_eq!(range(&[]), Some((None, None)));
        assert_eq!(range(&[("from", "3")]), Some((Some(3), None)));
        assert_eq!(range(&[("to", "7")]), Some((None, Some(7))));
        assert_eq!(
            range(&[("from", "3"), ("to", "7")]),
            Some((Some(3), Some(7)))
        );
    }

    #[test]
    fn height_range_maps_timestamps_without_excluding_out_of_order_blocks() {
        const T: u32 = 1_600_000_000;
        let times = [T + 100, T + 200, T + 150, T + 300, T + 250, T + 400];
        let range =
            |pairs: &[(&str, &str)]| parse(pairs).unwrap().height_range(times.iter().copied());

        assert_eq!(
            range(&[("from", "1600000180"), ("to", "1600000260")]),
            Some((Some(1), Some(4)))
        );
        assert_eq!(range(&[("from", "1600000000")]), Some((Some(0), None)));
        assert_eq!(range(&[("to", "1600000400")]), Some((None, Some(5))));
    }

    #[test]
    fn height_range_is_none_when_no_block_can_match() {
        const T: u32 = 1_600_000_000;
        let times = [T + 100, T + 300];
        let range =
            |pairs: &[(&str, &str)]| parse(pairs).unwrap().height_range(times.iter().copied());

        assert_eq!(range(&[("from", "1600000301")]), None);
        assert_eq!(range(&[("to", "1600000099")]), None);
        assert_eq!(range(&[("from", "1600000200"), ("to", "1600000200")]), None);
    }

    #[test]
    fn absent_params_yield_an_empty_filter() {
        let filter = parse(&[("max_txs", "10"), ("after_txid", "ff")]).unwrap();
        assert!(filter.is_empty());
        assert!(!filter.excludes_mempool());
        assert!(filter.matches(0, None));
        assert!(filter.matches(i64::MIN, Some(&block(1, 1))));
    }

    #[test]
    fn time_bounds_are_not_empty_and_exclude_the_mempool() {
        assert!(!parse(&[("from", "800000")]).unwrap().is_empty());
        assert!(parse(&[("from", "800000")]).unwrap().excludes_mempool());
        assert!(parse(&[("to", "800000")]).unwrap().excludes_mempool());
        #[cfg(not(feature = "liquid"))]
        assert!(!parse(&[("min_amount", "1")]).unwrap().excludes_mempool());
    }

    #[test]
    fn locktime_threshold_splits_heights_from_timestamps() {
        assert_eq!(
            TimeBound::parse("from", "499999999").unwrap(),
            TimeBound::Height(499_999_999)
        );
        assert_eq!(
            TimeBound::parse("from", "500000000").unwrap(),
            TimeBound::Time(500_000_000)
        );
        assert_eq!(TimeBound::parse("from", "0").unwrap(), TimeBound::Height(0));
        assert_eq!(
            TimeBound::parse("to", "4294967295").unwrap(),
            TimeBound::Time(u32::MAX)
        );
    }

    #[test]
    fn rejects_malformed_values() {
        for pairs in [
            vec![("direction", "sideways")],
            vec![("direction", "")],
            vec![("direction", "Incoming")],
            vec![("min_amount", "abc")],
            vec![("min_amount", "-1")],
            vec![("min_amount", "1.5")],
            vec![("max_amount", "")],
            vec![("from", "abc")],
            vec![("to", "-1")],
            vec![("from", "4294967296")],
            vec![("min_amount", "5"), ("max_amount", "1")],
            vec![("from", "900"), ("to", "100")],
            vec![("from", "1700000001"), ("to", "1700000000")],
            vec![("from", "100"), ("to", "1700000000")],
            vec![("from", "1700000000"), ("to", "100")],
        ] {
            assert!(
                parse(&pairs).is_err(),
                "expected {:?} to be rejected",
                pairs
            );
        }
    }

    #[test]
    fn accepts_equal_range_bounds() {
        #[cfg(not(feature = "liquid"))]
        assert!(parse(&[("min_amount", "5"), ("max_amount", "5")]).is_ok());
        assert!(parse(&[("from", "800000"), ("to", "800000")]).is_ok());
        assert!(parse(&[("from", "1700000000"), ("to", "1700000000")]).is_ok());
    }

    #[cfg(not(feature = "liquid"))]
    #[test]
    fn direction_follows_the_sign_of_the_net_value() {
        let incoming = parse(&[("direction", "incoming")]).unwrap();
        let outgoing = parse(&[("direction", "outgoing")]).unwrap();

        assert!(incoming.matches(1, None));
        assert!(!incoming.matches(0, None));
        assert!(!incoming.matches(-1, None));

        assert!(outgoing.matches(-1, None));
        assert!(!outgoing.matches(0, None));
        assert!(!outgoing.matches(1, None));
    }

    #[cfg(not(feature = "liquid"))]
    #[test]
    fn amount_bounds_are_inclusive_and_compare_the_absolute_net() {
        let filter = parse(&[("min_amount", "100"), ("max_amount", "200")]).unwrap();

        assert!(filter.matches(100, None));
        assert!(filter.matches(200, None));
        assert!(filter.matches(-150, None));
        assert!(!filter.matches(99, None));
        assert!(!filter.matches(201, None));
        assert!(!filter.matches(-201, None));
        assert!(!filter.matches(i64::MIN, None));
    }

    #[cfg(not(feature = "liquid"))]
    #[test]
    fn direction_and_amount_compose() {
        let filter = parse(&[("direction", "incoming"), ("min_amount", "10000000")]).unwrap();

        assert!(filter.matches(10_000_000, None));
        assert!(filter.matches(50_000_000, None));
        assert!(!filter.matches(9_999_999, None));
        assert!(!filter.matches(-50_000_000, None));
    }

    #[test]
    fn height_bounds_are_inclusive() {
        let filter = parse(&[("from", "800000"), ("to", "800010")]).unwrap();

        assert!(filter.matches(0, Some(&block(800_000, 0))));
        assert!(filter.matches(0, Some(&block(800_010, 0))));
        assert!(filter.matches(0, Some(&block(800_005, 0))));
        assert!(!filter.matches(0, Some(&block(799_999, 0))));
        assert!(!filter.matches(0, Some(&block(800_011, 0))));
    }

    #[test]
    fn time_bounds_are_inclusive_and_ignore_height() {
        let filter = parse(&[("from", "1704067200"), ("to", "1735689600")]).unwrap();

        assert!(filter.matches(0, Some(&block(1, 1_704_067_200))));
        assert!(filter.matches(0, Some(&block(999_999, 1_735_689_600))));
        assert!(!filter.matches(0, Some(&block(800_000, 1_704_067_199))));
        assert!(!filter.matches(0, Some(&block(800_000, 1_735_689_601))));
    }

    #[test]
    fn open_ended_time_bounds_work() {
        let from = parse(&[("from", "800000")]).unwrap();
        assert!(from.matches(0, Some(&block(800_000, 0))));
        assert!(!from.matches(0, Some(&block(799_999, 0))));

        let to = parse(&[("to", "800000")]).unwrap();
        assert!(to.matches(0, Some(&block(799_999, 0))));
        assert!(!to.matches(0, Some(&block(800_001, 0))));
    }

    #[test]
    fn unconfirmed_never_matches_a_time_bound() {
        assert!(!parse(&[("from", "0")]).unwrap().matches(0, None));
        assert!(!parse(&[("to", "4294967295")]).unwrap().matches(0, None));
    }

    #[cfg(feature = "liquid")]
    #[test]
    fn amount_and_direction_are_rejected_on_liquid() {
        assert!(parse(&[("direction", "incoming")]).is_err());
        assert!(parse(&[("min_amount", "1")]).is_err());
        assert!(parse(&[("max_amount", "1")]).is_err());
        assert!(parse(&[("from", "800000"), ("to", "800010")]).is_ok());
    }
}
