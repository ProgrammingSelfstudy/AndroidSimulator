use std::collections::HashMap;

/// Options moniqi reads from the kernel command line (`moniqi.key=value`).
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub zram_mib: u64,
    pub agent: String,
    pub net: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { zram_mib: 1024, agent: "/usr/local/bin/moniqi-agent".into(), net: true }
    }
}

/// Collect `moniqi.*` key/values (prefix stripped). Later occurrences win, like the kernel.
pub fn moniqi_args(cmdline: &str) -> HashMap<String, String> {
    cmdline
        .split_whitespace()
        .filter_map(|tok| tok.strip_prefix("moniqi.")?.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub fn parse(cmdline: &str) -> Options {
    let args = moniqi_args(cmdline);
    let mut o = Options::default();
    if let Some(v) = args.get("zram_mib").and_then(|v| v.parse().ok()) {
        o.zram_mib = v;
    }
    if let Some(v) = args.get("agent") {
        o.agent = v.clone();
    }
    if let Some(v) = args.get("net") {
        o.net = !matches!(v.as_str(), "0" | "off" | "no");
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_absent() {
        assert_eq!(parse("console=hvc0 root=/dev/vda ro"), Options::default());
    }

    #[test]
    fn overrides() {
        let o = parse("console=hvc0 moniqi.zram_mib=512 moniqi.agent=/bin/x moniqi.net=off quiet");
        assert_eq!(o, Options { zram_mib: 512, agent: "/bin/x".into(), net: false });
    }

    #[test]
    fn ignores_garbage_values_and_foreign_keys() {
        let o = parse("moniqi.zram_mib=lots other.zram_mib=7 moniqi.novalue");
        assert_eq!(o.zram_mib, 1024);
    }
}
