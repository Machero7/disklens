//! Formatting and file-type helpers shared by the CLI and GUI.

/// Binary-unit size: "512 B", "1.5 KB", "123 GB".
pub fn human(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    for unit in ["KB", "MB", "GB", "TB", "PB"] {
        v /= 1024.0;
        if v < 1024.0 || unit == "PB" {
            return if v < 100.0 { format!("{v:.1} {unit}") } else { format!("{v:.0} {unit}") };
        }
    }
    unreachable!()
}

/// "1234567" -> "1,234,567"
pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub const NO_EXT: &str = "(none)";
pub const RAR_PARTS: &str = ".r00–.r99";

/// Lower-case extension used for grouping by file type.
/// Implausible extensions (too long, odd characters) count as none;
/// split-RAR volumes `.r00`..`.r99` are grouped together.
pub fn ext_of(name: &str) -> String {
    let ext = match name.rfind('.') {
        // a leading dot is a hidden-file marker, not an extension (".bashrc")
        Some(i) if i > 0 => &name[i..],
        _ => return NO_EXT.to_string(),
    };
    let ext = ext.to_lowercase();
    let body = &ext[1..];
    if body.is_empty()
        || ext.chars().count() > 10
        || !body.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return NO_EXT.to_string();
    }
    let b = body.as_bytes();
    if b.len() == 3 && b[0] == b'r' && b[1].is_ascii_digit() && b[2].is_ascii_digit() {
        return RAR_PARTS.to_string();
    }
    ext
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1023), "1023 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(150 * 1024 * 1024 * 1024), "150 GB");
    }

    #[test]
    fn ints() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(947261), "947,261");
        assert_eq!(fmt_int(1000), "1,000");
    }

    #[test]
    fn extensions() {
        assert_eq!(ext_of("Movie.MKV"), ".mkv");
        assert_eq!(ext_of(".bashrc"), NO_EXT);
        assert_eq!(ext_of("Makefile"), NO_EXT);
        assert_eq!(ext_of("archive.r07"), RAR_PARTS);
        assert_eq!(ext_of("archive.rar"), ".rar");
        assert_eq!(ext_of("x.4B935656___0F5B77A01405_ [NEWZNZB] (1_6040)"), NO_EXT);
        assert_eq!(ext_of("trailing."), NO_EXT);
    }
}
