use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Download cache with integrity checking.
///
/// Artifacts with a pinned sha256 are verified against it. Rolling artifacts (the Ubuntu
/// "unpacked" kernel, module debs) are trust-on-first-use: the hash is recorded in
/// `artifacts.lock` and checked on every later cache hit.
pub struct Fetcher {
    cache: PathBuf,
    lock_path: PathBuf,
    lock: BTreeMap<String, String>,
}

impl Fetcher {
    pub fn new(cache: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache)?;
        let lock_path = cache.join("artifacts.lock");
        let lock = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Ok(Self { cache: cache.to_path_buf(), lock_path, lock })
    }

    fn save_lock(&self) -> Result<()> {
        std::fs::write(&self.lock_path, serde_json::to_string_pretty(&self.lock)?)?;
        Ok(())
    }

    /// Return the cached path of `name`, downloading `url` first if needed.
    pub fn fetch(&mut self, url: &str, name: &str, pinned_sha256: Option<&str>) -> Result<PathBuf> {
        self.fetch_with(url, name, pinned_sha256, &[])
    }

    /// Like [`fetch`](Self::fetch) with extra request headers. Headers are not forwarded across
    /// redirects (ureq's default), which registries rely on: blobs 307 to a signed CDN URL.
    pub fn fetch_with(
        &mut self,
        url: &str,
        name: &str,
        pinned_sha256: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<PathBuf> {
        let dest = self.cache.join(name);
        let expected = pinned_sha256.map(str::to_string).or_else(|| self.lock.get(name).cloned());

        if dest.exists() {
            let have = sha256_file(&dest)?;
            match &expected {
                Some(e) if *e == have => return Ok(dest),
                Some(e) => {
                    eprintln!("cache: {name} hash mismatch (have {have}, want {e}); re-downloading");
                    std::fs::remove_file(&dest)?;
                }
                None => {
                    self.lock.insert(name.to_string(), have);
                    self.save_lock()?;
                    return Ok(dest);
                }
            }
        }

        eprintln!("download: {url}");
        let part = self.cache.join(format!("{name}.part"));
        let mut req = ureq::get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let total = resp.body().content_length();
        let mut reader = resp.into_body().into_reader();
        let mut out = File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        let (mut done, mut last_print) = (0u64, 0u64);
        loop {
            let n = reader.read(&mut buf).with_context(|| format!("read {url}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            done += n as u64;
            if done - last_print >= 8 << 20 {
                last_print = done;
                match total {
                    Some(t) => eprintln!("  {name}: {} / {} MiB", done >> 20, t >> 20),
                    None => eprintln!("  {name}: {} MiB", done >> 20),
                }
            }
        }
        out.flush()?;
        drop(out);
        if let Some(t) = total {
            if t != done {
                bail!("{name}: truncated download ({done} of {t} bytes)");
            }
        }
        let got = format!("{:x}", hasher.finalize());
        if let Some(e) = pinned_sha256 {
            if e != got {
                std::fs::remove_file(&part).ok();
                bail!("{name}: sha256 mismatch: expected {e}, got {got}");
            }
        }
        std::fs::rename(&part, &dest)?;
        self.lock.insert(name.to_string(), got);
        self.save_lock()?;
        Ok(dest)
    }
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
