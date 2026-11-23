//! 스크롤백 찾기. 줄 목록에서 검색어가 시작하는 곳을 모은다.

/// 두 글자가 대소문자를 빼면 같은가. 소문자가 여러 글자로 풀리는 글자(`İ`)도 글자 단위로 견준다.
fn same(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// `query`가 시작하는 곳을 (줄, 글자 위치)로 모두 모은다. 대소문자는 가리지 않고, 겹쳐도 센다.
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
        // 한 줄에 여럿(겹쳐도), 한글, 소문자로 바꾸면 바이트 길이가 달라지는 `İ`(2바이트 → 3바이트)가 앞에 있어도 글자 위치다.
        assert_eq!(find(&l(&["abab", "aaa"]), "ab"), vec![(0, 0), (0, 2)]);
        assert_eq!(find(&l(&["aaa"]), "aa"), vec![(0, 0), (0, 1)]);
        assert_eq!(find(&l(&["가나다 나", "İ Ab"]), "나"), vec![(0, 1), (0, 4)]);
        assert_eq!(find(&l(&["İ Ab"]), "ab"), vec![(0, 2)]);
        assert_eq!(find(&l(&["abc"]), ""), vec![]);
        assert_eq!(find(&l(&["ab"]), "abc"), vec![]);
    }
}
