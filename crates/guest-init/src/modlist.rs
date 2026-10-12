/// Parse `modules.load` (one module name per line, `#` comments allowed).
///
/// Names become file paths, so anything but `[A-Za-z0-9_]` is rejected rather than trusted.
pub fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter(|l| l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_filters() {
        let t = "binder_linux\n\n# comment\nvsock # trailing\n../evil\nzram\n";
        assert_eq!(parse(t), vec!["binder_linux", "vsock", "zram"]);
    }
}
