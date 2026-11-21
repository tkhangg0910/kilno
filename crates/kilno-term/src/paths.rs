//! Local paths in terminal text (`C:\work\app.rs`, `src/main.rs:42:7`, `.\docs`): found under
//! the pointer, so Ctrl+click can show them in the Files panel. Only a path that exists counts;
//! the lookup is the caller's (it knows the pane's folder), through [`resolve`].

use std::path::{Path, PathBuf};

/// Part of a path: letters and digits in any script, and the punctuation file names use.
/// Everything else (spaces, quotes, brackets, box drawing, bullets) ends one.
fn in_path(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '\\' | '/' | ':' | '.' | '_' | '-' | '~' | '@' | '+' | '#' | '%' | '=' | '$' | '!' | '&')
}

/// The candidate under column `col` of a row (one char per column), and the columns it covers
/// (end exclusive): trailing sentence punctuation and a `:line[:col]` suffix are left out. None
/// when the text there does not look like a path (no separator and no extension).
pub fn candidate(row: &[char], col: usize) -> Option<(String, usize, usize)> {
    if col >= row.len() || !in_path(row[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && in_path(row[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < row.len() && in_path(row[end]) {
        end += 1;
    }
    let mut text: String = row[start..end].iter().collect();
    // "see app.rs." / "(at C:\x\y.rs:12:3)": the path is what is left.
    loop {
        let before = text.len();
        while text.ends_with(['.', ',', ':', ';', '!']) {
            text.pop();
        }
        if let Some(cut) = line_suffix(&text) {
            text.truncate(cut);
        }
        if text.len() == before {
            break;
        }
    }
    let looks = text.contains(['\\', '/']) || has_extension(&text);
    if text.is_empty() || !looks || text.contains("://") {
        return None;
    }
    let end = start + text.chars().count();
    (col < end).then_some((text, start, end))
}

/// Where a trailing `:12` or `:12:7` starts, if there is one (a drive's `C:` is not one).
fn line_suffix(s: &str) -> Option<usize> {
    let mut cut = s.len();
    for _ in 0..2 {
        let Some(i) = s[..cut].rfind(':') else { break };
        let digits = &s[i + 1..cut];
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || i < 2 {
            break;
        }
        cut = i;
    }
    (cut < s.len()).then_some(cut)
}

fn has_extension(s: &str) -> bool {
    Path::new(s)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| !e.is_empty() && e.len() <= 10 && e.chars().all(char::is_alphanumeric))
}

/// The candidate as an existing file or folder on a local drive: absolute as it is, else under
/// `cwd`. Anything that is not a drive path (`\server\share`, `/\server`, `\?\UNC\...`,
/// `\.\pipe\...`) is never looked up: terminal text is untrusted, and touching a network path
/// would send the user's NTLM credentials to whatever host it names (or stall for seconds).
pub fn resolve(text: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let p = Path::new(text);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        // "C:foo" (drive-relative) and "\foo" (root of the current drive) are not worth guessing.
        if p.has_root() || text.get(1..2) == Some(":") {
            return None;
        }
        cwd?.join(p)
    };
    // Judged on the parsed prefix, not the text, so no mix of separators slips a UNC path past.
    if !on_local_drive(&full) {
        return None;
    }
    // "src/a.rs" under a folder, or ".\docs": one kind of separator and no "." parts (Explorer's
    // /select wants that).
    let full: PathBuf = full.components().collect();
    full.exists().then_some(full)
}

/// `C:\...` or `\\?\C:\...`; on other systems, an absolute path that cannot reach the network:
/// not under an automounter's `/net`, `/smb` or `/afs` (looking a name up there connects to that
/// host) and not inside a network file system that is mounted ([`network_mounts`]).
fn on_local_drive(p: &Path) -> bool {
    use std::path::{Component, Prefix};
    match p.components().next() {
        Some(Component::Prefix(pre)) => matches!(pre.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)),
        Some(Component::RootDir) if !cfg!(windows) => {
            let automount = ["/net", "/smb", "/afs"].iter().any(|m| p.starts_with(m));
            !automount && !network_mounts(&read_mountinfo()).iter().any(|m| p.starts_with(m))
        }
        _ => false,
    }
}

/// `/proc/self/mountinfo`, or "" where there is none.
fn read_mountinfo() -> String {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
    } else {
        String::new()
    }
}

/// The mount points of network file systems in a `mountinfo` text: field 5 is the mount point,
/// and the type follows the ` - ` separator.
fn network_mounts(mountinfo: &str) -> Vec<PathBuf> {
    // Not 9p: WSL mounts the Windows drives (/mnt/c) with it, and they are local.
    const NETWORK: [&str; 8] = ["nfs", "nfs4", "cifs", "smb3", "smbfs", "afs", "ncpfs", "fuse.sshfs"];
    mountinfo
        .lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let point = left.split(' ').nth(4)?;
            let kind = right.split(' ').next()?;
            NETWORK.contains(&kind).then(|| PathBuf::from(point.replace("\\040", " ")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str, col: usize) -> Option<String> {
        let row: Vec<char> = s.chars().collect();
        candidate(&row, col).map(|c| c.0)
    }

    #[test]
    fn finds_paths_and_trims_them() {
        assert_eq!(at(r"  Read C:\work\app.rs now", 10).as_deref(), Some(r"C:\work\app.rs"));
        assert_eq!(at("Edited crates/x/src/lib.rs:42:7.", 12).as_deref(), Some("crates/x/src/lib.rs"));
        assert_eq!(at("(see README.md)", 7).as_deref(), Some("README.md"));
        assert_eq!(at("⎿ docs/faq.md, done", 4).as_deref(), Some("docs/faq.md"));
        assert_eq!(at(r"D:\Clayo\kiln:", 3).as_deref(), Some(r"D:\Clayo\kiln"));
        assert_eq!(at("│./scripts│", 3).as_deref(), Some("./scripts"));
    }

    #[test]
    fn ignores_what_is_not_a_path() {
        assert_eq!(at("hello world", 2), None, "a plain word");
        assert_eq!(at("see https://example.com/a.html", 8), None, "a web link");
        assert_eq!(at("a b", 1), None, "a space");
        assert_eq!(at("x", 5), None, "past the row");
        assert_eq!(at("app.rs: done", 7), None, "after the path");
    }

    #[test]
    fn columns_cover_the_trimmed_path() {
        let row: Vec<char> = "at src/a.rs:3 ok".chars().collect();
        let (_, s, e) = candidate(&row, 4).unwrap();
        assert_eq!((s, e), (3, 11));
        assert!(candidate(&row, 12).is_none(), "the :3 suffix is not part of the link");
    }

    #[test]
    fn resolves_only_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("a.rs"), "").unwrap();
        assert_eq!(resolve("src/a.rs", Some(dir.path())), Some(dir.path().join("src").join("a.rs")));
        assert_eq!(resolve("src/b.rs", Some(dir.path())), None, "missing");
        assert_eq!(resolve("src/a.rs", None), None, "relative with no folder");
        let abs = dir.path().join("src");
        assert_eq!(resolve(&abs.to_string_lossy(), None), Some(abs));
        for unc in [r"\\server\share\x", r"\/server\share", r"/\server\share", "//server/share", r"\\?\UNC\server\share", r"\\.\pipe\x"] {
            assert_eq!(resolve(unc, None), None, "never a network or device path: {unc}");
            assert_eq!(resolve(unc, Some(dir.path())), None, "not under a folder either: {unc}");
        }
    }

    #[test]
    fn network_mounts_from_mountinfo() {
        let info = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
40 22 0:35 / /mnt/c rw,noatime - 9p drvfs rw
41 22 0:36 / /srv/nfs\\040share rw - nfs4 host:/x rw
42 22 0:37 / /home/me/remote rw - fuse.sshfs me@host: rw";
        assert_eq!(
            network_mounts(info),
            [PathBuf::from("/srv/nfs share"), PathBuf::from("/home/me/remote")],
            "nfs and sshfs; not ext4, not WSL's 9p drives"
        );
    }

    #[cfg(unix)]
    #[test]
    fn automounted_network_folders_are_never_looked_up() {
        for p in ["/net/evil-host/x.rs", "/smb/host/share/a.txt", "/afs/cell/f"] {
            assert!(!on_local_drive(Path::new(p)), "{p}");
        }
        assert!(on_local_drive(Path::new("/tmp/x.rs")));
    }
}
