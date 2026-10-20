///! Windows path handling: parsing, normalization, direct-child matching, device mapping.

use crate::types::DbPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKind {
    /// `C:\...`
    Drive,
    /// `\\server\share\...`
    Unc,
    /// `\Device\HarddiskVolume3\...` as produced by a kernel driver.
    Device,
}

/// A normalized absolute Windows path split into its root and its components.
///
/// `C:\Users\a` -> root `C:\`, components `[Users, a]`
/// `C:\`        -> root `C:\`, components `[]`
/// `\\srv\sh\a` -> root `\\srv\sh\`, components `[a]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinPath {
    pub kind: RootKind,
    pub root: String,
    pub comps: Vec<String>,
}

/// Drop `\\?\`, `\\.\` and `\??\` prefixes and turn `/` into `\`.
pub fn strip_extended(p: &str) -> String {
    let mut s = p.trim().replace('/', "\\");
    loop {
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("\\\\?\\") {
            let rest = &s[4..];
            s = if rest.to_ascii_lowercase().starts_with("unc\\") {
                // \\?\UNC\server\share -> \\server\share
                format!("\\\\{}", &rest[4..])
            } else {
                rest.to_string()
            };
        } else if lower.starts_with("\\\\.\\") {
            s = s[4..].to_string();
        } else if lower.starts_with("\\??\\") {
            // \??\Device\X -> \Device\X (keep exactly one leading separator)
            s = format!("\\{}", &s[4..]);
        } else {
            break;
        }
    }
    s
}

/// Collapse repeated separators, but never the first two of a UNC path.
fn collapse_seps(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_sep = false;
    for c in s.chars() {
        if c == '\\' {
            if !prev_sep {
                out.push('\\');
            }
            prev_sep = true;
        } else {
            prev_sep = false;
            out.push(c);
        }
    }
    out
}

/// Parse an absolute Windows path. Relative paths are rejected on purpose: every value
/// stored in the ledger must be unambiguous.
pub fn parse(raw: &str) -> Option<WinPath> {
    let s = strip_extended(raw);
    if s.is_empty() {
        return None;
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let root = format!("{}:\\", (b[0] as char).to_ascii_uppercase());
        return Some(WinPath {
            kind: RootKind::Drive,
            root,
            comps: split_comps(&collapse_seps(&s[2..])),
        });
    }
    if s.starts_with("\\\\") {
        // UNC: the first two separators are the prefix, only the tail is collapsed.
        let tail = collapse_seps(&s[2..]);
        let comps = split_comps(tail.trim_start_matches('\\'));
        if comps.len() < 2 {
            return None;
        }
        let root = format!("\\\\{}\\{}\\", comps[0], comps[1]);
        return Some(WinPath {
            kind: RootKind::Unc,
            root,
            comps: comps[2..].to_vec(),
        });
    }
    if s.starts_with('\\') {
        let comps = split_comps(&collapse_seps(&s));
        // Only volume device paths are meaningful here: \Device\X[...] ; a bare
        // \Device\X is the volume root and has no components.
        if comps.len() >= 2 && comps[0].eq_ignore_ascii_case("device") {
            let root = format!("\\{}\\{}\\", comps[0], comps[1]);
            return Some(WinPath {
                kind: RootKind::Device,
                root,
                comps: comps[2..].to_vec(),
            });
        }
        return None;
    }
    None
}

fn split_comps(s: &str) -> Vec<String> {
    s.split('\\')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(|c| c.to_string())
        .collect()
}

impl WinPath {
    /// Canonical string form, always with a trailing separator for bare roots.
    pub fn as_str(&self) -> String {
        if self.comps.is_empty() {
            self.root.clone()
        } else {
            format!("{}{}", self.root, self.comps.join("\\"))
        }
    }

    /// Case-insensitive comparison key.
    pub fn key(&self) -> String {
        self.as_str().to_ascii_lowercase()
    }

    pub fn is_root(&self) -> bool {
        self.comps.is_empty()
    }

    pub fn name(&self) -> Option<&str> {
        self.comps.last().map(|s| s.as_str())
    }

    pub fn parent(&self) -> Option<WinPath> {
        if self.comps.is_empty() {
            return None;
        }
        Some(WinPath {
            kind: self.kind,
            root: self.root.clone(),
            comps: self.comps[..self.comps.len() - 1].to_vec(),
        })
    }

    pub fn child(&self, name: &str) -> WinPath {
        let mut comps = self.comps.clone();
        comps.push(name.to_string());
        WinPath {
            kind: self.kind,
            root: self.root.clone(),
            comps,
        }
    }

    /// True when `self` is an immediate sub-directory of `root`.
    pub fn is_direct_child_of(&self, root: &WinPath) -> bool {
        self.kind == root.kind
            && self.comps.len() == root.comps.len() + 1
            && self
                .comps
                .iter()
                .zip(root.comps.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
            && self.root.eq_ignore_ascii_case(&root.root)
    }

    /// True when `self` is `root` itself or below it, at any depth.
    pub fn is_within(&self, root: &WinPath) -> bool {
        self.kind == root.kind
            && self.root.eq_ignore_ascii_case(&root.root)
            && self.comps.len() >= root.comps.len()
            && self.comps[..root.comps.len()]
                .iter()
                .zip(root.comps.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

/// Normalized absolute path string, or `None` when the input is relative/unusable.
pub fn normalize(raw: &str) -> Option<DbPath> {
    parse(raw).map(|p| p.as_str())
}

/// Comparison key for database lookups.
pub fn key_of(raw: &str) -> Option<DbPath> {
    parse(raw).map(|p| p.key())
}

/// Case-insensitive same-directory test.
pub fn same_path(a: &str, b: &str) -> bool {
    match (key_of(a), key_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

pub fn is_direct_child(root: &str, candidate: &str) -> bool {
    match (parse(root), parse(candidate)) {
        (Some(r), Some(c)) => c.is_direct_child_of(&r),
        _ => false,
    }
}

pub fn is_within(root: &str, candidate: &str) -> bool {
    match (parse(root), parse(candidate)) {
        (Some(r), Some(c)) => c.is_within(&r),
        _ => false,
    }
}

pub fn parent_path(raw: &str) -> Option<DbPath> {
    parse(raw).and_then(|p| p.parent()).map(|p| p.as_str())
}

pub fn file_name(raw: &str) -> Option<String> {
    parse(raw).and_then(|p| p.name().map(|s| s.to_string()))
}

/// Drive letter -> device object, e.g. `[("C:", "\Device\HarddiskVolume3")]`.
pub type DeviceMap = Vec<(String, String)>;

/// Rewrite a `\Device\...` path into its mounted drive form. `\??\` and `\\?\` prefixes
/// are ignored. Returns `None` when no mounted drive matches.
pub fn device_to_dos(dev_path: &str, map: &DeviceMap) -> Option<DbPath> {
    let p = parse(dev_path)?;
    if p.kind != RootKind::Device {
        return Some(p.as_str());
    }
    let needle = device_root_key(&p.root)?;
    let mut best: Option<&(String, String)> = None;
    let mut best_len = 0usize;
    for entry in map {
        let dev = device_root_key(&entry.1)?;
        if dev == needle && dev.len() >= best_len {
            best = Some(entry);
            best_len = dev.len();
        }
    }
    let (dos, _) = best?;
    normalize(&format!("{}\\{}", dos.trim_end_matches('\\'), p.comps.join("\\")))
}

/// Normalized lowercase `\device\harddiskvolume3` key for a device object name, whatever
/// prefix (`\??\`, `\\?\`) the caller supplied. `None` for non-volume device names.
fn device_root_key(dev: &str) -> Option<String> {
    let p = parse(dev)?;
    if p.kind != RootKind::Device {
        return None;
    }
    Some(p.root.trim_end_matches('\\').to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shapes() {
        assert_eq!(normalize("c:\\users\\a\\").unwrap(), "C:\\users\\a");
        assert_eq!(normalize("C:\\").unwrap(), "C:\\");
        assert_eq!(normalize("\\\\?\\C:\\a\\b").unwrap(), "C:\\a\\b");
        assert_eq!(normalize("C:/a/b").unwrap(), "C:\\a\\b");
        assert_eq!(normalize("C:").unwrap(), "C:\\");
        assert_eq!(normalize("C:\\a\\.\\b").unwrap(), "C:\\a\\b");
        assert_eq!(normalize("\\\\srv\\sh\\a\\b\\").unwrap(), "\\\\srv\\sh\\a\\b");
        assert_eq!(normalize("\\\\?\\UNC\\srv\\sh\\x").unwrap(), "\\\\srv\\sh\\x");
        assert_eq!(normalize("a\\b"), None);
        assert_eq!(normalize(""), None);
        assert_eq!(parse("\\Device\\HarddiskVolume3\\Users\\a").unwrap().comps, vec!["Users", "a"]);
        assert_eq!(parse("\\??\\Device\\HarddiskVolume3\\Users").unwrap().comps, vec!["Users"]);
        assert!(parse("\\Device\\HarddiskVolume3").unwrap().is_root());
        assert_eq!(parse("\\\\srv\\sh").unwrap().comps, Vec::<String>::new());
    }

    #[test]
    fn unc_and_device_roots() {
        // The two leading separators of a UNC path must survive normalization.
        assert_eq!(parent_path("\\\\srv\\sh\\a").unwrap(), "\\\\srv\\sh\\");
        assert_eq!(parent_path("\\\\srv\\sh"), None);
        assert!(is_direct_child("\\\\srv\\sh", "\\\\srv\\sh\\a"));
        assert!(!is_direct_child("\\\\srv\\sh", "\\\\srv\\sh\\a\\b"));
        assert!(!is_direct_child("\\\\srv\\share", "\\\\srv\\shareBackup\\a"));
    }

    #[test]
    fn parents_and_names() {
        assert_eq!(parent_path("C:\\Users\\a\\b").unwrap(), "C:\\Users\\a");
        assert_eq!(parent_path("C:\\a").unwrap(), "C:\\");
        assert_eq!(parent_path("C:\\"), None);
        assert_eq!(parent_path("\\\\srv\\sh"), None);
        assert_eq!(parent_path("\\\\srv\\sh\\a").unwrap(), "\\\\srv\\sh\\");
        assert_eq!(file_name("C:\\a\\AppA.exe").unwrap(), "AppA.exe");
        assert_eq!(file_name("C:\\"), None);
    }

    #[test]
    fn direct_child_is_separator_bounded() {
        let root = "C:\\Users\\Administrator\\AppData\\Local";
        assert!(is_direct_child(root, "C:\\Users\\Administrator\\AppData\\Local\\AppA"));
        assert!(is_direct_child(root, "c:\\users\\administrator\\appdata\\local\\AppA"));
        // prefix-name trap: LocalBackup is not inside Local
        assert!(!is_direct_child(root, "C:\\Users\\Administrator\\AppData\\LocalBackup\\AppA"));
        assert!(!is_direct_child(root, "C:\\Users\\Administrator\\AppData\\Local\\AppA\\sub"));
        assert!(!is_direct_child(root, root));
        assert!(!is_direct_child(root, "D:\\Users\\Administrator\\AppData\\Local\\AppA"));
    }

    #[test]
    fn within_and_same() {
        let root = "C:\\a";
        assert!(is_within(root, "C:\\a\\b\\c"));
        assert!(is_within(root, "c:\\A"));
        assert!(!is_within(root, "C:\\ab\\c"));
        assert!(same_path("C:\\a\\", "c:\\a"));
    }

    #[test]
    fn device_mapping() {
        let map: DeviceMap = vec![
            ("C:".into(), "\\Device\\HarddiskVolume3".into()),
            ("D:".into(), "\\??\\Device\\HarddiskVolume4".into()),
        ];
        assert_eq!(
            device_to_dos("\\Device\\HarddiskVolume3\\Users\\a", &map).unwrap(),
            "C:\\Users\\a"
        );
        assert_eq!(
            device_to_dos("\\??\\Device\\HarddiskVolume4\\x", &map).unwrap(),
            "D:\\x"
        );
        assert_eq!(device_to_dos("\\Device\\Floppy0\\x", &map), None);
        assert_eq!(device_to_dos("C:\\y", &map).unwrap(), "C:\\y");
    }
}
