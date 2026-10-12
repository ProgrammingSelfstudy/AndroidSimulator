//! Pull an image from a Docker Hub-style registry and flatten its layers into one tarball.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use flate2::read::GzDecoder;
use serde_json::Value;

use crate::arch::Arch;
use crate::fetch::Fetcher;

const REGISTRY: &str = "https://registry-1.docker.io";
const AUTH: &str = "https://auth.docker.io/token?service=registry.docker.io";
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.v2+json";

pub use moniqi_proto::RunConfig;

#[derive(Debug)]
pub struct PulledImage {
    pub layers: Vec<PathBuf>,
    pub config: RunConfig,
}

fn get_text(url: &str, token: &str, accept: &str) -> Result<String> {
    let mut resp = ureq::get(url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", accept)
        .call()
        .with_context(|| format!("GET {url}"))?;
    resp.body_mut().read_to_string().with_context(|| format!("read {url}"))
}

fn token(repo: &str) -> Result<String> {
    let url = format!("{AUTH}&scope=repository:{repo}:pull");
    let mut resp = ureq::get(&url).call().with_context(|| format!("GET {url}"))?;
    let v: Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    v["token"].as_str().map(str::to_string).ok_or_else(|| anyhow!("no token in auth response"))
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

pub fn pull(f: &mut Fetcher, repo: &str, tag: &str, arch: Arch) -> Result<PulledImage> {
    let tok = token(repo)?;
    let mut manifest: Value =
        serde_json::from_str(&get_text(&format!("{REGISTRY}/v2/{repo}/manifests/{tag}"), &tok, MANIFEST_ACCEPT)?)?;

    // Multi-arch index: resolve to the platform manifest.
    if let Some(list) = manifest["manifests"].as_array() {
        let want = arch.deb(); // "arm64" / "amd64" match the OCI architecture names
        let digest = list
            .iter()
            .find(|m| m["platform"]["architecture"] == want && m["platform"]["os"] == "linux")
            .and_then(|m| m["digest"].as_str())
            .ok_or_else(|| anyhow!("{repo}:{tag} has no linux/{want} image"))?
            .to_string();
        manifest = serde_json::from_str(&get_text(
            &format!("{REGISTRY}/v2/{repo}/manifests/{digest}"),
            &tok,
            MANIFEST_ACCEPT,
        )?)?;
    }

    let cfg_digest = manifest["config"]["digest"].as_str().ok_or_else(|| anyhow!("manifest has no config"))?;
    let cfg: Value = serde_json::from_str(&get_text(
        &format!("{REGISTRY}/v2/{repo}/blobs/{cfg_digest}"),
        &tok,
        "application/octet-stream",
    )?)?;
    let config = RunConfig {
        entrypoint: strings(&cfg["config"]["Entrypoint"]),
        cmd: strings(&cfg["config"]["Cmd"]),
        env: strings(&cfg["config"]["Env"]),
    };

    let layers_json = manifest["layers"].as_array().ok_or_else(|| anyhow!("manifest has no layers"))?;
    let mut layers = Vec::new();
    for (i, l) in layers_json.iter().enumerate() {
        let digest = l["digest"].as_str().ok_or_else(|| anyhow!("layer {i} has no digest"))?;
        let mt = l["mediaType"].as_str().unwrap_or("");
        if !(mt.contains("gzip") || mt.ends_with("tar")) {
            bail!("layer {i}: unsupported media type {mt}");
        }
        let hex = digest.strip_prefix("sha256:").ok_or_else(|| anyhow!("unsupported digest {digest}"))?;
        let name = format!("oci-{hex}.tar.gz");
        let auth = format!("Bearer {tok}");
        let p = f.fetch_with(
            &format!("{REGISTRY}/v2/{repo}/blobs/{digest}"),
            &name,
            Some(hex),
            &[("Authorization", auth.as_str())],
        )?;
        layers.push(p);
    }
    Ok(PulledImage { layers, config })
}

fn open_layer(p: &Path) -> Result<tar::Archive<Box<dyn Read>>> {
    let mut f = File::open(p)?;
    let mut magic = [0u8; 2];
    let n = f.read(&mut magic)?;
    let f = File::open(p)?;
    let r: Box<dyn Read> = if n == 2 && magic == [0x1f, 0x8b] { Box::new(GzDecoder::new(f)) } else { Box::new(f) };
    Ok(tar::Archive::new(r))
}

fn norm(p: &Path) -> String {
    p.to_string_lossy().trim_start_matches("./").trim_end_matches('/').to_string()
}

enum Whiteout {
    Path(String),   // delete this path (and anything under it) from lower layers
    Opaque(String), // delete everything under this dir from lower layers
}

fn whiteout_of(path: &str) -> Option<Whiteout> {
    let (dir, base) = match path.rsplit_once('/') {
        Some((d, b)) => (d, b),
        None => ("", path),
    };
    if base == ".wh..wh..opq" {
        return Some(Whiteout::Opaque(dir.to_string()));
    }
    let target = base.strip_prefix(".wh.")?;
    Some(Whiteout::Path(if dir.is_empty() { target.to_string() } else { format!("{dir}/{target}") }))
}

fn under(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || path.starts_with(&format!("{dir}/"))
}

/// Apply `layers` in order into a single uncompressed tar (OCI layer semantics: later layers
/// override earlier ones; `.wh.<name>` deletes; `.wh..wh..opq` empties a directory).
/// `extra` entries (path, bytes, mode) are appended last.
pub fn flatten(layers: &[PathBuf], extra: &[(String, Vec<u8>, u32)], out: &Path) -> Result<()> {
    // Pass 1: which layer last defines each path, and which whiteouts apply from which layer.
    let mut last_def: BTreeMap<String, usize> = BTreeMap::new();
    let mut whiteouts: Vec<(usize, Whiteout)> = Vec::new();
    for (i, l) in layers.iter().enumerate() {
        for e in open_layer(l)?.entries()? {
            let e = e?;
            let path = norm(&e.path()?);
            if path.is_empty() {
                continue;
            }
            match whiteout_of(&path) {
                Some(w) => whiteouts.push((i, w)),
                None => {
                    last_def.insert(path, i);
                }
            }
        }
    }
    let removed = |path: &str, layer: usize| {
        whiteouts.iter().any(|(wl, w)| {
            *wl > layer
                && match w {
                    Whiteout::Path(p) => under(path, p),
                    Whiteout::Opaque(d) => path != d && under(path, d),
                }
        })
    };

    // Pass 2: emit survivors. Directories may legitimately repeat across layers: keep the last.
    let mut builder = tar::Builder::new(File::create(out)?);
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    for (i, l) in layers.iter().enumerate() {
        for e in open_layer(l)?.entries()? {
            let mut e = e?;
            let path = norm(&e.path()?);
            if path.is_empty() || whiteout_of(&path).is_some() {
                continue;
            }
            if last_def.get(&path) != Some(&i) || removed(&path, i) || !emitted.insert(path.clone()) {
                continue;
            }
            let mut header = e.header().clone();
            let t = header.entry_type();
            if t.is_symlink() || t.is_hard_link() {
                let target = e.link_name()?.context("link without target")?.into_owned();
                builder.append_link(&mut header, &path, target)?;
            } else {
                builder.append_data(&mut header, &path, &mut e)?;
            }
        }
    }
    for (path, data, mode) in extra {
        let mut h = tar::Header::new_gnu();
        h.set_mode(*mode);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(data.len() as u64);
        builder.append_data(&mut h, path, &data[..])?;
    }
    builder.into_inner()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn layer(dir: &Path, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let p = dir.join(name);
        let mut b = tar::Builder::new(File::create(&p).unwrap());
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap().flush().unwrap();
        p
    }

    fn names(tar_path: &Path) -> Vec<String> {
        let mut v: Vec<String> = tar::Archive::new(File::open(tar_path).unwrap())
            .entries()
            .unwrap()
            .map(|e| norm(&e.unwrap().path().unwrap()))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn whiteout_parsing() {
        assert!(matches!(whiteout_of("a/.wh.b"), Some(Whiteout::Path(p)) if p == "a/b"));
        assert!(matches!(whiteout_of(".wh.x"), Some(Whiteout::Path(p)) if p == "x"));
        assert!(matches!(whiteout_of("a/.wh..wh..opq"), Some(Whiteout::Opaque(d)) if d == "a"));
        assert!(whiteout_of("a/b").is_none());
    }

    #[test]
    fn flatten_applies_overrides_whiteouts_and_opaque_dirs() {
        let d = std::env::temp_dir().join(format!("moniqi-oci-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let l0 = layer(&d, "l0", &[("keep", b"0"), ("over", b"old"), ("gone", b"x"), ("dir/a", b"1"), ("dir/b", b"1"), ("rmdir/f", b"1")]);
        let l1 = layer(&d, "l1", &[("over", b"new"), (".wh.gone", b""), ("dir/.wh..wh..opq", b""), ("dir/c", b"2"), (".wh.rmdir", b"")]);
        let out = d.join("out.tar");
        flatten(&[l0, l1], &[("extra/file".into(), b"e".to_vec(), 0o600)], &out).unwrap();

        assert_eq!(names(&out), vec!["dir/c", "extra/file", "keep", "over"]);
        let mut a = tar::Archive::new(File::open(&out).unwrap());
        let mut content = String::new();
        for e in a.entries().unwrap() {
            let mut e = e.unwrap();
            if norm(&e.path().unwrap()) == "over" {
                e.read_to_string(&mut content).unwrap();
            }
        }
        assert_eq!(content, "new");
        std::fs::remove_dir_all(&d).ok();
    }
}
