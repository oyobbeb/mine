//! Scrollback search. Collects where the query starts in a list of lines.

/// Whether two chars are equal ignoring case. Chars whose lowercase expands to several chars (`İ`) are still compared char by char.
fn same(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Collects every place `query` starts as (line, char position). Case-insensitive, and overlapping matches count.
pub fn find(lines: &[String], query: &str) -> Vec<(usize, usize)> {
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        for (at, window) in chars.windows(query.len()).enumerate() {
            if query.iter().zip(window).all(|(&q, &c)| same(q, c)) {
                hits.push((i, at));
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn l(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(
            find(&l(&["Error a", "ok", "x error"]), "error"),
            vec![(0, 0), (2, 2)]
        );
    }

    #[test]
    fn find_reports_every_match_by_char_index() {
        // Several per line (overlapping too), Hangul, and `İ` whose byte length changes when lowercased (2 bytes → 3 bytes) in front: positions are still char positions.
        assert_eq!(find(&l(&["abab", "aaa"]), "ab"), vec![(0, 0), (0, 2)]);
        assert_eq!(find(&l(&["aaa"]), "aa"), vec![(0, 0), (0, 1)]);
        assert_eq!(find(&l(&["가나다 나", "İ Ab"]), "나"), vec![(0, 1), (0, 4)]);
        assert_eq!(find(&l(&["İ Ab"]), "ab"), vec![(0, 2)]);
        assert_eq!(find(&l(&["abc"]), ""), vec![]);
        assert_eq!(find(&l(&["ab"]), "abc"), vec![]);
    }
}
