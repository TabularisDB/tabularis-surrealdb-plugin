//! Reading the `DEFINE …` statements returned by `INFO FOR DB` and
//! `INFO FOR TABLE`.
//!
//! Both statements return maps of `name → DEFINE statement` on SurrealDB 2.x
//! and 3.x. That text form is the documented, stable output; the alternative
//! `INFO … STRUCTURE` form is documented as internal and "subject to change
//! without notice" (its keys already differ between releases), so the
//! plugin reads the statements instead. Only the clauses Tabularis can show
//! are extracted; everything else is ignored.

/// The parts of `DEFINE TABLE` the plugin uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDef {
    pub schemafull: bool,
    /// `TYPE RELATION`: a graph edge table with `in` / `out` fields.
    pub relation: bool,
    /// `DEFINE TABLE … AS SELECT …`: a pre-computed table view.
    pub view: bool,
    pub comment: Option<String>,
}

/// The parts of `DEFINE FIELD` the plugin uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldDef {
    /// The `TYPE` clause as written, e.g. `option<int>` or `none | int`.
    pub kind: Option<String>,
    pub default: Option<String>,
    pub readonly: bool,
    /// `VALUE` or `COMPUTED`: the server derives the value.
    pub computed: bool,
    pub comment: Option<String>,
}

/// The parts of `DEFINE INDEX` the plugin uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDef {
    pub columns: Vec<String>,
    pub unique: bool,
    pub comment: Option<String>,
}

const TABLE_CLAUSES: &[&str] = &[
    "TYPE",
    "SCHEMAFULL",
    "SCHEMALESS",
    "DROP",
    "CHANGEFEED",
    "PERMISSIONS",
    "COMMENT",
    "AS",
];

const FIELD_CLAUSES: &[&str] = &[
    "ON",
    "TYPE",
    "FLEXIBLE",
    "REFERENCE",
    "DEFAULT",
    "READONLY",
    "VALUE",
    "COMPUTED",
    "ASSERT",
    "PERMISSIONS",
    "COMMENT",
];

const INDEX_CLAUSES: &[&str] = &[
    "ON",
    "FIELDS",
    "COLUMNS",
    "UNIQUE",
    "COUNT",
    "SEARCH",
    "FULLTEXT",
    "MTREE",
    "HNSW",
    "CONCURRENTLY",
    "DEFER",
    "COMMENT",
];

pub fn parse_table(def: &str) -> TableDef {
    let clauses = clauses(def, TABLE_CLAUSES);
    let mut out = TableDef::default();
    for (keyword, value) in &clauses {
        match keyword.as_str() {
            "SCHEMAFULL" => out.schemafull = true,
            "SCHEMALESS" => out.schemafull = false,
            "TYPE" => {
                out.relation = value
                    .split_whitespace()
                    .next()
                    .is_some_and(|w| w.eq_ignore_ascii_case("RELATION"))
            }
            "AS" => out.view = true,
            "COMMENT" => out.comment = unquote_string(value),
            _ => {}
        }
    }
    out
}

pub fn parse_field(def: &str) -> FieldDef {
    let mut out = FieldDef::default();
    for (keyword, value) in clauses(def, FIELD_CLAUSES) {
        match keyword.as_str() {
            "TYPE" => out.kind = non_empty(value),
            "DEFAULT" => {
                let value = value.trim();
                let value = strip_keyword(value, "ALWAYS").unwrap_or(value);
                out.default = non_empty(value.to_string());
            }
            "READONLY" => out.readonly = true,
            "VALUE" | "COMPUTED" => out.computed = true,
            "COMMENT" => out.comment = unquote_string(&value),
            _ => {}
        }
    }
    out
}

pub fn parse_index(def: &str) -> IndexDef {
    let mut out = IndexDef::default();
    for (keyword, value) in clauses(def, INDEX_CLAUSES) {
        match keyword.as_str() {
            "FIELDS" | "COLUMNS" => {
                out.columns = split_top_level_commas(&value)
                    .into_iter()
                    .map(|c| unquote_identifier(c.trim()))
                    .filter(|c| !c.is_empty())
                    .collect();
            }
            "UNIQUE" => out.unique = true,
            "COMMENT" => out.comment = unquote_string(&value),
            _ => {}
        }
    }
    out
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let head = s.get(..keyword.len())?;
    let rest = &s[keyword.len()..];
    (head.eq_ignore_ascii_case(keyword) && rest.starts_with(char::is_whitespace))
        .then(|| rest.trim_start())
}

/// Words that can precede the defined object's name.
const PREFIX_WORDS: &[&str] = &[
    "DEFINE",
    "TABLE",
    "FIELD",
    "INDEX",
    "OVERWRITE",
    "IF",
    "NOT",
    "EXISTS",
];

/// Token positions needed by the clause splitter.
#[derive(Debug)]
enum Tok {
    Word { upper: String, start: usize },
    Other,
}

/// Split a statement into `(KEYWORD, text up to the next keyword)` pairs,
/// considering only keywords at bracket depth 0. Inside a `TYPE` clause
/// angle brackets count as brackets too, so `record<value>` does not end it.
fn clauses(def: &str, keywords: &[&str]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, usize)> = None;
    let mut depth = 0usize;
    let mut angle = 0usize;
    let mut prev_char: Option<char> = None;
    // Clause keywords only count after the statement's own name, so a field
    // called `value` or a table called `type` is not mistaken for a clause.
    let mut name_seen = false;

    for (tok, end, ch) in Tokens::new(def) {
        if !name_seen {
            match &tok {
                Tok::Word { upper, .. } if PREFIX_WORDS.contains(&upper.as_str()) => {}
                _ => name_seen = true,
            }
            prev_char = Some('a');
            continue;
        }
        match tok {
            Tok::Word { upper, start } => {
                let in_type = current.as_ref().is_some_and(|(k, _)| k == "TYPE");
                let is_member = matches!(prev_char, Some(':' | '.' | '$'));
                if depth == 0
                    && (!in_type || angle == 0)
                    && !is_member
                    && keywords.contains(&upper.as_str())
                {
                    if let Some((k, from)) = current.take() {
                        out.push((k, def[from..start].trim().to_string()));
                    }
                    angle = 0;
                    current = Some((upper, end));
                }
                prev_char = Some('a');
            }
            Tok::Other => {
                let in_type = current.as_ref().is_some_and(|(k, _)| k == "TYPE");
                match ch {
                    Some('(' | '[' | '{') => depth += 1,
                    Some(')' | ']' | '}') => depth = depth.saturating_sub(1),
                    Some('<') if in_type => angle += 1,
                    Some('>') if in_type => angle = angle.saturating_sub(1),
                    _ => {}
                }
                if ch.is_some() {
                    prev_char = ch;
                }
            }
        }
    }
    if let Some((k, from)) = current {
        out.push((k, def[from..].trim().to_string()));
    }
    out
}

/// Minimal tokenizer: words, quoted runs (skipped as one token) and single
/// punctuation characters. Yields `(token, end offset, punctuation char)`.
struct Tokens<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Tokens<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }
}

impl Iterator for Tokens<'_> {
    type Item = (Tok, usize, Option<char>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let ch = self.src[self.pos..].chars().next()?;
            let start = self.pos;
            if ch.is_whitespace() {
                self.pos += ch.len_utf8();
                continue;
            }
            if ch.is_alphanumeric() || ch == '_' {
                let len = self.src[start..]
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(self.src.len() - start);
                self.pos += len;
                let upper = self.src[start..self.pos].to_ascii_uppercase();
                return Some((Tok::Word { upper, start }, self.pos, None));
            }
            let close = match ch {
                '\'' | '"' | '`' => Some(ch),
                '⟨' => Some('⟩'),
                _ => None,
            };
            self.pos += ch.len_utf8();
            if let Some(close) = close {
                while let Some(c) = self.src[self.pos..].chars().next() {
                    self.pos += c.len_utf8();
                    if c == '\\' {
                        if let Some(n) = self.src[self.pos..].chars().next() {
                            self.pos += n.len_utf8();
                        }
                    } else if c == close {
                        break;
                    }
                }
                return Some((Tok::Other, self.pos, Some('q')));
            }
            return Some((Tok::Other, self.pos, Some(ch)));
        }
    }
}

/// Split on commas that are not nested in brackets or quotes.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut last = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '\'' | '"' | '`' | '⟨' => {
                let close = if c == '⟨' { '⟩' } else { c };
                while let Some((_, n)) = chars.next() {
                    if n == '\\' {
                        chars.next();
                    } else if n == close {
                        break;
                    }
                }
            }
            ',' if depth == 0 => {
                parts.push(&s[last..i]);
                last = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[last..]);
    parts
}

/// Remove backtick or `⟨⟩` quoting from an identifier path segment-wise is
/// not needed for display; only a fully quoted name is unquoted.
fn unquote_identifier(s: &str) -> String {
    if let Some(inner) = s.strip_prefix('`').and_then(|r| r.strip_suffix('`')) {
        return unescape(inner);
    }
    if let Some(inner) = s.strip_prefix('⟨').and_then(|r| r.strip_suffix('⟩')) {
        return unescape(inner);
    }
    s.to_string()
}

/// Read a SurrealQL string literal (`'…'` or `"…"`, optionally `s`-prefixed).
pub fn unquote_string(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s.strip_prefix('s').unwrap_or(s);
    let inner = s
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .or_else(|| s.strip_prefix('"').and_then(|r| r.strip_suffix('"')))?;
    Some(unescape(inner))
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Statements below are copied from SurrealDB 3.2.3 `INFO FOR …` output,
    // plus 2.x spellings (FLEXIBLE before TYPE, VALUE <future>, SEARCH).

    #[test]
    fn tables() {
        let t = parse_table(
            "DEFINE TABLE person TYPE ANY SCHEMAFULL COMMENT 'People' PERMISSIONS NONE",
        );
        assert_eq!(
            t,
            TableDef {
                schemafull: true,
                relation: false,
                view: false,
                comment: Some("People".into())
            }
        );
        let r = parse_table(
            "DEFINE TABLE knows TYPE RELATION IN person OUT person SCHEMALESS PERMISSIONS NONE",
        );
        assert!(r.relation && !r.schemafull);
        let odd = parse_table("DEFINE TABLE `odd table` TYPE ANY SCHEMALESS PERMISSIONS NONE");
        assert!(!odd.schemafull && !odd.relation);
        let view = parse_table(
            "DEFINE TABLE adults TYPE NORMAL SCHEMALESS AS SELECT * FROM person WHERE age >= 18 PERMISSIONS NONE",
        );
        assert!(view.view);
        let c = parse_table(r#"DEFINE TABLE t SCHEMAFULL COMMENT "it's \"quoted\"""#);
        assert_eq!(c.comment.as_deref(), Some("it's \"quoted\""));
    }

    #[test]
    fn fields() {
        let f = parse_field("DEFINE FIELD age ON person TYPE none | int PERMISSIONS FULL");
        assert_eq!(f.kind.as_deref(), Some("none | int"));
        assert!(!f.readonly && !f.computed);

        let f = parse_field(
            "DEFINE FIELD tags ON person TYPE array<string> DEFAULT [] PERMISSIONS FULL",
        );
        assert_eq!(f.kind.as_deref(), Some("array<string>"));
        assert_eq!(f.default.as_deref(), Some("[]"));

        let f = parse_field(
            "DEFINE FIELD owner ON doc TYPE option<record<value>> DEFAULT ALWAYS NONE READONLY COMMENT 'who' PERMISSIONS FULL",
        );
        assert_eq!(f.kind.as_deref(), Some("option<record<value>>"));
        assert_eq!(f.default.as_deref(), Some("NONE"));
        assert!(f.readonly);
        assert_eq!(f.comment.as_deref(), Some("who"));

        let f = parse_field("DEFINE FIELD meta ON t FLEXIBLE TYPE object");
        assert_eq!(f.kind.as_deref(), Some("object"));

        let f = parse_field("DEFINE FIELD updated ON t VALUE <future> { time::now() } ASSERT $value < d'2100-01-01' COMMENT 'x'");
        assert!(f.computed);
        assert_eq!(f.kind, None);
        assert_eq!(f.comment.as_deref(), Some("x"));

        let f = parse_field("DEFINE FIELD full ON t COMPUTED string::concat(first, ' ', last)");
        assert!(f.computed);
    }

    #[test]
    fn indexes() {
        let i = parse_index("DEFINE INDEX person_name ON person FIELDS name UNIQUE");
        assert_eq!(i.columns, vec!["name"]);
        assert!(i.unique);
        let i = parse_index("DEFINE INDEX person_city_age ON person FIELDS address.city, age");
        assert_eq!(i.columns, vec!["address.city", "age"]);
        assert!(!i.unique);
        let i = parse_index(
            "DEFINE INDEX ft ON doc FIELDS body SEARCH ANALYZER simple BM25(1.2,0.75) HIGHLIGHTS",
        );
        assert_eq!(i.columns, vec!["body"]);
        let i = parse_index("DEFINE INDEX q ON t COLUMNS `odd field`, ⟨other⟩ COMMENT 'c'");
        assert_eq!(i.columns, vec!["odd field", "other"]);
        assert_eq!(i.comment.as_deref(), Some("c"));
    }

    #[test]
    fn names_that_look_like_keywords() {
        let f = parse_field("DEFINE FIELD value ON t TYPE int");
        assert!(!f.computed);
        assert_eq!(f.kind.as_deref(), Some("int"));
        let f = parse_field("DEFINE FIELD address.type ON t TYPE string");
        assert_eq!(f.kind.as_deref(), Some("string"));
        let t = parse_table("DEFINE TABLE OVERWRITE type SCHEMAFULL");
        assert!(t.schemafull && !t.relation);
        let t = parse_table("DEFINE TABLE `comment` SCHEMAFULL");
        assert!(t.schemafull && t.comment.is_none());
    }

    #[test]
    fn keywords_inside_strings_and_brackets_are_ignored() {
        let f = parse_field("DEFINE FIELD x ON t TYPE string DEFAULT 'COMMENT READONLY' ASSERT string::len($value) > 0");
        assert_eq!(f.default.as_deref(), Some("'COMMENT READONLY'"));
        assert!(!f.readonly);
        assert_eq!(f.comment, None);
    }

    #[test]
    fn unquotes_strings() {
        assert_eq!(unquote_string("'a\\'b'").as_deref(), Some("a'b"));
        assert_eq!(unquote_string("s\"x\"").as_deref(), Some("x"));
        assert_eq!(unquote_string("plain"), None);
    }
}
