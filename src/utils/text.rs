/// Parses a list file (such as `urls.txt`): one entry per line, blank lines
/// and `#` comments are ignored, a UTF-8 BOM is tolerated.
pub fn parse_list(text: &str) -> Vec<String> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_comments_and_bom() {
        let text = "\u{feff}# header\r\nhttps://a\r\n\r\n  https://b  \n#x\n";
        assert_eq!(parse_list(text), vec!["https://a", "https://b"]);
    }
}
