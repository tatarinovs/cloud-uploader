use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

/// Everything except RFC 3986 "unreserved" characters gets percent-encoded.
/// This is what AWS SigV4 requires and what every WebDAV server accepts.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encodes a single path segment or query component.
pub fn encode_component(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

/// Percent-encodes every segment of a `/`-separated path, keeping the separators.
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_component)
        .collect::<Vec<_>>()
        .join("/")
}

/// Resolves a POSIX-style remote path, normalizing `.` and `..` segments,
/// treating `\` as a separator, stripping redundant slashes, and always
/// anchoring to `/` so a stray `..` cannot escape root.
pub fn normalize_remote_dir(input: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for part in input.split(['/', '\\']) {
        match part.trim() {
            "" | "." => continue,
            ".." => {
                stack.pop();
            }
            _ => stack.push(part),
        }
    }
    format!("/{}", stack.join("/"))
}

/// Joins a remote directory and a relative path into a normalized absolute path.
pub fn join_remote(dir: &str, name: &str) -> String {
    normalize_remote_dir(&format!("{dir}/{name}"))
}

/// Splits a remote path into its normalized parent directory and final name.
pub fn split_remote(path: &str) -> (String, String) {
    let path = normalize_remote_dir(path);
    match path.rsplit_once('/') {
        Some(("", name)) => ("/".to_string(), name.to_string()),
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => ("/".to_string(), String::new()),
    }
}

/// Makes an arbitrary string safe to use as a single file name on any
/// platform and in any cloud: no separators, reserved or control characters.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "_".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_dotdot_inside_root() {
        assert_eq!(normalize_remote_dir("/Upload"), "/Upload");
        assert_eq!(normalize_remote_dir("Upload/../../etc"), "/etc");
        assert_eq!(normalize_remote_dir("../../etc/passwd"), "/etc/passwd");
        assert_eq!(normalize_remote_dir("/a/./b//c/"), "/a/b/c");
        assert_eq!(normalize_remote_dir(""), "/");
        assert_eq!(normalize_remote_dir("/"), "/");
        assert_eq!(normalize_remote_dir("/Backups/../Upload/"), "/Upload");
        assert_eq!(normalize_remote_dir("\\Backups\\Daily"), "/Backups/Daily");
    }

    #[test]
    fn join_and_split() {
        assert_eq!(join_remote("/", "a.txt"), "/a.txt");
        assert_eq!(join_remote("/Upload/", "/sub/a.txt"), "/Upload/sub/a.txt");
        assert_eq!(
            split_remote("/Upload/a b.txt"),
            ("/Upload".into(), "a b.txt".into())
        );
        assert_eq!(split_remote("/a.txt"), ("/".into(), "a.txt".into()));
    }

    #[test]
    fn encoding_is_strict() {
        assert_eq!(
            encode_component("[v1.0] my app~_-.apk"),
            "%5Bv1.0%5D%20my%20app~_-.apk"
        );
        assert_eq!(encode_component("a/b+c"), "a%2Fb%2Bc");
        assert_eq!(
            encode_path("Папка/x y"),
            "%D0%9F%D0%B0%D0%BF%D0%BA%D0%B0/x%20y"
        );
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_file_name("release/1.0"), "release_1.0");
        assert_eq!(sanitize_file_name("a:b*c?.zip"), "a_b_c_.zip");
        assert_eq!(sanitize_file_name(".."), "_");
        assert_eq!(sanitize_file_name("  name. "), "name");
        assert_eq!(sanitize_file_name("Файл.txt"), "Файл.txt");
    }
}
