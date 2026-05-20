/*
lexer.rs: turn raw ASL source text into a stream of tokens.

phase 4.1 / 4.2 of plans.txt. the lexer is hand-rolled, no external crates. its
job is to chop a query string into the smallest meaningful pieces (keywords,
literals, operators, punctuation) so the parser can work on a clean stream
instead of raw bytes.

design notes:
  - one Token enum with flat variants per keyword. could have used a separate
    Keyword sub-enum, but flat is fine for ~40 keywords and the parser matches
    on these constantly, so terser pattern matching wins.
  - Spanned wraps each Token with the line/column where it started. cheap to
    carry around and lets the parser produce errors that point at the right
    spot in the source.
  - source is held as &str + a &[u8] view. ASL keywords and operators are all
    ASCII, so we index the byte slice for speed. string literals can contain
    UTF-8, and we step through those codepoint-by-codepoint via the &str.
  - we do NOT collapse "not in" or "create index" into single tokens here. the
    parser handles those multi-word forms. keeping the lexer dumb means we can
    re-use these keywords elsewhere without surprise (e.g. "not" also appears
    standalone as a logical operator).
*/

use std::fmt;

/*
Token: every lexeme the lexer can produce.

literals carry their decoded value, identifiers carry the name. everything else
is unit-like because the keyword name IS the information.
*/
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    // keywords (pipeline stages and statement openers)
    From,
    Where,
    Select,
    Order,
    Limit,
    Offset,
    Group,
    Join,
    Insert,
    Update,
    Delete,
    Drop,
    Create,
    Index,
    On,
    Set,
    As,
    In,
    Exists,
    Missing,
    Contains,
    Starts,
    Ends,
    Use,
    Asc,
    Desc,
    And,
    Or,
    Not,
    If,
    Then,
    Else,
    True,
    False,
    Null,
    Required,
    Unique,
    Any,

    // literals
    IntLit(i64),
    FloatLit(f64),
    StringLit(String),
    Ident(String),

    // operators
    EqEq,    // ==
    BangEq,  // !=
    Gt,      // >
    GtEq,    // >=
    Lt,      // <
    LtEq,    // <=
    Plus,    // +
    Minus,   // -
    Star,    // *
    Slash,   // /
    Percent, // %
    Pipe,    // |
    Eq,      // =  (used by update set)

    // punctuation
    LBrace,   // {
    RBrace,   // }
    LBracket, // [
    RBracket, // ]
    LParen,   // (
    RParen,   // )
    Comma,    // ,
    Colon,    // :
    Dot,      // .
}

/*
Spanned: a Token plus where it started in the source.

we track (line, col) instead of a byte offset because human-readable errors
want "line 3, column 12" not "byte 42". line and col both start at 1, matching
how editors number things.
*/
#[derive(Clone, Debug, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub line: usize,
    pub col: usize,
}

/*
LexError: things that can go wrong while lexing.

all variants carry position info so the caller can format a useful message. we
keep these as plain data so callers can match on them (e.g. tests can assert
"this should fail with UnterminatedString").
*/
#[derive(Clone, Debug, PartialEq)]
pub enum LexError {
    UnexpectedChar { ch: char, line: usize, col: usize },
    UnterminatedString { line: usize, col: usize },
    InvalidNumber { text: String, line: usize, col: usize },
    InvalidEscape { ch: char, line: usize, col: usize },
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexError::UnexpectedChar { ch, line, col } => {
                write!(f, "unexpected character {ch:?} at line {line}, column {col}")
            }
            LexError::UnterminatedString { line, col } => {
                write!(f, "unterminated string starting at line {line}, column {col}")
            }
            LexError::InvalidNumber { text, line, col } => {
                write!(f, "invalid number literal {text:?} at line {line}, column {col}")
            }
            LexError::InvalidEscape { ch, line, col } => {
                write!(f, "invalid escape sequence \\{ch} at line {line}, column {col}")
            }
        }
    }
}

impl std::error::Error for LexError {}

/*
tokenize: public entry point. wraps a Lexer and returns the full token vector.

we return Vec<Spanned>, not an iterator. an iterator would be more memory
efficient on huge inputs, but queries are tiny (a few hundred tokens at most)
and Vec makes the parser much simpler since it can peek arbitrarily ahead.
*/
pub fn tokenize(input: &str) -> Result<Vec<Spanned>, LexError> {
    let mut lexer = Lexer::new(input);
    lexer.tokenize()
}

/*
Lexer: cursor over the source.

bytes is just src.as_bytes() cached so we don't recompute it every step. pos
indexes into both. line and col are 1-based for human display.
*/
struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    /*
    bump: advance one byte and update line/col bookkeeping.

    we only call this for ASCII bytes outside string literals, OR for the
    single-byte ASCII parts of strings. multi-byte UTF-8 inside a string is
    advanced via bump_char so the col count tracks codepoints not bytes.
    */
    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    /*
    bump_char: advance one UTF-8 codepoint and treat it as one column.

    used inside string literals where the body can contain any UTF-8. for
    column counting we treat each char as width 1, which is wrong for wide
    glyphs but matches what most editors show.
    */
    fn bump_char(&mut self) -> Option<char> {
        let ch = self.src[self.pos..].chars().next()?;
        let n = ch.len_utf8();
        self.pos += n;
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn tokenize(&mut self) -> Result<Vec<Spanned>, LexError> {
        let mut out = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let line = self.line;
            let col = self.col;
            let Some(b) = self.peek() else { break };

            let token = match b {
                b'{' => { self.bump(); Token::LBrace }
                b'}' => { self.bump(); Token::RBrace }
                b'[' => { self.bump(); Token::LBracket }
                b']' => { self.bump(); Token::RBracket }
                b'(' => { self.bump(); Token::LParen }
                b')' => { self.bump(); Token::RParen }
                b',' => { self.bump(); Token::Comma }
                b':' => { self.bump(); Token::Colon }
                b'.' => { self.bump(); Token::Dot }
                b'+' => { self.bump(); Token::Plus }
                b'*' => { self.bump(); Token::Star }
                b'/' => { self.bump(); Token::Slash }
                b'%' => { self.bump(); Token::Percent }
                b'|' => { self.bump(); Token::Pipe }
                b'-' => {
                    // comments are -- to end of line, but skip_whitespace_and_comments
                    // already drained those. a single - here is the minus operator.
                    self.bump();
                    Token::Minus
                }
                b'=' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::EqEq
                    } else {
                        Token::Eq
                    }
                }
                b'!' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::BangEq
                    } else {
                        return Err(LexError::UnexpectedChar { ch: '!', line, col });
                    }
                }
                b'>' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::GtEq
                    } else {
                        Token::Gt
                    }
                }
                b'<' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::LtEq
                    } else {
                        Token::Lt
                    }
                }
                b'"' => self.read_string(line, col)?,
                b'0'..=b'9' => self.read_number(line, col)?,
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.read_ident_or_keyword(),
                _ => {
                    // grab the offending char for the error message. it might be
                    // multi-byte UTF-8 (some user pasted a smart quote or em dash)
                    // so we decode it from the &str view, not the byte slice.
                    let ch = self.src[self.pos..].chars().next().unwrap_or('?');
                    return Err(LexError::UnexpectedChar { ch, line, col });
                }
            };
            out.push(Spanned { token, line, col });
        }
        Ok(out)
    }

    /*
    skip_whitespace_and_comments: chew through anything the parser doesn't care about.

    -- to end of line is a comment. we explicitly do not consume the newline as
    part of the comment, the next loop iteration sweeps it up as whitespace. that
    keeps line counting correct without special-casing.
    */
    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') => {
                    self.bump();
                }
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    // line comment, consume until newline or eof
                    self.bump();
                    self.bump();
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    /*
    read_string: consume "..." with simple escape support.

    we support \" \\ \n \t \r. anything else after a backslash is rejected
    rather than passed through, so typos surface early. the opening quote has
    already been peeked but not consumed when we get here.
    */
    fn read_string(&mut self, open_line: usize, open_col: usize) -> Result<Token, LexError> {
        self.bump(); // consume opening "
        let mut out = String::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(LexError::UnterminatedString { line: open_line, col: open_col });
            };
            match b {
                b'"' => {
                    self.bump();
                    return Ok(Token::StringLit(out));
                }
                b'\\' => {
                    self.bump();
                    let esc_line = self.line;
                    let esc_col = self.col;
                    match self.peek() {
                        Some(b'"') => { out.push('"'); self.bump(); }
                        Some(b'\\') => { out.push('\\'); self.bump(); }
                        Some(b'n') => { out.push('\n'); self.bump(); }
                        Some(b't') => { out.push('\t'); self.bump(); }
                        Some(b'r') => { out.push('\r'); self.bump(); }
                        Some(other) => {
                            return Err(LexError::InvalidEscape {
                                ch: other as char,
                                line: esc_line,
                                col: esc_col,
                            });
                        }
                        None => {
                            return Err(LexError::UnterminatedString {
                                line: open_line,
                                col: open_col,
                            });
                        }
                    }
                }
                _ => {
                    // any other byte: advance as a full UTF-8 codepoint so non-ASCII
                    // characters in strings work correctly.
                    let ch = self.bump_char().expect("peek said there was a byte");
                    out.push(ch);
                }
            }
        }
    }

    /*
    read_number: integer or float.

    we read digits, then optionally a '.' followed by more digits. note we only
    treat the dot as part of the number if there is at least one digit after it,
    otherwise "users.id" would lex "users" then start trying to read ".id" as a
    number. checking peek_at(1).is_digit guards against that.

    we then use str::parse to decode the captured slice. that handles overflow
    and malformed input. on parse failure we return InvalidNumber so the user
    gets a useful message instead of a confusing "unexpected token".
    */
    fn read_number(&mut self, line: usize, col: usize) -> Result<Token, LexError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.peek_at(1).map_or(false, |b| b.is_ascii_digit()) {
            is_float = true;
            self.bump();
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.bump();
                } else {
                    break;
                }
            }
        }
        let text = &self.src[start..self.pos];
        if is_float {
            text.parse::<f64>()
                .map(Token::FloatLit)
                .map_err(|_| LexError::InvalidNumber { text: text.to_string(), line, col })
        } else {
            text.parse::<i64>()
                .map(Token::IntLit)
                .map_err(|_| LexError::InvalidNumber { text: text.to_string(), line, col })
        }
    }

    /*
    read_ident_or_keyword: collect [a-zA-Z_][a-zA-Z0-9_]* then look up in the
    keyword table.

    we use a linear match over &str rather than a HashMap. with ~40 keywords
    the compiler turns this into a fast jump table, no allocation, no hashing.
    */
    fn read_ident_or_keyword(&mut self) -> Token {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.bump();
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        match text {
            "from" => Token::From,
            "where" => Token::Where,
            "select" => Token::Select,
            "order" => Token::Order,
            "limit" => Token::Limit,
            "offset" => Token::Offset,
            "group" => Token::Group,
            "join" => Token::Join,
            "insert" => Token::Insert,
            "update" => Token::Update,
            "delete" => Token::Delete,
            "drop" => Token::Drop,
            "create" => Token::Create,
            "index" => Token::Index,
            "on" => Token::On,
            "set" => Token::Set,
            "as" => Token::As,
            "in" => Token::In,
            "exists" => Token::Exists,
            "missing" => Token::Missing,
            "contains" => Token::Contains,
            "starts" => Token::Starts,
            "ends" => Token::Ends,
            "use" => Token::Use,
            "asc" => Token::Asc,
            "desc" => Token::Desc,
            "and" => Token::And,
            "or" => Token::Or,
            "not" => Token::Not,
            "if" => Token::If,
            "then" => Token::Then,
            "else" => Token::Else,
            "true" => Token::True,
            "false" => Token::False,
            "null" => Token::Null,
            "required" => Token::Required,
            "unique" => Token::Unique,
            "any" => Token::Any,
            _ => Token::Ident(text.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /*
    helper: lex a string and return just the token kinds, dropping spans.
    most tests only care about the token stream, not exact positions.
    */
    fn lex(input: &str) -> Vec<Token> {
        tokenize(input)
            .unwrap()
            .into_iter()
            .map(|s| s.token)
            .collect()
    }

    #[test]
    fn empty_input_yields_no_tokens() {
        assert_eq!(lex(""), Vec::<Token>::new());
        assert_eq!(lex("   \t\n  "), Vec::<Token>::new());
    }

    #[test]
    fn line_comment_skipped() {
        assert_eq!(lex("-- hello world"), Vec::<Token>::new());
        assert_eq!(
            lex("from users -- a comment\n| where age > 0"),
            vec![
                Token::From,
                Token::Ident("users".into()),
                Token::Pipe,
                Token::Where,
                Token::Ident("age".into()),
                Token::Gt,
                Token::IntLit(0),
            ]
        );
    }

    #[test]
    fn keywords_lex_as_keywords() {
        let src = "from where select order limit offset group join insert update \
                   delete drop create index on set as in exists missing contains \
                   starts ends use asc desc and or not if then else true false null \
                   required unique any";
        let out = lex(src);
        assert_eq!(
            out,
            vec![
                Token::From, Token::Where, Token::Select, Token::Order, Token::Limit,
                Token::Offset, Token::Group, Token::Join, Token::Insert, Token::Update,
                Token::Delete, Token::Drop, Token::Create, Token::Index, Token::On,
                Token::Set, Token::As, Token::In, Token::Exists, Token::Missing,
                Token::Contains, Token::Starts, Token::Ends, Token::Use, Token::Asc,
                Token::Desc, Token::And, Token::Or, Token::Not, Token::If, Token::Then,
                Token::Else, Token::True, Token::False, Token::Null, Token::Required,
                Token::Unique, Token::Any,
            ]
        );
    }

    #[test]
    fn identifiers_with_underscores_and_digits() {
        assert_eq!(
            lex("user_id _internal abc123 a"),
            vec![
                Token::Ident("user_id".into()),
                Token::Ident("_internal".into()),
                Token::Ident("abc123".into()),
                Token::Ident("a".into()),
            ]
        );
    }

    #[test]
    fn keyword_prefix_does_not_become_keyword() {
        // "fromage" is not "from" + "age". identifiers eat greedily.
        assert_eq!(lex("fromage"), vec![Token::Ident("fromage".into())]);
        assert_eq!(lex("ord_key"), vec![Token::Ident("ord_key".into())]);
    }

    #[test]
    fn int_and_float_literals() {
        assert_eq!(
            lex("0 42 1000000 3.14 0.5"),
            vec![
                Token::IntLit(0),
                Token::IntLit(42),
                Token::IntLit(1_000_000),
                Token::FloatLit(3.14),
                Token::FloatLit(0.5),
            ]
        );
    }

    #[test]
    fn dot_after_ident_is_punctuation_not_float() {
        // ensures "users.id" is three tokens, not "users", error, "id".
        assert_eq!(
            lex("users.id"),
            vec![
                Token::Ident("users".into()),
                Token::Dot,
                Token::Ident("id".into()),
            ]
        );
    }

    #[test]
    fn negative_numbers_lex_as_minus_plus_int() {
        // we do not fold the sign into the literal. the parser handles unary minus.
        assert_eq!(
            lex("-7 + 3"),
            vec![Token::Minus, Token::IntLit(7), Token::Plus, Token::IntLit(3)],
        );
    }

    #[test]
    fn string_literal_basic() {
        assert_eq!(lex("\"hello\""), vec![Token::StringLit("hello".into())]);
        assert_eq!(lex("\"\""), vec![Token::StringLit("".into())]);
    }

    #[test]
    fn string_literal_with_escapes() {
        assert_eq!(
            lex("\"a\\\"b\\\\c\\nd\""),
            vec![Token::StringLit("a\"b\\c\nd".into())]
        );
    }

    #[test]
    fn string_literal_with_unicode() {
        assert_eq!(
            lex("\"naïve résumé\""),
            vec![Token::StringLit("naïve résumé".into())]
        );
    }

    #[test]
    fn unterminated_string_is_error() {
        let err = tokenize("\"oops").unwrap_err();
        assert!(matches!(err, LexError::UnterminatedString { .. }));
    }

    #[test]
    fn invalid_escape_is_error() {
        let err = tokenize("\"\\q\"").unwrap_err();
        assert!(matches!(err, LexError::InvalidEscape { ch: 'q', .. }));
    }

    #[test]
    fn operators_all_lex_correctly() {
        assert_eq!(
            lex("== != > >= < <= + - * / % | = !=>"),
            vec![
                Token::EqEq, Token::BangEq, Token::Gt, Token::GtEq, Token::Lt, Token::LtEq,
                Token::Plus, Token::Minus, Token::Star, Token::Slash, Token::Percent, Token::Pipe,
                Token::Eq, Token::BangEq, Token::Gt,
            ]
        );
    }

    #[test]
    fn bang_without_eq_errors() {
        let err = tokenize("a ! b").unwrap_err();
        assert!(matches!(err, LexError::UnexpectedChar { ch: '!', .. }));
    }

    #[test]
    fn punctuation_lexes_correctly() {
        assert_eq!(
            lex("{ } [ ] ( ) , : ."),
            vec![
                Token::LBrace, Token::RBrace, Token::LBracket, Token::RBracket,
                Token::LParen, Token::RParen, Token::Comma, Token::Colon, Token::Dot,
            ]
        );
    }

    #[test]
    fn position_tracking_on_multiline() {
        let out = tokenize("from users\n  | where x > 0").unwrap();
        assert_eq!(out[0].token, Token::From);
        assert_eq!((out[0].line, out[0].col), (1, 1));
        assert_eq!(out[1].token, Token::Ident("users".into()));
        assert_eq!((out[1].line, out[1].col), (1, 6));
        assert_eq!(out[2].token, Token::Pipe);
        assert_eq!((out[2].line, out[2].col), (2, 3));
        assert_eq!(out[3].token, Token::Where);
        assert_eq!((out[3].line, out[3].col), (2, 5));
    }

    #[test]
    fn milestone_query_tokens() {
        // exact token stream for the phase 4 milestone query in plans.txt
        let src = "from users | where age > 18 and country == \"CA\" \
                   | order name asc | select name, email | limit 25";
        assert_eq!(
            lex(src),
            vec![
                Token::From, Token::Ident("users".into()),
                Token::Pipe, Token::Where, Token::Ident("age".into()), Token::Gt, Token::IntLit(18),
                Token::And, Token::Ident("country".into()), Token::EqEq, Token::StringLit("CA".into()),
                Token::Pipe, Token::Order, Token::Ident("name".into()), Token::Asc,
                Token::Pipe, Token::Select, Token::Ident("name".into()), Token::Comma, Token::Ident("email".into()),
                Token::Pipe, Token::Limit, Token::IntLit(25),
            ]
        );
    }

    #[test]
    fn lots_of_representative_queries_lex_without_error() {
        // smoke-test: anything from asl.txt examples should lex cleanly.
        let queries = [
            "from users",
            "from users use index age",
            "from users | where age > 18",
            "from users | where age > 18 and country == \"CA\"",
            "from users | where name contains \"ali\"",
            "from orders | where status in [\"pending\", \"shipped\"]",
            "from orders | where status not in [\"cancelled\"]",
            "from logs | where exists error_code",
            "from logs | where missing error_code",
            "from users | select name, email",
            "from users | select *",
            "from users | select name, age as years",
            "from users | drop password, internal_id",
            "from users | order age desc",
            "from users | order country asc, age desc",
            "from users | limit 10",
            "from users | offset 20 | limit 10",
            "from orders | group status | select status, count",
            "from orders | group user_id | select user_id, sum total",
            "from users | insert { name: \"alice\", age: 25 }",
            "from users | insert [{ name: \"bob\" }, { name: \"carol\" }]",
            "from users | where id == 1 | update set age = 26",
            "from orders | where status == \"pending\" | update set retries = retries + 1",
            "from users | where age < 0 | delete",
            "from orders | join users on user_id == id | select name, total",
            "from products | select name, price * 1.13 as price_with_tax",
            "from users | select name, if age >= 18 then \"adult\" else \"minor\" as group",
            "create users { id: int required unique, name: string required, age: int }",
            "create logs { timestamp: int required, message: string, level: string }",
            "drop collection users",
            "create index on users.id",
            "create index on orders.user_id, orders.status",
            "drop index on users.id",
            "from products | where price >= 10.0 and price <= 99.99",
            "from users | where age != null",
            "from users | where active == true",
            "from users | where active == false",
            "from users | where score == null",
            "from x | where a == 1 or b == 2 or c == 3",
            "from x | where not (a == 1)",
            "from x | where (a + b) * c >= 10",
            "from x | where field starts \"pre\"",
            "from x | where field ends \"fix\"",
            "from x | where field contains \"mid\"",
            "from x | where tags in [1, 2, 3]",
            "from x | where exists optional_field",
            "from x | where missing optional_field",
            "from x | insert { nested: { a: 1, b: [1, 2, 3] } }",
            "from x | update set score = score * 2",
            "from x | where -age < 0",
        ];
        for q in queries.iter() {
            tokenize(q).unwrap_or_else(|e| panic!("failed to lex {q:?}: {e}"));
        }
        assert!(queries.len() >= 50, "we want >= 50 representative queries, got {}", queries.len());
    }
}
