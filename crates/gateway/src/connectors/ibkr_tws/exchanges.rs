//! IBKR exchange codes (the `exchange` / `primaryExchange` of a contract)
//! mapped to ISO 10383 MICs.

const TABLE: &[(&str, &str)] = &[
    ("NASDAQ", "XNAS"),
    ("NYSE", "XNYS"),
    ("ARCA", "ARCX"),
    ("AMEX", "XASE"),
    ("BATS", "BATS"),
    ("IBIS", "XETR"),
    ("FWB", "XFRA"),
    ("LSE", "XLON"),
    ("SBF", "XPAR"),
    ("AEB", "XAMS"),
    ("EBS", "XSWX"),
    ("BVME", "XMIL"),
    ("BM", "XMAD"),
    ("SFB", "XSTO"),
    ("TSE", "XTSE"),
    ("SEHK", "XHKG"),
    ("ASX", "XASX"),
    ("TSEJ", "XTKS"),
];

pub fn to_mic(ibkr: &str) -> Option<&'static str> {
    TABLE.iter().find(|(i, _)| *i == ibkr).map(|(_, m)| *m)
}

pub fn from_mic(mic: &str) -> Option<&'static str> {
    TABLE.iter().find(|(_, m)| *m == mic).map(|(i, _)| *i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ibis_to_xetr() {
        assert_eq!(to_mic("IBIS"), Some("XETR"));
    }

    #[test]
    fn round_trips_every_entry() {
        for (ibkr, _) in TABLE {
            let mic = to_mic(ibkr).unwrap();
            assert_eq!(from_mic(mic), Some(*ibkr));
        }
        let mut mics: Vec<&str> = TABLE.iter().map(|(_, m)| *m).collect();
        mics.sort_unstable();
        mics.dedup();
        assert_eq!(mics.len(), TABLE.len(), "each MIC must appear once");
    }

    #[test]
    fn toronto_and_tokyo_map_correctly() {
        assert_eq!(to_mic("TSE"), Some("XTSE"));
        assert_eq!(to_mic("TSEJ"), Some("XTKS"));
        assert_eq!(to_mic("TSX"), None);
    }

    #[test]
    fn unknown_exchange_is_none() {
        assert_eq!(to_mic("FOO"), None);
    }
}
