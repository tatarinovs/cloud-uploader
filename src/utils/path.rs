/// Resolves a POSIX-style remote path, normalizing `.` and `..` segments,
/// stripping redundant slashes, and always anchoring to `/` so a stray `..`
/// cannot escape root (prevents path-traversal vulnerabilities).
pub fn normalize_remote_dir(input: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for part in input.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                stack.pop();
            }
            p => stack.push(p),
        }
    }
    format!("/{}", stack.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_remote_dir_keeps_dotdot_inside_root() {
        assert_eq!(normalize_remote_dir("/Upload"), "/Upload");
        assert_eq!(normalize_remote_dir("Upload/../../etc"), "/etc");
        assert_eq!(normalize_remote_dir("../../etc/passwd"), "/etc/passwd");
        assert_eq!(normalize_remote_dir("/a/./b//c/"), "/a/b/c");
        assert_eq!(normalize_remote_dir(""), "/");
        assert_eq!(normalize_remote_dir("/"), "/");
        assert_eq!(normalize_remote_dir("/Backups/../Upload/"), "/Upload");
    }
}
