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
    ("TSE", "XTKS"),
    ("SEHK", "XHKG"),
    ("ASX", "XASX"),
    ("TSX", "XTSE"),
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
    }

    #[test]
    fn unknown_exchange_is_none() {
        assert_eq!(to_mic("FOO"), None);
    }
}
