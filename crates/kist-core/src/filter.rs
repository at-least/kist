//! 路徑樣式：backup 的 `--exclude`，restore 的 `--include`／`--exclude`（ADR 020）。
//!
//! 比對的對象是節點在 snapshot 裡的路徑：root 的定位切段
//! （[`crate::fsmeta::locator_components`]；本機來源就是 canonical 絕對路徑）接上 tree
//! 裡一層層的節點名——也就是整份還原時它在目標底下的位置，`kist snapshots` 的 PATHS
//! 欄位再往下。規則：
//!
//! - `/` 開頭的樣式從路徑頂端比起（錨定）；其他樣式可以從任何深度開始：`*.tmp`、
//!   `node_modules`、`build/out` 在每一層都會比對。
//! - 以 `/` 分段，每段比對恰好一個路徑元件：`*` 任意多個字元、`?` 一個字元、
//!   `[abc]`／`[a-z]`／`[!a]`（或 `[^a]`）一個字元、`\` 讓下一個字元照字面比。名稱裡
//!   不是合法 UTF-8 的地方，每個 byte 算一個字元。
//! - 整段只有 `**`：零或多個路徑元件。`**` 不能和其他字元寫在同一段。
//! - 區分大小寫。結尾的一個 `/` 會被去掉（目錄與同名的檔案一起比對）。
//! - 不支援 `!` 開頭的反向樣式（直接報錯；字面的 `!` 寫成 `\!`），也不支援 `{a,b}`
//!   （大括號是字面字元）。
//!
//! 比對逐段進行（NFA）：走訪到一個目錄時，記下每條樣式「比到第幾段」（[`FilterState`]），
//! 子項目從它往下走一步（[`PathFilter::descend`]）。這讓 restore 的 `--include` 在走訪時
//! 就知道一個目錄底下還可不可能有東西被選到，選不到的分支連 tree 都不讀。

/// 一組編譯好的樣式。空的（[`Default`]）什麼都不比對。
#[derive(Debug, Clone, Default)]
pub struct PathFilter {
    patterns: Vec<Pattern>,
}

/// 樣式寫錯：在 backup／restore 開始之前就回報，不讓一條比對不到任何東西的樣式默默生效。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid pattern {pattern:?}: {reason}")]
pub struct PatternError {
    pub pattern: String,
    pub reason: String,
}

/// 走訪到某個節點時各條樣式的進度（[`PathFilter::start`] 是路徑頂端，往下一層用
/// [`PathFilter::descend`]）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterState {
    /// (樣式, 下一個要比的段)：往下還可能比對到東西的位置。
    active: Vec<(usize, usize)>,
    /// 在這個節點比對成功的樣式（依 [`PathFilter::new`] 收到的順序編號）。
    matched: Vec<usize>,
}

#[derive(Debug, Clone)]
struct Pattern {
    /// 使用者寫的原文（「沒有比對到任何東西」的回報用）。
    text: String,
    comps: Vec<Comp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Comp {
    /// `**`：零或多個元件。
    AnyDepth,
    /// 沒有萬用字元的段：逐 byte 相等。
    Literal(Vec<u8>),
    /// 含 `*`、`?`、`[...]` 的段：比對恰好一個元件。
    Glob(Vec<Token>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Char(char),
    /// `?`
    AnyChar,
    /// `*`
    Star,
    /// `[...]`：閉區間的清單；單一字元 `c` 存成 `(c, c)`。
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

/// 名稱裡的一個字元：合法 UTF-8 的字元，或不成 UTF-8 的單一 byte。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Char(char),
    Byte(u8),
}

impl PathFilter {
    /// 編譯 `patterns`；任何一條寫錯就回 [`PatternError`]。
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<Self, PatternError> {
        let patterns = patterns
            .iter()
            .map(|p| Pattern::parse(p.as_ref()))
            .collect::<Result<_, _>>()?;
        Ok(Self { patterns })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// 樣式條數。[`FilterState::matched`] 的編號介於 `0..len()`。
    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// 第 `i` 條樣式的原文。
    pub fn pattern(&self, i: usize) -> &str {
        &self.patterns[i].text
    }

    /// 路徑頂端（還沒走過任何元件）的狀態。頂端本身不是節點，不拿來問 [`FilterState::is_match`]。
    pub fn start(&self) -> FilterState {
        let mut state = FilterState::default();
        for (p, pattern) in self.patterns.iter().enumerate() {
            state.add(p, 0, &pattern.comps);
        }
        state
    }

    /// 從目錄的狀態 `state` 往下走到它的子項目 `name`（單一路徑元件的原始 bytes）。
    pub fn descend(&self, state: &FilterState, name: &[u8]) -> FilterState {
        let mut next = FilterState::default();
        for &(p, i) in &state.active {
            let comps = &self.patterns[p].comps;
            let advanced = match &comps[i] {
                // `**` 吃掉這個元件、停在原位（它也可能吃更多）。
                Comp::AnyDepth => {
                    next.add(p, i, comps);
                    continue;
                }
                Comp::Literal(literal) => literal.as_slice() == name,
                Comp::Glob(tokens) => glob_match(tokens, name),
            };
            if advanced {
                next.add(p, i + 1, comps);
            }
        }
        next
    }
}

impl FilterState {
    /// 這個節點被某條樣式比對到。
    pub fn is_match(&self) -> bool {
        !self.matched.is_empty()
    }

    /// 在這個節點比對成功的樣式編號。
    pub fn matched(&self) -> &[usize] {
        &self.matched
    }

    /// 這個節點之下還可能有東西被比對到。false 時整棵子樹都不必再比。
    pub fn may_match_below(&self) -> bool {
        !self.active.is_empty()
    }

    /// 樣式 `p` 走到第 `i` 段，連同 `**` 可以什麼都不吃的那些位置。
    fn add(&mut self, p: usize, mut i: usize, comps: &[Comp]) {
        loop {
            if i == comps.len() {
                if !self.matched.contains(&p) {
                    self.matched.push(p);
                }
                return;
            }
            if !self.active.contains(&(p, i)) {
                self.active.push((p, i));
            }
            if comps[i] != Comp::AnyDepth {
                return;
            }
            i += 1;
        }
    }
}

impl Pattern {
    fn parse(text: &str) -> Result<Self, PatternError> {
        let fail = |reason: &str| PatternError {
            pattern: text.to_owned(),
            reason: reason.to_owned(),
        };
        if text.is_empty() {
            return Err(fail("empty pattern"));
        }
        if text.starts_with('!') {
            return Err(fail(
                "negated patterns (a leading `!`) are not supported; write `\\!` to match a literal `!`",
            ));
        }
        let (anchored, rest) = match text.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, text),
        };
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty() {
            return Err(fail("the pattern names no path component"));
        }
        let mut comps = Vec::new();
        if !anchored {
            comps.push(Comp::AnyDepth);
        }
        for segment in rest.split('/') {
            comps.push(parse_segment(segment).map_err(|reason| fail(&reason))?);
        }
        Ok(Self {
            text: text.to_owned(),
            comps,
        })
    }
}

/// 一段（兩個 `/` 之間）→ [`Comp`]。錯誤回原因。
fn parse_segment(segment: &str) -> Result<Comp, String> {
    match segment {
        "" => return Err("empty path component (`//`)".to_owned()),
        "." | ".." => {
            return Err(format!(
                "`{segment}` never matches: paths in a snapshot have no `.` or `..` components"
            ))
        }
        "**" => return Ok(Comp::AnyDepth),
        _ => {}
    }
    let mut tokens = Vec::new();
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        let token = match c {
            '\\' => Token::Char(
                chars
                    .next()
                    .ok_or_else(|| "a trailing `\\` escapes nothing".to_owned())?,
            ),
            '*' => {
                if tokens.last() == Some(&Token::Star) {
                    return Err("`**` must be a whole path component, as in `a/**/b`".to_owned());
                }
                Token::Star
            }
            '?' => Token::AnyChar,
            '[' => parse_class(&mut chars)?,
            c => Token::Char(c),
        };
        tokens.push(token);
    }
    let literal: Option<String> = tokens
        .iter()
        .map(|t| match t {
            Token::Char(c) => Some(*c),
            _ => None,
        })
        .collect();
    Ok(match literal {
        Some(s) => Comp::Literal(s.into_bytes()),
        None => Comp::Glob(tokens),
    })
}

/// `[` 之後到 `]` 為止。開頭的 `!` 或 `^` 是反向；緊接在開頭（或反向記號）之後的 `]` 是
/// 字面字元；`a-z` 是閉區間（`-` 在清單頭尾時是字面字元）；`\` 讓下一個字元照字面。
fn parse_class(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<Token, String> {
    let unclosed = || "unclosed `[`: a character class needs a closing `]`".to_owned();
    let negated = matches!(chars.peek(), Some('!' | '^'));
    if negated {
        chars.next();
    }
    let mut ranges = Vec::new();
    let mut first = true;
    loop {
        let lo = match chars.next().ok_or_else(unclosed)? {
            ']' if !first => break,
            '\\' => chars.next().ok_or_else(unclosed)?,
            c => c,
        };
        first = false;
        // `-` 之後還有 `]` 以外的字元才是區間；否則這個 `-` 下一輪當字面字元處理。
        let is_range = chars.peek() == Some(&'-') && chars.clone().nth(1).is_some_and(|c| c != ']');
        let hi = if is_range {
            chars.next();
            match chars.next().ok_or_else(unclosed)? {
                '\\' => chars.next().ok_or_else(unclosed)?,
                c => c,
            }
        } else {
            lo
        };
        if hi < lo {
            return Err(format!("invalid range `{lo}-{hi}` in a character class"));
        }
        ranges.push((lo, hi));
    }
    Ok(Token::Class { negated, ranges })
}

impl Token {
    /// 單一字元的 token 對 `unit`（`Star` 由 [`glob_match`] 自己處理，這裡回 false）。
    fn matches(&self, unit: Unit) -> bool {
        match (self, unit) {
            (Token::AnyChar, _) => true,
            (Token::Char(c), Unit::Char(u)) => *c == u,
            (Token::Class { negated, ranges }, Unit::Char(u)) => {
                ranges.iter().any(|&(lo, hi)| lo <= u && u <= hi) != *negated
            }
            // 不成 UTF-8 的 byte 不等於任何字面字元，只會被反向清單選到。
            (Token::Class { negated, .. }, Unit::Byte(_)) => *negated,
            (Token::Char(_), Unit::Byte(_)) | (Token::Star, _) => false,
        }
    }
}

/// `name[at]` 起的一個字元與它的 byte 長度（`at < name.len()`）。
fn unit_at(name: &[u8], at: usize) -> (Unit, usize) {
    let lead = name[at];
    let len = match lead {
        0x00..=0x7F => return (Unit::Char(char::from(lead)), 1),
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return (Unit::Byte(lead), 1),
    };
    match name
        .get(at..at + len)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|s| s.chars().next())
    {
        Some(c) => (Unit::Char(c), len),
        None => (Unit::Byte(lead), 1),
    }
}

/// 一段的 token 是否比對整個 `name`。`*` 以「記住最後一個 `*`、失敗時讓它多吃一個字元」
/// 回溯：其他 token 都恰好吃一個字元，所以只需要回到最後一個 `*`，時間是
/// O(token 數 × 名稱長度)。
fn glob_match(tokens: &[Token], name: &[u8]) -> bool {
    let (mut t, mut at) = (0, 0);
    // (`*` 之後的 token 位置, `*` 目前吃到的 byte 位置)
    let mut star: Option<(usize, usize)> = None;
    while at < name.len() {
        let (unit, len) = unit_at(name, at);
        match tokens.get(t) {
            Some(Token::Star) => {
                star = Some((t + 1, at));
                t += 1;
                continue;
            }
            Some(token) if token.matches(unit) => {
                t += 1;
                at += len;
                continue;
            }
            _ => {}
        }
        let Some((after, star_at)) = star else {
            return false;
        };
        let (_, absorbed) = unit_at(name, star_at);
        star = Some((after, star_at + absorbed));
        t = after;
        at = star_at + absorbed;
    }
    tokens[t..].iter().all(|token| *token == Token::Star)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// 從頂端沿 `path`（`/` 分隔，開頭的 `/` 可有可無）走到底的狀態。
    fn walk(filter: &PathFilter, path: &[u8]) -> FilterState {
        let mut state = filter.start();
        for name in path.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            state = filter.descend(&state, name);
        }
        state
    }

    fn matches(pattern: &str, path: &[u8]) -> bool {
        walk(&PathFilter::new(&[pattern]).unwrap(), path).is_match()
    }

    fn reason(pattern: &str) -> String {
        PathFilter::new(&[pattern]).unwrap_err().reason
    }

    #[test]
    fn unanchored_patterns_match_at_any_depth() {
        assert!(matches("node_modules", b"/home/me/p/node_modules"));
        assert!(matches("node_modules", b"/node_modules"));
        assert!(!matches("node_modules", b"/home/me/node_modules2"));
        assert!(matches("*.tmp", b"/a/b/x.tmp"));
        assert!(matches("*.tmp", b"/a/.hidden.tmp"), "`*` 也吃開頭的點");
        assert!(
            !matches("*.tmp", b"/a/x.tmp/y"),
            "比對的是整個元件，不是子字串"
        );
        assert!(matches("build/out", b"/src/build/out"));
        assert!(!matches("build/out", b"/src/build/x/out"));
    }

    #[test]
    fn anchored_patterns_match_from_the_top() {
        assert!(matches("/home/me/.cache", b"/home/me/.cache"));
        assert!(!matches("/home/me/.cache", b"/x/home/me/.cache"));
        assert!(
            !matches("/home/me", b"/home/me/.cache"),
            "樣式比對節點本身，不是它的子孫"
        );
        assert!(matches("/home/*/.cache", b"/home/you/.cache"));
        assert!(
            !matches("/home/*/.cache", b"/home/a/b/.cache"),
            "`*` 不跨 `/`"
        );
    }

    #[test]
    fn double_star_spans_zero_or_more_components() {
        assert!(matches("/a/**/z", b"/a/z"));
        assert!(matches("/a/**/z", b"/a/b/c/z"));
        assert!(
            matches("/a/**", b"/a"),
            "零個元件：`/a/**` 也比對 `/a` 本身"
        );
        assert!(matches("/a/**", b"/a/b/c"));
        assert!(matches("**/z", b"/z"));
        assert!(matches("/**", b"/anything/at/all"));
        assert!(!matches("/a/**/z", b"/b/z"));
    }

    #[test]
    fn question_mark_and_classes_take_one_character() {
        assert!(matches("?.txt", b"/a/x.txt"));
        assert!(
            matches("?.txt", "/a/文.txt".as_bytes()),
            "一個 CJK 字元（3 bytes）"
        );
        assert!(matches("??.txt", "/a/中文.txt".as_bytes()));
        assert!(!matches("?.txt", "/a/中文.txt".as_bytes()));
        assert!(
            matches("?.txt", b"/a/\xff.txt"),
            "不成 UTF-8 的 byte 算一個字元"
        );
        assert!(matches("[ab].c", b"/b.c"));
        assert!(!matches("[ab].c", b"/x.c"));
        assert!(matches("[a-c]x", b"/bx"));
        assert!(matches("[!a]x", b"/bx"));
        assert!(matches("[^a]x", b"/bx"));
        assert!(!matches("[!a]x", b"/ax"));
        assert!(matches("[]a]", b"/]"), "開頭的 `]` 是字面字元");
        assert!(matches("[!]]", b"/a"));
        assert!(matches("[a-]", b"/-"), "結尾的 `-` 是字面字元");
        assert!(matches("[中文]", "/文".as_bytes()));
        assert!(
            matches("[!a]", b"/\xff"),
            "不成 UTF-8 的 byte 只被反向清單選到"
        );
        assert!(!matches("[a\\]]", b"/\xff"));
    }

    #[test]
    fn escapes_and_odd_names() {
        assert!(matches("a\\*b", b"/x/a*b"));
        assert!(!matches("a\\*b", b"/x/aXb"));
        assert!(matches("\\!keep", b"/!keep"));
        assert!(matches("{a,b}", b"/{a,b}"), "大括號是字面字元");
        assert!(
            matches("*.tmp", b"/a\nb/x\ny.tmp"),
            "換行只是名稱裡的一個字元"
        );
        assert!(matches("node_modules", b"/\xfe\xff/node_modules"));
        assert!(matches("/中文/*", "/中文/檔案".as_bytes()));
        assert!(!matches("Foo", b"/foo"), "區分大小寫");
    }

    #[test]
    fn a_trailing_slash_is_dropped() {
        assert!(matches("/home/me/.cache/", b"/home/me/.cache"));
        assert!(matches("build/", b"/p/build"));
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        assert_eq!(reason(""), "empty pattern");
        assert!(reason("/").contains("names no path component"));
        assert!(reason("a//b").contains("empty path component"));
        assert!(reason("/a/../b").contains("never matches"));
        assert!(reason("./build").contains("never matches"));
        assert!(reason("!keep").contains("not supported"));
        assert!(reason("a**").contains("whole path component"));
        assert!(reason("/a/b**c").contains("whole path component"));
        assert!(reason("[abc").contains("unclosed"));
        assert!(reason("[!]").contains("unclosed"));
        assert!(reason("[z-a]").contains("invalid range"));
        assert!(reason("abc\\").contains("escapes nothing"));
        let err = PathFilter::new(&["ok", "[bad"]).unwrap_err();
        assert_eq!(err.pattern, "[bad", "錯誤要點名寫錯的那一條");
    }

    #[test]
    fn states_report_which_patterns_matched_and_what_can_still_match() {
        let f = PathFilter::new(&["/a/b", "*.txt", "/a/**/c"]).unwrap();
        let a = f.descend(&f.start(), b"a");
        assert!(!a.is_match());
        assert!(a.may_match_below());
        let ab = f.descend(&a, b"b");
        assert_eq!(ab.matched(), &[0]);
        let x = f.descend(&f.descend(&f.start(), b"x"), b"n.txt");
        assert_eq!(x.matched(), &[1]);
        // 只剩錨定樣式時，走出它們的前綴就沒有東西能比對了。
        let anchored = PathFilter::new(&["/a/b/c", "/a/x"]).unwrap();
        let other = anchored.descend(&anchored.start(), b"z");
        assert!(!other.may_match_below());
        let ab = anchored.descend(&anchored.descend(&anchored.start(), b"a"), b"b");
        assert!(ab.may_match_below());
        let abc = anchored.descend(&ab, b"c");
        assert!(abc.is_match());
        assert!(!abc.may_match_below());
        // 沒有樣式：什麼都不比對，也不往下比。
        let empty = PathFilter::default();
        assert!(!empty.start().may_match_below());
        assert!(!empty.descend(&empty.start(), b"a").is_match());
    }

    // --- 與天真的遞迴參考實作比對（proptest）---

    /// 名稱 → 字元序列，以 std 的 utf8_chunks 解碼（與 `unit_at` 是兩套獨立的寫法）。
    fn units(name: &[u8]) -> Vec<Unit> {
        let mut out = Vec::new();
        for chunk in name.utf8_chunks() {
            out.extend(chunk.valid().chars().map(Unit::Char));
            out.extend(chunk.invalid().iter().map(|&b| Unit::Byte(b)));
        }
        out
    }

    fn ref_glob(tokens: &[Token], units: &[Unit]) -> bool {
        match tokens.split_first() {
            None => units.is_empty(),
            Some((Token::Star, rest)) => (0..=units.len()).any(|k| ref_glob(rest, &units[k..])),
            Some((token, rest)) => {
                !units.is_empty() && token.matches(units[0]) && ref_glob(rest, &units[1..])
            }
        }
    }

    fn ref_path(comps: &[Comp], path: &[Vec<u8>]) -> bool {
        match comps.split_first() {
            None => path.is_empty(),
            Some((Comp::AnyDepth, rest)) => (0..=path.len()).any(|k| ref_path(rest, &path[k..])),
            Some((comp, rest)) => {
                let one = match (comp, path.first()) {
                    (Comp::Literal(literal), Some(name)) => literal == name,
                    (Comp::Glob(tokens), Some(name)) => ref_glob(tokens, &units(name)),
                    _ => false,
                };
                one && ref_path(rest, &path[1..])
            }
        }
    }

    fn segment() -> impl Strategy<Value = String> {
        let atom = prop::sample::select(vec![
            "a", "b", "ä", "中", "*", "?", "[ab]", "[!a]", "[a-c]", "\\*", "[中]", "[!中]", "[!ä]",
        ]);
        prop_oneof![
            4 => prop::collection::vec(atom, 1..4).prop_map(|atoms| {
                // `**` 只能整段：相鄰的兩個 `*` 併成一個。
                let mut s = String::new();
                for a in atoms {
                    if !(a == "*" && s.ends_with('*') && !s.ends_with("\\*")) {
                        s.push_str(a);
                    }
                }
                s
            }),
            1 => Just("**".to_owned()),
        ]
    }

    fn pattern() -> impl Strategy<Value = String> {
        (any::<bool>(), prop::collection::vec(segment(), 1..4)).prop_map(|(anchored, segs)| {
            format!("{}{}", if anchored { "/" } else { "" }, segs.join("/"))
        })
    }

    fn path() -> impl Strategy<Value = Vec<Vec<u8>>> {
        let name = prop::sample::select(vec![
            b"a".to_vec(),
            b"b".to_vec(),
            b"ab".to_vec(),
            "ä".as_bytes().to_vec(),
            "中".as_bytes().to_vec(),
            "a中".as_bytes().to_vec(),
            b"*".to_vec(),
            b"\xff".to_vec(),
            b"a\xffb".to_vec(),
        ]);
        prop::collection::vec(name, 0..6)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// 逐段的狀態與參考實作對每一個前綴都同意；而且整條路徑比對到時，每個真前綴都還
        /// `may_match_below`——restore 依它剪枝，不能剪掉會被選到的東西。
        #[test]
        fn stepwise_matching_agrees_with_the_reference(
            texts in prop::collection::vec(pattern(), 1..4),
            path in path(),
        ) {
            let filter = PathFilter::new(&texts).unwrap();
            let mut states = vec![filter.start()];
            for name in &path {
                let next = filter.descend(states.last().unwrap(), name);
                states.push(next);
            }
            for (depth, state) in states.iter().enumerate().skip(1) {
                let prefix = &path[..depth];
                for (p, pat) in filter.patterns.iter().enumerate() {
                    let want = ref_path(&pat.comps, prefix);
                    prop_assert_eq!(state.matched().contains(&p), want, "{} vs {:?}", pat.text, prefix);
                    if want {
                        for (k, earlier) in states[..depth].iter().enumerate() {
                            prop_assert!(earlier.active.iter().any(|&(q, _)| q == p),
                                "{} matches {:?} but was pruned at depth {}", pat.text, prefix, k);
                        }
                    }
                }
            }
        }
    }
}
