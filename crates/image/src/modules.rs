//! Pull kernel modules (plus their dependencies) out of Ubuntu `linux-modules*` debs.
//!
//! The debs ship no `modules.dep` (depmod runs in postinst), so we read each module's
//! `.modinfo` `depends=` ourselves and emit an ordered `modules.load` for the guest init.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

/// Modules the guest needs beyond what Ubuntu's kernel builds in.
pub const DEFAULT_MODULES: &[&str] = &[
    "binder_linux", // Android IPC (redroid)
    "vmw_vsock_virtio_transport", // host <-> guest control channel
    "zram",
    "overlay",
    "veth",
    "tun",
    "uinput",
    "fuse",
    "xfrm_user", // netd opens a NETLINK_XFRM socket at startup and exits if it can't
    "dummy",     // netd's bandwidth controller expects a `dummy0` interface
];

const BUILTIN_PREFIX: &str = "builtin:";

/// Whole kernel source directories to include. Android's netd configures iptables/ip6tables
/// with dozens of match/target modules and the guest has no modprobe, so on-demand loading
/// (`request_module`) can't work; shipping the netfilter families is simpler than chasing
/// each `xt_*` by name. IPVS is irrelevant and excluded.
pub const DEFAULT_MODULE_DIRS: &[&str] = &["/kernel/net/netfilter/", "/kernel/net/ipv4/netfilter/", "/kernel/net/ipv6/netfilter/"];
const EXCLUDED_DIRS: &[&str] = &["/ipvs/"];

pub struct Module {
    pub name: String,
    pub depends: Vec<String>,
    /// Uncompressed `.ko` bytes (only kept for modules in the closure).
    pub data: Vec<u8>,
}

/// Canonical module name: file stem without `.ko[.zst]`, `-` → `_`.
fn module_name(path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let stem = file.strip_suffix(".ko.zst").or_else(|| file.strip_suffix(".ko"))?;
    Some(stem.replace('-', "_"))
}

/// Extract `depends=a,b` from a module's `.modinfo` (NUL-separated `key=value` strings).
pub fn parse_depends(ko: &[u8]) -> Vec<String> {
    let key = b"depends=";
    let mut i = 0;
    while let Some(p) = ko[i..].windows(key.len()).position(|w| w == key) {
        let start = i + p;
        // must be at the start of a string: preceded by NUL
        if start == 0 || ko[start - 1] == 0 {
            let end = ko[start..].iter().position(|&b| b == 0).map_or(ko.len(), |e| start + e);
            let v = String::from_utf8_lossy(&ko[start + key.len()..end]);
            return v.split(',').filter(|s| !s.is_empty()).map(|s| s.replace('-', "_")).collect();
        }
        i = start + key.len();
    }
    Vec::new()
}

/// Walk every `.ko[.zst]` in the debs, calling `f(name, path_in_deb, bytes)`.
fn for_each_module(debs: &[&Path], mut f: impl FnMut(String, &str, Vec<u8>)) -> Result<()> {
    for deb in debs {
        let file = std::fs::File::open(deb).with_context(|| format!("open {}", deb.display()))?;
        let mut ar = ar::Archive::new(file);
        let mut found_data = false;
        while let Some(entry) = ar.next_entry() {
            let entry = entry?;
            let id = String::from_utf8_lossy(entry.header().identifier()).to_string();
            if !id.starts_with("data.tar") {
                continue;
            }
            found_data = true;
            // Ubuntu's kernel debs use a plain `data.tar` (modules are .ko.zst individually);
            // other packagings use .zst or .gz.
            let payload: Box<dyn Read> = match id.trim_end_matches('/') {
                "data.tar" => Box::new(entry),
                "data.tar.zst" => Box::new(zstd::stream::read::Decoder::new(entry)?),
                "data.tar.gz" => Box::new(flate2::read::GzDecoder::new(entry)),
                other => bail!("{}: unsupported deb payload {other}", deb.display()),
            };
            let mut tar = tar::Archive::new(payload);
            for e in tar.entries()? {
                let mut e = e?;
                let path = e.path()?.to_string_lossy().to_string();
                if path.ends_with("/modules.builtin") {
                    // One `kernel/.../name.ko` per line: built into the kernel, nothing to load.
                    let mut txt = String::new();
                    e.read_to_string(&mut txt)?;
                    for line in txt.lines() {
                        if let Some(n) = module_name(line) {
                            f(format!("{BUILTIN_PREFIX}{n}"), "", Vec::new());
                        }
                    }
                    continue;
                }
                let Some(name) = module_name(&path) else { continue };
                if !path.contains("/modules/") {
                    continue;
                }
                let mut raw = Vec::with_capacity(e.size() as usize);
                e.read_to_end(&mut raw)?;
                let ko = if path.ends_with(".zst") { zstd::decode_all(&raw[..])? } else { raw };
                f(name, &path, ko);
            }
        }
        if !found_data {
            bail!("{}: no data.tar.* member", deb.display());
        }
    }
    Ok(())
}

/// Resolve `wanted` plus transitive deps from `debs`. Returns modules in load order
/// (dependencies first).
pub fn collect(debs: &[&Path], wanted: &[&str], wanted_dirs: &[&str]) -> Result<Vec<Module>> {
    // Pass 1: dependency graph of every module (bytes discarded).
    let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut builtin: BTreeSet<String> = BTreeSet::new();
    let mut from_dirs: BTreeSet<String> = BTreeSet::new();
    for_each_module(debs, |name, path, ko| match name.strip_prefix(BUILTIN_PREFIX) {
        Some(b) => {
            builtin.insert(b.to_string());
        }
        None => {
            if wanted_dirs.iter().any(|d| path.contains(d)) && !EXCLUDED_DIRS.iter().any(|d| path.contains(d)) {
                from_dirs.insert(name.clone());
            }
            graph.insert(name, parse_depends(&ko));
        }
    })?;

    // Built-in modules need no loading; anything else must exist as a .ko.
    let mut wanted: Vec<&str> = wanted.iter().copied().filter(|w| !builtin.contains(*w)).collect();
    let dir_names: Vec<String> = from_dirs.into_iter().filter(|n| !wanted.contains(&n.as_str())).collect();
    wanted.extend(dir_names.iter().map(String::as_str));
    for w in &wanted {
        if !graph.contains_key(*w) {
            bail!("module `{w}` is neither a loadable module nor built into this kernel");
        }
    }

    // Topological order via DFS.
    let mut order: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    fn visit(
        n: &str,
        graph: &BTreeMap<String, Vec<String>>,
        builtin: &BTreeSet<String>,
        seen: &mut BTreeSet<String>,
        order: &mut Vec<String>,
        depth: usize,
    ) -> Result<()> {
        if depth > 64 {
            bail!("module dependency cycle at {n}");
        }
        if builtin.contains(n) || !seen.insert(n.to_string()) {
            return Ok(());
        }
        let deps = graph.get(n).ok_or_else(|| anyhow!("dependency `{n}` missing from module packages"))?;
        for d in deps {
            visit(d, graph, builtin, seen, order, depth + 1)?;
        }
        order.push(n.to_string());
        Ok(())
    }
    for w in &wanted {
        visit(w, &graph, &builtin, &mut seen, &mut order, 0)?;
    }

    // Pass 2: keep bytes for the closure only.
    let mut by_name: BTreeMap<String, Module> = BTreeMap::new();
    let need: BTreeSet<&String> = order.iter().collect();
    for_each_module(debs, |name, _path, ko| {
        if !name.starts_with(BUILTIN_PREFIX) && need.contains(&name) {
            let depends = parse_depends(&ko);
            by_name.insert(name.clone(), Module { name, depends, data: ko });
        }
    })?;
    order
        .into_iter()
        .map(|n| by_name.remove(&n).ok_or_else(|| anyhow!("module `{n}` vanished on second pass")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(module_name("./lib/modules/6/kernel/net/veth.ko.zst").unwrap(), "veth");
        assert_eq!(module_name("a/b/vmw_vsock-virtio.ko").unwrap(), "vmw_vsock_virtio");
        assert!(module_name("a/modules.dep").is_none());
    }

    #[test]
    fn depends_parsing() {
        let mut ko = b"\x7fELF....".to_vec();
        ko.extend_from_slice(b"\0license=GPL\0depends=vsock,virtio-ring\0retpoline=Y\0");
        assert_eq!(parse_depends(&ko), vec!["vsock", "virtio_ring"]);
        // `depends=` inside another string must not match
        let ko2 = b"\0description=x depends=nope\0depends=\0".to_vec();
        assert!(parse_depends(&ko2).is_empty());
    }
}
