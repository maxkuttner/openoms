//! Map an ISO 10383 MIC (our master `instrument.venue`) to an OpenFIGI `exchCode`.
//!
//! OpenFIGI's mapping API keys on its own two-letter exchange codes, not MICs, and its
//! `micCode` field is unreliable for equities — so callers translate the MIC first.
//! US venues all resolve to the `"US"` composite (one FIGI per name across US listings).

/// OpenFIGI `exchCode` for a MIC, or `None` if unknown (caller falls back to no exchange).
pub fn openfigi_exch_code(mic: &str) -> Option<&'static str> {
    // US equity venues → the "US" composite.
    const US: &[&str] = &[
        "XNAS", "XNYS", "ARCX", "XASE", "BATS", "BATY", "EDGA", "EDGX", "IEXG", "XBOS",
        "XPSX", "XCIS", "XCHI", "EPRL", "MEMX", "LTSE", "XADF", "OTCM", "PSGM", "XNCM",
        "XNGS", "XNMS",
    ];
    if US.contains(&mic) {
        return Some("US");
    }
    // Selected international venues (extend as needed).
    let code = match mic {
        "XLON" => "LN", // London
        "XETR" | "XFRA" => "GY", // Deutsche Börse / Frankfurt
        "XPAR" => "FP", // Euronext Paris
        "XAMS" => "NA", // Euronext Amsterdam
        "XSWX" | "XVTX" => "SW", // SIX Swiss
        "XTKS" => "JT", // Tokyo
        "XHKG" => "HK", // Hong Kong
        "XTSE" => "CN", // Toronto
        "XASX" => "AT", // Australia
        "XMIL" => "IM", // Borsa Italiana
        "XMAD" => "SM", // Madrid
        "XSTO" => "SS", // Stockholm
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::openfigi_exch_code;

    #[test]
    fn maps_venues() {
        assert_eq!(openfigi_exch_code("XNAS"), Some("US"));
        assert_eq!(openfigi_exch_code("ARCX"), Some("US"));
        assert_eq!(openfigi_exch_code("XLON"), Some("LN"));
        assert_eq!(openfigi_exch_code("ZZZZ"), None);
    }
}
