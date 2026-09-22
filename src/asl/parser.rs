/*
parser.rs: token stream -> Statement (ast).

phase 4.4 of plans.txt. recursive descent, hand-rolled, no parser-generator.

high-level shape:

  parse_statement
    ├── parse_create        ── create <coll> {...} | create index on coll.f, ...
    ├── parse_drop          ── drop collection <name> | drop index on coll.f
    └── parse_pipeline      ── from <coll> [use index f] (| stage)*
            └── parse_stage ── where | select | drop | order | limit | offset
                               | group | join | insert | update | delete

  expressions use a layered recursive-descent grammar, lowest precedence at the
  top (parse_expr) drilling down to highest (parse_primary):

      expr        := ternary | or_expr
      or_expr     := and_expr ("or" and_expr)*
      and_expr    := not_expr ("and" not_expr)*
      not_expr    := "not" not_expr | predicate
      predicate   := "exists" IDENT
                   | "missing" IDENT
                   | add_expr  ( cmp_op add_expr
                               | "in" "[" lit_list "]"
                               | "not" "in" "[" lit_list "]"
                               | "contains" STR
                               | "starts" STR
                               | "ends" STR
                               )?
      add_expr    := mul_expr (("+"|"-") mul_expr)*
      mul_expr    := unary_expr (("*"|"/"|"%") unary_expr)*
      unary_expr  := "-" unary_expr | primary
      primary     := INT | FLOAT | STR | "true" | "false" | "null"
                   | IDENT | "(" expr ")" | "[" expr,* "]" | "{" field,* "}"
                   | "if" expr "then" expr "else" expr

design notes:

  - exists/missing live in `predicate` because they're prefix forms that
    require a bare field, not an arbitrary subexpression. putting them at the
    unary level would be wrong because they should bind tighter than "and"/"or"
    but looser than comparisons (so "exists a and b == 1" groups correctly).

  - membership / string-match tails (in, not in, contains, starts, ends) only
    legally apply to a bare field. we parse them after an additive expression
    and then assert the lhs is Expr::Field, otherwise we emit a parse error.
    that catches things like "(a + b) in [1, 2]" at parse time.

  - aggregate functions (count, sum, avg, ...) are not lexer keywords. they
    look like normal identifiers. but inside a select item they have special
    meaning, so parse_select_expr peeks for them before falling through to
    generic expression parsing.

  - "collection" is also a contextual keyword (appears only in "drop
    collection <name>"). it stays an Ident token; the parser checks for it by
    string match.
*/

use super::{
    Assignment, BinOp, Direction, DocLiteral, Expr, LitValue, OrderKey, Pipeline, SchemaField,
    SchemaType, SelectItem, Spanned, Stage, Statement, StringMatchOp, Token, UnaryOp,
};

use std::fmt;

/*
ParseError: anything the parser rejects.

we keep a single struct (not a giant enum) because the messages are mostly
context-dependent strings anyway. line/col come from whichever token was
"unexpected", or from end-of-input if we ran out.
*/
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at line {}, column {}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for ParseError {}

/*
parse: top-level entry point. takes the full token stream and either returns
a Statement or the first parse error encountered.

we require the parser to consume the entire stream. trailing tokens are an
error because they almost always mean the user wrote something the grammar
didn't expect (e.g. a missing pipe between two stages).
*/
pub fn parse(tokens: &[Spanned]) -> Result<Statement, ParseError> {
    let mut parser = Parser::new(tokens);
    let stmt = parser.parse_statement()?;
    if !parser.is_eof() {
        return Err(parser.error_here("unexpected trailing tokens"));
    }
    Ok(stmt)
}

struct Parser<'a> {
    tokens: &'a [Spanned],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Spanned]) -> Self {
        Parser { tokens, pos: 0 }
    }

    // ---------- cursor helpers ----------

    fn peek_token(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|s| &s.token)
    }

    fn peek_token_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset).map(|s| &s.token)
    }

    fn peek_span(&self) -> Option<&Spanned> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Spanned> {
        let s = self.tokens.get(self.pos).cloned();
        if s.is_some() {
            self.pos += 1;
        }
        s
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /*
    error_here: build a ParseError pointing at the current cursor.

    if we're at end-of-input we fall back to the last seen position so the
    message still points somewhere useful. line/col 1/1 is a fallback for
    empty input.
    */
    fn error_here<S: Into<String>>(&self, message: S) -> ParseError {
        let (line, col) = if let Some(s) = self.peek_span() {
            (s.line, s.col)
        } else if let Some(last) = self.tokens.last() {
            (last.line, last.col)
        } else {
            (1, 1)
        };
        ParseError { message: message.into(), line, col }
    }

    /*
    expect: consume the next token if it matches `want` exactly, else error.

    relies on Token's derived PartialEq. only usable for dataless variants (the
    ones that carry data, like Ident/IntLit, get dedicated methods below since
    we can't pre-build a payload to compare against).
    */
    fn expect(&mut self, want: Token) -> Result<(), ParseError> {
        match self.peek_token() {
            Some(t) if *t == want => {
                self.advance();
                Ok(())
            }
            Some(other) => Err(self.error_here(format!(
                "expected {:?}, found {:?}", want, other
            ))),
            None => Err(self.error_here(format!("expected {:?}, found end of input", want))),
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.peek_token() {
            Some(Token::Ident(_)) => {
                if let Some(Spanned { token: Token::Ident(name), .. }) = self.advance() {
                    Ok(name)
                } else {
                    unreachable!()
                }
            }
            Some(other) => Err(self.error_here(format!(
                "expected an identifier, found {:?}", other
            ))),
            None => Err(self.error_here("expected an identifier, found end of input")),
        }
    }

    fn expect_int_lit(&mut self) -> Result<i64, ParseError> {
        match self.peek_token() {
            Some(Token::IntLit(_)) => {
                if let Some(Spanned { token: Token::IntLit(n), .. }) = self.advance() {
                    Ok(n)
                } else {
                    unreachable!()
                }
            }
            Some(other) => Err(self.error_here(format!(
                "expected an integer literal, found {:?}", other
            ))),
            None => Err(self.error_here("expected an integer literal, found end of input")),
        }
    }

    fn expect_string_lit(&mut self) -> Result<String, ParseError> {
        match self.peek_token() {
            Some(Token::StringLit(_)) => {
                if let Some(Spanned { token: Token::StringLit(s), .. }) = self.advance() {
                    Ok(s)
                } else {
                    unreachable!()
                }
            }
            Some(other) => Err(self.error_here(format!(
                "expected a string literal, found {:?}", other
            ))),
            None => Err(self.error_here("expected a string literal, found end of input")),
        }
    }

    // ---------- top-level statement ----------

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek_token() {
            Some(Token::Create) => self.parse_create(),
            Some(Token::Drop) => self.parse_drop_statement(),
            Some(Token::From) => Ok(Statement::Pipeline(self.parse_pipeline()?)),
            Some(_) => Err(self.error_here("expected 'from', 'create', or 'drop' at start of statement")),
            None => Err(self.error_here("empty input")),
        }
    }

    fn parse_create(&mut self) -> Result<Statement, ParseError> {
        self.expect(Token::Create)?;
        // create index ...   or   create <coll> { schema }
        // create [unique] index [if not exists] on <coll>.<field>
        let unique = if self.peek_token() == Some(&Token::Unique) {
            self.advance();
            true
        } else {
            false
        };
        if self.peek_token() == Some(&Token::Index) {
            self.advance();
            let if_not_exists = self.parse_if_not_exists()?;
            self.expect(Token::On)?;
            let (collection, fields) = self.parse_index_target()?;
            return Ok(Statement::CreateIndex { collection, fields, if_not_exists, unique });
        }
        if unique {
            return Err(self.error_here("expected 'index' after 'unique'"));
        }
        let if_not_exists = self.parse_if_not_exists()?;
        let name = self.expect_ident()?;
        self.expect(Token::LBrace)?;
        let schema = self.parse_schema_field_list()?;
        self.expect(Token::RBrace)?;
        Ok(Statement::CreateCollection { name, schema, if_not_exists })
    }

    /*
    parse_if_not_exists / parse_if_exists: the optional guard that turns a DDL
    statement into a no-op instead of an error when the object is already in the
    state asked for.

    They exist because startup DDL is run on every boot: a client that creates its
    collections each time would otherwise have to send statements it expects to
    fail, and then tell a benign "already exists" apart from a real failure by
    reading the message.
    */
    fn parse_if_not_exists(&mut self) -> Result<bool, ParseError> {
        if self.peek_token() != Some(&Token::If) {
            return Ok(false);
        }
        self.advance();
        self.expect(Token::Not)?;
        self.expect(Token::Exists)?;
        Ok(true)
    }

    fn parse_if_exists(&mut self) -> Result<bool, ParseError> {
        if self.peek_token() != Some(&Token::If) {
            return Ok(false);
        }
        self.advance();
        self.expect(Token::Exists)?;
        Ok(true)
    }

    fn parse_drop_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(Token::Drop)?;
        // clone the peeked token so we don't hold a borrow on self across the
        // self.advance() calls in the match arms. cloning a Token is cheap for
        // the dataless variants and is a single String clone for Ident.
        let peeked = self.peek_token().cloned();
        match peeked {
            Some(Token::Index) => {
                self.advance();
                let if_exists = self.parse_if_exists()?;
                self.expect(Token::On)?;
                let (collection, fields) = self.parse_index_target()?;
                Ok(Statement::DropIndex { collection, fields, if_exists })
            }
            Some(Token::Ident(s)) if s == "collection" => {
                self.advance();
                let if_exists = self.parse_if_exists()?;
                let name = self.expect_ident()?;
                Ok(Statement::DropCollection { name, if_exists })
            }
            Some(other) => Err(self.error_here(format!(
                "expected 'collection' or 'index' after 'drop', found {:?}", other
            ))),
            None => Err(self.error_here("expected 'collection' or 'index' after 'drop'")),
        }
    }

    /*
    parse_index_target: "<coll>.<field> (, <coll>.<field>)*"

    we enforce that all qualified fields refer to the same collection, since
    a composite index lives on one collection. anything else is almost
    certainly a typo.
    */
    fn parse_index_target(&mut self) -> Result<(String, Vec<String>), ParseError> {
        let (collection, first) = self.parse_qualified_field()?;
        let mut fields = vec![first];
        while self.peek_token() == Some(&Token::Comma) {
            self.advance();
            let (c, f) = self.parse_qualified_field()?;
            if c != collection {
                return Err(self.error_here(format!(
                    "all index fields must belong to the same collection (got {} and {})",
                    collection, c
                )));
            }
            fields.push(f);
        }
        Ok((collection, fields))
    }

    fn parse_qualified_field(&mut self) -> Result<(String, String), ParseError> {
        let coll = self.expect_ident()?;
        self.expect(Token::Dot)?;
        let field = self.expect_ident()?;
        Ok((coll, field))
    }

    fn parse_schema_field_list(&mut self) -> Result<Vec<SchemaField>, ParseError> {
        let mut fields = Vec::new();
        if self.peek_token() != Some(&Token::RBrace) {
            fields.push(self.parse_schema_field()?);
            while self.peek_token() == Some(&Token::Comma) {
                self.advance();
                fields.push(self.parse_schema_field()?);
            }
        }
        Ok(fields)
    }

    fn parse_schema_field(&mut self) -> Result<SchemaField, ParseError> {
        let name = self.expect_ident()?;
        self.expect(Token::Colon)?;
        let ty = self.parse_schema_type()?;
        let mut required = false;
        let mut unique = false;
        loop {
            match self.peek_token() {
                Some(Token::Required) => { self.advance(); required = true; }
                Some(Token::Unique) => { self.advance(); unique = true; }
                _ => break,
            }
        }
        Ok(SchemaField { name, ty, required, unique })
    }

    /*
    parse_schema_type: int | float | string | bool | any

    "any" is a lexer keyword (Token::Any); the others are just identifiers.
    rather than promote them to keywords (which would bleed into expressions
    where "string" as a field name is normal), we match them by name here.
    */
    fn parse_schema_type(&mut self) -> Result<SchemaType, ParseError> {
        // clone the peeked token so we can match without holding a borrow on
        // self when we call self.advance() inside an arm.
        let peeked = self.peek_token().cloned();
        match peeked {
            Some(Token::Any) => { self.advance(); Ok(SchemaType::Any) }
            Some(Token::Ident(name)) => {
                match name.as_str() {
                    "int" => { self.advance(); Ok(SchemaType::Int) }
                    "float" => { self.advance(); Ok(SchemaType::Float) }
                    "string" => { self.advance(); Ok(SchemaType::String) }
                    "bool" => { self.advance(); Ok(SchemaType::Bool) }
                    other => Err(self.error_here(format!("unknown schema type {:?}", other))),
                }
            }
            Some(other) => Err(self.error_here(format!("expected a type name, found {:?}", other))),
            None => Err(self.error_here("expected a type name, found end of input")),
        }
    }

    // ---------- pipeline ----------

    /*
    parse_pipeline: stages, separated by anything or nothing (asl.txt v0.2).

    THE RULE: a new stage begins wherever a STAGE KEYWORD appears in stage
    position. Whatever separator sits between stages is optional decoration.

    So all four of these produce the identical Pipeline:

        from users | where age > 18 | select name      pipe
        from users, where age > 18, select name        comma
        from users where age > 18 select name          nothing
        from users \n where age > 18 \n select name     newline

    The loop below is the whole implementation. It skips any separator it
    finds, then continues as long as the next token opens a stage. Because the
    KEYWORD is what delimits, a missing separator costs nothing, and a
    newline needs no handling at all: the lexer already treats it as
    whitespace, and the keyword on the next line is itself the delimiter.

    BACKWARD COMPATIBLE. Every v0.1 query still parses unchanged, because a
    pipe between stages is still accepted, just no longer required.
    */
    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut stages = Vec::new();
        stages.push(self.parse_from_stage()?);
        loop {
            if matches!(self.peek_token(), Some(Token::Pipe) | Some(Token::Comma)) {
                /*
                A separator is only consumed when a stage actually follows it.
                A TRAILING separator is left in place on purpose, so the
                caller's end-of-input check rejects `from users |` instead of
                silently accepting a truncated query. Optional decoration
                between two stages is one thing; a separator with nothing
                after it is a typo.
                */
                if !self.stage_keyword_at(1) {
                    break;
                }
                self.advance();
            } else if !self.stage_keyword_at(0) {
                break;
            }
            stages.push(self.parse_stage()?);
        }
        Ok(stages)
    }

    /*
    at_stage_keyword: does the next token open a new stage?

    The closed set from asl.txt. Keeping it in one place matters: this
    predicate is what makes the separator optional, and it is also what stops
    a select list at a stage boundary, so the two uses cannot drift apart.

    From is included even though it may only appear first. Recognising it here
    means `from a from b` fails with the parser's own "from may only appear as
    the first stage" message rather than a confusing token error.
    */
    fn stage_keyword_at(&self, offset: usize) -> bool {
        matches!(
            self.peek_token_at(offset),
            Some(Token::From)
                | Some(Token::Where)
                | Some(Token::Select)
                | Some(Token::Drop)
                | Some(Token::Order)
                | Some(Token::Limit)
                | Some(Token::Offset)
                | Some(Token::Group)
                | Some(Token::Join)
                | Some(Token::Insert)
                | Some(Token::Update)
                | Some(Token::Upsert)
                | Some(Token::Delete)
        )
    }

    /*
    at_list_continuation: is this comma continuing a list, or ending a stage?

    asl.txt: at a comma, the token AFTER it decides.

        select name, email          email is not a keyword -> list continues
        select name, where x > 1    where is a keyword     -> stage ended

    Used by every comma-separated list inside a stage, so `select a, b` and
    `select a, where ...` both do the right thing.
    */
    fn at_list_continuation(&self) -> bool {
        if self.peek_token() != Some(&Token::Comma) {
            return false;
        }
        !matches!(
            self.peek_token_at(1),
            Some(Token::From)
                | Some(Token::Where)
                | Some(Token::Select)
                | Some(Token::Drop)
                | Some(Token::Order)
                | Some(Token::Limit)
                | Some(Token::Offset)
                | Some(Token::Group)
                | Some(Token::Join)
                | Some(Token::Insert)
                | Some(Token::Update)
                | Some(Token::Upsert)
                | Some(Token::Delete)
        )
    }

    fn parse_from_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::From)?;
        let collection = self.expect_ident()?;
        let index_hint = if self.peek_token() == Some(&Token::Use) {
            self.advance();
            self.expect(Token::Index)?;
            let hint = self.expect_ident()?;
            Some(hint)
        } else {
            None
        };
        Ok(Stage::From { collection, index_hint })
    }

    fn parse_stage(&mut self) -> Result<Stage, ParseError> {
        match self.peek_token() {
            Some(Token::Where) => self.parse_where_stage(),
            Some(Token::Select) => self.parse_select_stage(),
            Some(Token::Drop) => self.parse_drop_stage(),
            Some(Token::Order) => self.parse_order_stage(),
            Some(Token::Limit) => self.parse_limit_stage(),
            Some(Token::Offset) => self.parse_offset_stage(),
            Some(Token::Group) => self.parse_group_stage(),
            Some(Token::Join) => self.parse_join_stage(),
            Some(Token::Insert) => self.parse_insert_stage(),
            Some(Token::Update) => self.parse_update_stage(),
            Some(Token::Upsert) => self.parse_upsert_stage(),
            Some(Token::Delete) => { self.advance(); Ok(Stage::Delete) }
            Some(other) => Err(self.error_here(format!("expected a stage name, found {:?}", other))),
            None => Err(self.error_here("expected a stage name, found end of input")),
        }
    }

    fn parse_where_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Where)?;
        let expr = self.parse_expr()?;
        Ok(Stage::Where { expr })
    }

    fn parse_select_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Select)?;
        // select *  -> no items
        if self.peek_token() == Some(&Token::Star) {
            self.advance();
            return Ok(Stage::Select { items: None });
        }
        let mut items = Vec::new();
        items.push(self.parse_select_item()?);
        while self.at_list_continuation() {
            self.advance();
            items.push(self.parse_select_item()?);
        }
        Ok(Stage::Select { items: Some(items) })
    }

    fn parse_select_item(&mut self) -> Result<SelectItem, ParseError> {
        let expr = self.parse_select_expr()?;
        let alias = if self.peek_token() == Some(&Token::As) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(SelectItem { expr, alias })
    }

    /*
    parse_select_expr: like parse_expr, but recognises aggregate-call forms
    that only make sense inside a select.

    count          -> AggCall { name: "count", arg: None }
    sum total      -> AggCall { name: "sum",   arg: Some("total") }
    avg / min / max / collect all behave like sum.

    we only treat an identifier as an aggregate when the following token makes
    it unambiguous (next is comma / "as" / "|" / end for count, next is another
    ident for sum-family). otherwise we fall through and let the generic
    expression parser handle it as a plain field reference.
    */
    fn parse_select_expr(&mut self) -> Result<Expr, ParseError> {
        // grab the head ident (if any) and the next token, both by clone, so we
        // can decide whether to take the aggregate-call branch without holding
        // a borrow on self.
        let head = match self.peek_token() {
            Some(Token::Ident(name)) => Some(name.clone()),
            _ => None,
        };
        let next_kind = self.peek_token_at(1).cloned();
        if let Some(n) = head {
            if n == "count" {
                let terminates = matches!(
                    next_kind,
                    Some(Token::Comma) | Some(Token::As) | Some(Token::Pipe) | None
                );
                if terminates {
                    self.advance();
                    return Ok(Expr::AggCall { name: n, arg: None });
                }
            }
            if matches!(n.as_str(), "sum" | "avg" | "min" | "max" | "collect")
                && matches!(next_kind, Some(Token::Ident(_)))
            {
                self.advance();
                let field = self.expect_ident()?;
                return Ok(Expr::AggCall { name: n, arg: Some(field) });
            }
        }
        self.parse_expr()
    }

    fn parse_drop_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Drop)?;
        let mut fields = Vec::new();
        fields.push(self.expect_ident()?);
        while self.at_list_continuation() {
            self.advance();
            fields.push(self.expect_ident()?);
        }
        Ok(Stage::Drop { fields })
    }

    fn parse_order_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Order)?;
        let mut keys = Vec::new();
        keys.push(self.parse_order_key()?);
        while self.at_list_continuation() {
            self.advance();
            keys.push(self.parse_order_key()?);
        }
        Ok(Stage::Order { keys })
    }

    fn parse_order_key(&mut self) -> Result<OrderKey, ParseError> {
        let field = self.expect_ident()?;
        let direction = match self.peek_token() {
            Some(Token::Asc) => { self.advance(); Direction::Asc }
            Some(Token::Desc) => { self.advance(); Direction::Desc }
            // direction defaults to asc when omitted, matching SQL convention
            _ => Direction::Asc,
        };
        Ok(OrderKey { field, direction })
    }

    fn parse_limit_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Limit)?;
        let n = self.expect_int_lit()?;
        if n < 0 {
            return Err(self.error_here("limit must be non-negative"));
        }
        Ok(Stage::Limit { n })
    }

    fn parse_offset_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Offset)?;
        let n = self.expect_int_lit()?;
        if n < 0 {
            return Err(self.error_here("offset must be non-negative"));
        }
        Ok(Stage::Offset { n })
    }

    fn parse_group_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Group)?;
        let mut fields = Vec::new();
        fields.push(self.expect_ident()?);
        while self.at_list_continuation() {
            self.advance();
            fields.push(self.expect_ident()?);
        }
        Ok(Stage::Group { fields })
    }

    fn parse_join_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Join)?;
        let collection = self.expect_ident()?;
        self.expect(Token::On)?;
        let left_field = self.expect_ident()?;
        self.expect(Token::EqEq)?;
        let right_field = self.expect_ident()?;
        Ok(Stage::Join { collection, left_field, right_field })
    }

    /*
    upsert: update every matched row with this document's fields, or insert it
    when the pipeline matched nothing.

        from nodes | where nodeId == "n1" | upsert { nodeId: "n1", load: 3 }

    One statement rather than the client's update-then-insert-if-zero, which
    cannot be made atomic from outside and races another writer.
    */
    fn parse_upsert_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Upsert)?;
        let doc = self.parse_doc_literal()?;
        Ok(Stage::Upsert { doc })
    }

    fn parse_insert_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Insert)?;
        match self.peek_token() {
            Some(Token::LBrace) => {
                let doc = self.parse_doc_literal()?;
                Ok(Stage::Insert { docs: vec![doc] })
            }
            Some(Token::LBracket) => {
                self.advance();
                let mut docs = Vec::new();
                if self.peek_token() != Some(&Token::RBracket) {
                    docs.push(self.parse_doc_literal()?);
                    while self.peek_token() == Some(&Token::Comma) {
                        self.advance();
                        docs.push(self.parse_doc_literal()?);
                    }
                }
                self.expect(Token::RBracket)?;
                Ok(Stage::Insert { docs })
            }
            Some(other) => Err(self.error_here(format!(
                "expected '{{' or '[' after insert, found {:?}", other
            ))),
            None => Err(self.error_here("expected '{' or '[' after insert")),
        }
    }

    fn parse_update_stage(&mut self) -> Result<Stage, ParseError> {
        self.expect(Token::Update)?;
        self.expect(Token::Set)?;
        let mut assignments = Vec::new();
        assignments.push(self.parse_assignment()?);
        while self.at_list_continuation() {
            self.advance();
            assignments.push(self.parse_assignment()?);
        }
        Ok(Stage::Update { assignments })
    }

    fn parse_assignment(&mut self) -> Result<Assignment, ParseError> {
        let field = self.expect_ident()?;
        self.expect(Token::Eq)?;
        let value = self.parse_expr()?;
        Ok(Assignment { field, value })
    }

    // ---------- document / array literals ----------

    fn parse_doc_literal(&mut self) -> Result<DocLiteral, ParseError> {
        self.expect(Token::LBrace)?;
        let mut fields = Vec::new();
        if self.peek_token() != Some(&Token::RBrace) {
            fields.push(self.parse_doc_field()?);
            while self.peek_token() == Some(&Token::Comma) {
                self.advance();
                fields.push(self.parse_doc_field()?);
            }
        }
        self.expect(Token::RBrace)?;
        Ok(DocLiteral { fields })
    }

    fn parse_doc_field(&mut self) -> Result<(String, Expr), ParseError> {
        let key = self.expect_ident()?;
        self.expect(Token::Colon)?;
        let value = self.parse_expr()?;
        Ok((key, value))
    }

    // ---------- expressions ----------

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        // ternary is handled inside parse_primary so it can appear anywhere a
        // value is allowed (including nested inside arithmetic). but at the
        // top level we also allow it as a shortcut, mostly for symmetry.
        if self.peek_token() == Some(&Token::If) {
            return self.parse_ternary();
        }
        self.parse_or()
    }

    fn parse_ternary(&mut self) -> Result<Expr, ParseError> {
        self.expect(Token::If)?;
        let cond = self.parse_expr()?;
        self.expect(Token::Then)?;
        let then_branch = self.parse_expr()?;
        self.expect(Token::Else)?;
        let else_branch = self.parse_expr()?;
        Ok(Expr::Ternary {
            cond: Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        })
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while self.peek_token() == Some(&Token::Or) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::BinOp(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_not()?;
        while self.peek_token() == Some(&Token::And) {
            self.advance();
            let rhs = self.parse_not()?;
            lhs = Expr::BinOp(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_not(&mut self) -> Result<Expr, ParseError> {
        if self.peek_token() == Some(&Token::Not) {
            self.advance();
            let inner = self.parse_not()?;
            return Ok(Expr::UnaryOp(UnaryOp::Not, Box::new(inner)));
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Result<Expr, ParseError> {
        // prefix forms: exists / missing take a bare field name
        if self.peek_token() == Some(&Token::Exists) {
            self.advance();
            let name = self.expect_ident()?;
            return Ok(Expr::Exists(name));
        }
        if self.peek_token() == Some(&Token::Missing) {
            self.advance();
            let name = self.expect_ident()?;
            return Ok(Expr::Missing(name));
        }

        // generic case: parse an additive expression, then maybe a comparison
        // or membership/string-match tail.
        let lhs = self.parse_add()?;

        match self.peek_token() {
            Some(Token::EqEq) => {
                self.advance();
                let rhs = self.parse_add()?;
                Ok(Expr::BinOp(BinOp::Eq, Box::new(lhs), Box::new(rhs)))
            }
            Some(Token::BangEq) => {
                self.advance();
                let rhs = self.parse_add()?;
                Ok(Expr::BinOp(BinOp::NotEq, Box::new(lhs), Box::new(rhs)))
            }
            Some(Token::Gt) => {
                self.advance();
                let rhs = self.parse_add()?;
                Ok(Expr::BinOp(BinOp::Gt, Box::new(lhs), Box::new(rhs)))
            }
            Some(Token::GtEq) => {
                self.advance();
                let rhs = self.parse_add()?;
                Ok(Expr::BinOp(BinOp::GtEq, Box::new(lhs), Box::new(rhs)))
            }
            Some(Token::Lt) => {
                self.advance();
                let rhs = self.parse_add()?;
                Ok(Expr::BinOp(BinOp::Lt, Box::new(lhs), Box::new(rhs)))
            }
            Some(Token::LtEq) => {
                self.advance();
                let rhs = self.parse_add()?;
                Ok(Expr::BinOp(BinOp::LtEq, Box::new(lhs), Box::new(rhs)))
            }
            Some(Token::In) => {
                let field = require_field(lhs, self)?;
                self.advance();
                let values = self.parse_lit_array()?;
                Ok(Expr::In { field, values, negated: false })
            }
            Some(Token::Not) if self.peek_token_at(1) == Some(&Token::In) => {
                let field = require_field(lhs, self)?;
                self.advance();
                self.advance();
                let values = self.parse_lit_array()?;
                Ok(Expr::In { field, values, negated: true })
            }
            Some(Token::Contains) => {
                let field = require_field(lhs, self)?;
                self.advance();
                let pattern = self.expect_string_lit()?;
                Ok(Expr::StringMatch { field, op: StringMatchOp::Contains, pattern })
            }
            Some(Token::Starts) => {
                let field = require_field(lhs, self)?;
                self.advance();
                let pattern = self.expect_string_lit()?;
                Ok(Expr::StringMatch { field, op: StringMatchOp::Starts, pattern })
            }
            Some(Token::Ends) => {
                let field = require_field(lhs, self)?;
                self.advance();
                let pattern = self.expect_string_lit()?;
                Ok(Expr::StringMatch { field, op: StringMatchOp::Ends, pattern })
            }
            _ => Ok(lhs),
        }
    }

    fn parse_add(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_mul()?;
        loop {
            match self.peek_token() {
                Some(Token::Plus) => {
                    self.advance();
                    let rhs = self.parse_mul()?;
                    lhs = Expr::BinOp(BinOp::Add, Box::new(lhs), Box::new(rhs));
                }
                Some(Token::Minus) => {
                    self.advance();
                    let rhs = self.parse_mul()?;
                    lhs = Expr::BinOp(BinOp::Sub, Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            match self.peek_token() {
                Some(Token::Star) => {
                    self.advance();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::BinOp(BinOp::Mul, Box::new(lhs), Box::new(rhs));
                }
                Some(Token::Slash) => {
                    self.advance();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::BinOp(BinOp::Div, Box::new(lhs), Box::new(rhs));
                }
                Some(Token::Percent) => {
                    self.advance();
                    let rhs = self.parse_unary()?;
                    lhs = Expr::BinOp(BinOp::Mod, Box::new(lhs), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.peek_token() == Some(&Token::Minus) {
            self.advance();
            let inner = self.parse_unary()?;
            return Ok(Expr::UnaryOp(UnaryOp::Neg, Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let span = match self.advance() {
            Some(s) => s,
            None => return Err(self.error_here("expected an expression, found end of input")),
        };
        let line = span.line;
        let col = span.col;
        match span.token {
            Token::IntLit(n) => Ok(Expr::Lit(LitValue::Int(n))),
            Token::FloatLit(f) => Ok(Expr::Lit(LitValue::Float(f))),
            Token::StringLit(s) => Ok(Expr::Lit(LitValue::String(s))),
            Token::True => Ok(Expr::Lit(LitValue::Bool(true))),
            Token::False => Ok(Expr::Lit(LitValue::Bool(false))),
            Token::Null => Ok(Expr::Lit(LitValue::Null)),
            Token::Ident(name) => Ok(Expr::Field(name)),
            Token::LParen => {
                let inner = self.parse_expr()?;
                self.expect(Token::RParen)?;
                Ok(inner)
            }
            Token::LBracket => {
                // array literal of arbitrary expressions
                let mut items = Vec::new();
                if self.peek_token() != Some(&Token::RBracket) {
                    items.push(self.parse_expr()?);
                    while self.peek_token() == Some(&Token::Comma) {
                        self.advance();
                        items.push(self.parse_expr()?);
                    }
                }
                self.expect(Token::RBracket)?;
                Ok(Expr::ArrayLit(items))
            }
            Token::LBrace => {
                // document literal
                let mut fields = Vec::new();
                if self.peek_token() != Some(&Token::RBrace) {
                    fields.push(self.parse_doc_field()?);
                    while self.peek_token() == Some(&Token::Comma) {
                        self.advance();
                        fields.push(self.parse_doc_field()?);
                    }
                }
                self.expect(Token::RBrace)?;
                Ok(Expr::DocLit(DocLiteral { fields }))
            }
            Token::If => {
                // ternary: we already consumed "if", so parse the rest manually.
                let cond = self.parse_expr()?;
                self.expect(Token::Then)?;
                let then_branch = self.parse_expr()?;
                self.expect(Token::Else)?;
                let else_branch = self.parse_expr()?;
                Ok(Expr::Ternary {
                    cond: Box::new(cond),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(else_branch),
                })
            }
            other => Err(ParseError {
                message: format!("unexpected token {:?} in expression", other),
                line,
                col,
            }),
        }
    }

    /*
    parse_lit_array: [lit, lit, ...]

    used by `in` / `not in`. only literal values allowed inside (per spec). we
    reach this with the '[' still in the token stream because callers expect
    a uniform interface; consume it here.
    */
    fn parse_lit_array(&mut self) -> Result<Vec<LitValue>, ParseError> {
        self.expect(Token::LBracket)?;
        let mut values = Vec::new();
        if self.peek_token() != Some(&Token::RBracket) {
            values.push(self.parse_lit_value()?);
            while self.peek_token() == Some(&Token::Comma) {
                self.advance();
                values.push(self.parse_lit_value()?);
            }
        }
        self.expect(Token::RBracket)?;
        Ok(values)
    }

    fn parse_lit_value(&mut self) -> Result<LitValue, ParseError> {
        // we also allow a leading minus for negative numbers inside an `in` list
        // so users can write "where score in [-1, 0, 1]" naturally.
        let negate = if self.peek_token() == Some(&Token::Minus) {
            self.advance();
            true
        } else {
            false
        };
        let span = match self.advance() {
            Some(s) => s,
            None => return Err(self.error_here("expected a literal, found end of input")),
        };
        let line = span.line;
        let col = span.col;
        let v = match span.token {
            Token::IntLit(n) => LitValue::Int(if negate { -n } else { n }),
            Token::FloatLit(f) => LitValue::Float(if negate { -f } else { f }),
            Token::StringLit(s) if !negate => LitValue::String(s),
            Token::True if !negate => LitValue::Bool(true),
            Token::False if !negate => LitValue::Bool(false),
            Token::Null if !negate => LitValue::Null,
            other => {
                return Err(ParseError {
                    message: format!("expected a literal value, found {:?}", other),
                    line,
                    col,
                });
            }
        };
        Ok(v)
    }
}

/*
require_field: bridge from "we just parsed an arbitrary expression" to
"the grammar said we needed a bare field reference here".

we accept Expr::Field(_) and reject everything else. the message names the
offender so the user can tell why their query was rejected.
*/
fn require_field(expr: Expr, parser: &Parser<'_>) -> Result<String, ParseError> {
    match expr {
        Expr::Field(name) => Ok(name),
        other => Err(parser.error_here(format!(
            "expected a field name on the left of in/contains/starts/ends, found {:?}",
            other
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asl::tokenize;

    fn parse_str(src: &str) -> Statement {
        let tokens = tokenize(src).unwrap_or_else(|e| panic!("lex failed on {src:?}: {e}"));
        parse(&tokens).unwrap_or_else(|e| panic!("parse failed on {src:?}: {e}"))
    }

    fn pipeline_of(stmt: Statement) -> Pipeline {
        match stmt {
            Statement::Pipeline(p) => p,
            other => panic!("expected pipeline, got {other:?}"),
        }
    }

    fn lit_int(n: i64) -> Expr { Expr::Lit(LitValue::Int(n)) }
    fn lit_str(s: &str) -> Expr { Expr::Lit(LitValue::String(s.into())) }
    fn field(name: &str) -> Expr { Expr::Field(name.into()) }

    // ---------- milestone ----------

    #[test]
    fn milestone_query_ast() {
        // phase 4 milestone from plans.txt
        let src = "from users | where age > 18 and country == \"CA\" \
                   | order name asc | select name, email | limit 25";
        let stmt = parse_str(src);
        let stages = pipeline_of(stmt);
        assert_eq!(stages.len(), 5);
        // from users
        assert_eq!(stages[0], Stage::From { collection: "users".into(), index_hint: None });
        // where age > 18 and country == "CA"
        let expected_where = Expr::BinOp(
            BinOp::And,
            Box::new(Expr::BinOp(BinOp::Gt, Box::new(field("age")), Box::new(lit_int(18)))),
            Box::new(Expr::BinOp(BinOp::Eq, Box::new(field("country")), Box::new(lit_str("CA")))),
        );
        assert_eq!(stages[1], Stage::Where { expr: expected_where });
        // order name asc
        assert_eq!(
            stages[2],
            Stage::Order { keys: vec![OrderKey { field: "name".into(), direction: Direction::Asc }] }
        );
        // select name, email
        assert_eq!(
            stages[3],
            Stage::Select {
                items: Some(vec![
                    SelectItem { expr: field("name"), alias: None },
                    SelectItem { expr: field("email"), alias: None },
                ])
            }
        );
        // limit 25
        assert_eq!(stages[4], Stage::Limit { n: 25 });
    }

    // ---------- source / from ----------

    #[test]
    fn from_simple() {
        assert_eq!(
            pipeline_of(parse_str("from users")),
            vec![Stage::From { collection: "users".into(), index_hint: None }]
        );
    }

    #[test]
    fn from_with_index_hint() {
        assert_eq!(
            pipeline_of(parse_str("from users use index age")),
            vec![Stage::From { collection: "users".into(), index_hint: Some("age".into()) }]
        );
    }

    // ---------- where / expressions ----------

    #[test]
    fn where_simple_compare() {
        let stages = pipeline_of(parse_str("from u | where age > 18"));
        assert_eq!(
            stages[1],
            Stage::Where {
                expr: Expr::BinOp(BinOp::Gt, Box::new(field("age")), Box::new(lit_int(18)))
            }
        );
    }

    #[test]
    fn where_and_or_precedence() {
        // a == 1 or b == 2 and c == 3   ->   a==1 OR (b==2 AND c==3)
        let stages = pipeline_of(parse_str("from u | where a == 1 or b == 2 and c == 3"));
        let inner_and = Expr::BinOp(
            BinOp::And,
            Box::new(Expr::BinOp(BinOp::Eq, Box::new(field("b")), Box::new(lit_int(2)))),
            Box::new(Expr::BinOp(BinOp::Eq, Box::new(field("c")), Box::new(lit_int(3)))),
        );
        let expected = Expr::BinOp(
            BinOp::Or,
            Box::new(Expr::BinOp(BinOp::Eq, Box::new(field("a")), Box::new(lit_int(1)))),
            Box::new(inner_and),
        );
        assert_eq!(stages[1], Stage::Where { expr: expected });
    }

    #[test]
    fn where_not_and_parens() {
        let stages = pipeline_of(parse_str("from u | where not (a == 1)"));
        let eq = Expr::BinOp(BinOp::Eq, Box::new(field("a")), Box::new(lit_int(1)));
        assert_eq!(
            stages[1],
            Stage::Where { expr: Expr::UnaryOp(UnaryOp::Not, Box::new(eq)) }
        );
    }

    #[test]
    fn where_arithmetic_precedence() {
        // (a + b) * c >= 10
        let stages = pipeline_of(parse_str("from u | where (a + b) * c >= 10"));
        let add = Expr::BinOp(BinOp::Add, Box::new(field("a")), Box::new(field("b")));
        let mul = Expr::BinOp(BinOp::Mul, Box::new(add), Box::new(field("c")));
        assert_eq!(
            stages[1],
            Stage::Where { expr: Expr::BinOp(BinOp::GtEq, Box::new(mul), Box::new(lit_int(10))) }
        );
    }

    #[test]
    fn where_in_list() {
        let stages = pipeline_of(parse_str(
            "from o | where status in [\"pending\", \"shipped\"]"
        ));
        assert_eq!(
            stages[1],
            Stage::Where {
                expr: Expr::In {
                    field: "status".into(),
                    values: vec![LitValue::String("pending".into()), LitValue::String("shipped".into())],
                    negated: false,
                }
            }
        );
    }

    #[test]
    fn where_not_in_list() {
        let stages = pipeline_of(parse_str("from o | where status not in [\"cancelled\"]"));
        assert_eq!(
            stages[1],
            Stage::Where {
                expr: Expr::In {
                    field: "status".into(),
                    values: vec![LitValue::String("cancelled".into())],
                    negated: true,
                }
            }
        );
    }

    #[test]
    fn where_in_list_with_negative_numbers() {
        let stages = pipeline_of(parse_str("from x | where score in [-1, 0, 1]"));
        assert_eq!(
            stages[1],
            Stage::Where {
                expr: Expr::In {
                    field: "score".into(),
                    values: vec![LitValue::Int(-1), LitValue::Int(0), LitValue::Int(1)],
                    negated: false,
                }
            }
        );
    }

    #[test]
    fn where_exists_and_missing() {
        let stages = pipeline_of(parse_str("from l | where exists error_code"));
        assert_eq!(stages[1], Stage::Where { expr: Expr::Exists("error_code".into()) });

        let stages = pipeline_of(parse_str("from l | where missing error_code"));
        assert_eq!(stages[1], Stage::Where { expr: Expr::Missing("error_code".into()) });
    }

    #[test]
    fn where_string_matches() {
        let stages = pipeline_of(parse_str("from u | where name contains \"ali\""));
        assert_eq!(
            stages[1],
            Stage::Where {
                expr: Expr::StringMatch {
                    field: "name".into(),
                    op: StringMatchOp::Contains,
                    pattern: "ali".into(),
                }
            }
        );
        let stages = pipeline_of(parse_str("from u | where name starts \"pre\""));
        assert!(matches!(
            stages[1],
            Stage::Where { expr: Expr::StringMatch { op: StringMatchOp::Starts, .. } }
        ));
        let stages = pipeline_of(parse_str("from u | where name ends \"fix\""));
        assert!(matches!(
            stages[1],
            Stage::Where { expr: Expr::StringMatch { op: StringMatchOp::Ends, .. } }
        ));
    }

    #[test]
    fn where_unary_minus() {
        let stages = pipeline_of(parse_str("from u | where -age < 0"));
        let neg = Expr::UnaryOp(UnaryOp::Neg, Box::new(field("age")));
        assert_eq!(
            stages[1],
            Stage::Where { expr: Expr::BinOp(BinOp::Lt, Box::new(neg), Box::new(lit_int(0))) }
        );
    }

    #[test]
    fn where_literal_types() {
        // ensure true / false / null / float lit roundtrip correctly
        let stages = pipeline_of(parse_str("from u | where active == true"));
        assert!(matches!(
            stages[1],
            Stage::Where { expr: Expr::BinOp(BinOp::Eq, _, _) }
        ));
        let stages = pipeline_of(parse_str("from u | where score == null"));
        assert!(matches!(
            stages[1],
            Stage::Where { expr: Expr::BinOp(BinOp::Eq, _, _) }
        ));
        let stages = pipeline_of(parse_str("from p | where price >= 10.0"));
        let expected = Expr::BinOp(
            BinOp::GtEq,
            Box::new(field("price")),
            Box::new(Expr::Lit(LitValue::Float(10.0))),
        );
        assert_eq!(stages[1], Stage::Where { expr: expected });
    }

    // ---------- select / projection ----------

    #[test]
    fn select_star() {
        let stages = pipeline_of(parse_str("from u | select *"));
        assert_eq!(stages[1], Stage::Select { items: None });
    }

    #[test]
    fn select_fields() {
        let stages = pipeline_of(parse_str("from u | select name, email"));
        assert_eq!(
            stages[1],
            Stage::Select {
                items: Some(vec![
                    SelectItem { expr: field("name"), alias: None },
                    SelectItem { expr: field("email"), alias: None },
                ])
            }
        );
    }

    #[test]
    fn select_with_alias() {
        let stages = pipeline_of(parse_str("from u | select name, age as years"));
        assert_eq!(
            stages[1],
            Stage::Select {
                items: Some(vec![
                    SelectItem { expr: field("name"), alias: None },
                    SelectItem { expr: field("age"), alias: Some("years".into()) },
                ])
            }
        );
    }

    #[test]
    fn select_expression_with_alias() {
        let stages = pipeline_of(parse_str("from p | select name, price * 1.13 as price_with_tax"));
        match &stages[1] {
            Stage::Select { items: Some(items) } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[1].alias.as_deref(), Some("price_with_tax"));
                assert!(matches!(items[1].expr, Expr::BinOp(BinOp::Mul, _, _)));
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn select_ternary_with_alias() {
        let stages = pipeline_of(parse_str(
            "from u | select name, if age >= 18 then \"adult\" else \"minor\" as group_label"
        ));
        match &stages[1] {
            Stage::Select { items: Some(items) } => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[1].expr, Expr::Ternary { .. }));
                assert_eq!(items[1].alias.as_deref(), Some("group_label"));
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn select_aggregate_count() {
        let stages = pipeline_of(parse_str("from o | group status | select status, count"));
        assert_eq!(stages[1], Stage::Group { fields: vec!["status".into()] });
        match &stages[2] {
            Stage::Select { items: Some(items) } => {
                assert_eq!(items[0].expr, field("status"));
                assert_eq!(
                    items[1].expr,
                    Expr::AggCall { name: "count".into(), arg: None }
                );
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn select_aggregate_sum_with_field() {
        let stages = pipeline_of(parse_str("from o | group user_id | select user_id, sum total"));
        match &stages[2] {
            Stage::Select { items: Some(items) } => {
                assert_eq!(
                    items[1].expr,
                    Expr::AggCall { name: "sum".into(), arg: Some("total".into()) }
                );
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    // ---------- drop / order / limit / offset ----------

    #[test]
    fn drop_fields_stage() {
        let stages = pipeline_of(parse_str("from u | drop password, internal_id"));
        assert_eq!(
            stages[1],
            Stage::Drop { fields: vec!["password".into(), "internal_id".into()] }
        );
    }

    #[test]
    fn order_multi_key() {
        let stages = pipeline_of(parse_str("from u | order country asc, age desc"));
        assert_eq!(
            stages[1],
            Stage::Order {
                keys: vec![
                    OrderKey { field: "country".into(), direction: Direction::Asc },
                    OrderKey { field: "age".into(), direction: Direction::Desc },
                ]
            }
        );
    }

    #[test]
    fn order_default_direction_is_asc() {
        let stages = pipeline_of(parse_str("from u | order name"));
        assert_eq!(
            stages[1],
            Stage::Order { keys: vec![OrderKey { field: "name".into(), direction: Direction::Asc }] }
        );
    }

    #[test]
    fn limit_and_offset() {
        let stages = pipeline_of(parse_str("from u | offset 20 | limit 10"));
        assert_eq!(stages[1], Stage::Offset { n: 20 });
        assert_eq!(stages[2], Stage::Limit { n: 10 });
    }

    #[test]
    fn limit_negative_is_error() {
        let tokens = tokenize("from u | limit -5").unwrap();
        // -5 lexes as Minus then IntLit(5); the parser will not see an IntLit
        // first, so it errors at expect_int_lit. either way this must fail.
        assert!(parse(&tokens).is_err());
    }

    // ---------- group / join ----------

    #[test]
    fn group_single_field() {
        let stages = pipeline_of(parse_str("from o | group status | select status, count"));
        assert_eq!(stages[1], Stage::Group { fields: vec!["status".into()] });
    }

    #[test]
    fn join_stage() {
        let stages = pipeline_of(parse_str("from o | join users on user_id == id | select name"));
        assert_eq!(
            stages[1],
            Stage::Join {
                collection: "users".into(),
                left_field: "user_id".into(),
                right_field: "id".into(),
            }
        );
    }

    // ---------- insert / update / delete ----------

    #[test]
    fn insert_single_doc() {
        let stages = pipeline_of(parse_str("from u | insert { name: \"alice\", age: 25 }"));
        match &stages[1] {
            Stage::Insert { docs } => {
                assert_eq!(docs.len(), 1);
                assert_eq!(docs[0].fields.len(), 2);
                assert_eq!(docs[0].fields[0].0, "name");
                assert_eq!(docs[0].fields[0].1, lit_str("alice"));
                assert_eq!(docs[0].fields[1].0, "age");
                assert_eq!(docs[0].fields[1].1, lit_int(25));
            }
            other => panic!("expected insert, got {other:?}"),
        }
    }

    #[test]
    fn insert_batch() {
        let stages = pipeline_of(parse_str(
            "from u | insert [{ name: \"bob\" }, { name: \"carol\" }]"
        ));
        match &stages[1] {
            Stage::Insert { docs } => {
                assert_eq!(docs.len(), 2);
                assert_eq!(docs[0].fields[0].0, "name");
                assert_eq!(docs[1].fields[0].0, "name");
            }
            other => panic!("expected insert, got {other:?}"),
        }
    }

    #[test]
    fn insert_nested_doc_and_array() {
        let stages = pipeline_of(parse_str(
            "from x | insert { nested: { a: 1, b: [1, 2, 3] } }"
        ));
        match &stages[1] {
            Stage::Insert { docs } => {
                assert_eq!(docs.len(), 1);
                assert_eq!(docs[0].fields[0].0, "nested");
                match &docs[0].fields[0].1 {
                    Expr::DocLit(inner) => {
                        assert_eq!(inner.fields[0].0, "a");
                        assert_eq!(inner.fields[1].0, "b");
                        assert!(matches!(inner.fields[1].1, Expr::ArrayLit(_)));
                    }
                    other => panic!("expected DocLit, got {other:?}"),
                }
            }
            other => panic!("expected insert, got {other:?}"),
        }
    }

    #[test]
    fn update_simple() {
        let stages = pipeline_of(parse_str("from u | where id == 1 | update set age = 26"));
        assert_eq!(
            stages[2],
            Stage::Update {
                assignments: vec![Assignment { field: "age".into(), value: lit_int(26) }]
            }
        );
    }

    #[test]
    fn update_field_expression() {
        let stages = pipeline_of(parse_str(
            "from o | where status == \"pending\" | update set retries = retries + 1"
        ));
        let value = Expr::BinOp(BinOp::Add, Box::new(field("retries")), Box::new(lit_int(1)));
        assert_eq!(
            stages[2],
            Stage::Update { assignments: vec![Assignment { field: "retries".into(), value }] }
        );
    }

    #[test]
    fn delete_stage() {
        let stages = pipeline_of(parse_str("from u | where age < 0 | delete"));
        assert_eq!(stages[2], Stage::Delete);
    }

    // ---------- create / drop ddl ----------

    #[test]
    fn create_collection_with_schema() {
        let stmt = parse_str(
            "create users { id: int required unique, name: string required, age: int }"
        );
        assert_eq!(
            stmt,
            Statement::CreateCollection {
                name: "users".into(),
                schema: vec![
                    SchemaField { name: "id".into(), ty: SchemaType::Int, required: true, unique: true },
                    SchemaField { name: "name".into(), ty: SchemaType::String, required: true, unique: false },
                    SchemaField { name: "age".into(), ty: SchemaType::Int, required: false, unique: false },
                ],
                if_not_exists: false,
            }
        );
    }

    #[test]
    fn create_collection_any_type() {
        let stmt = parse_str("create blobs { payload: any }");
        match stmt {
            Statement::CreateCollection { schema, .. } => {
                assert_eq!(schema[0].ty, SchemaType::Any);
            }
            other => panic!("expected create collection, got {other:?}"),
        }
    }

    #[test]
    fn drop_collection_stmt() {
        let stmt = parse_str("drop collection users");
        assert_eq!(stmt, Statement::DropCollection { name: "users".into(), if_exists: false });
    }

    #[test]
    fn create_index_single_field() {
        let stmt = parse_str("create index on users.id");
        assert_eq!(
            stmt,
            Statement::CreateIndex { collection: "users".into(), fields: vec!["id".into()], if_not_exists: false, unique: false }
        );
    }

    #[test]
    fn create_collection_if_not_exists() {
        let stmt = parse_str("create if not exists users { }");
        assert_eq!(
            stmt,
            Statement::CreateCollection {
                name: "users".into(),
                schema: vec![],
                if_not_exists: true,
            }
        );
    }

    #[test]
    fn create_index_if_not_exists() {
        let stmt = parse_str("create index if not exists on users.age");
        assert_eq!(
            stmt,
            Statement::CreateIndex {
                collection: "users".into(),
                fields: vec!["age".into()],
                if_not_exists: true,
                unique: false,
            }
        );
    }

    #[test]
    fn drop_collection_if_exists() {
        let stmt = parse_str("drop collection if exists users");
        assert_eq!(stmt, Statement::DropCollection { name: "users".into(), if_exists: true });
    }

    #[test]
    fn drop_index_if_exists() {
        let stmt = parse_str("drop index if exists on users.age");
        assert_eq!(
            stmt,
            Statement::DropIndex {
                collection: "users".into(),
                fields: vec!["age".into()],
                if_exists: true,
            }
        );
    }

    #[test]
    fn create_index_composite() {
        let stmt = parse_str("create index on orders.user_id, orders.status");
        assert_eq!(
            stmt,
            Statement::CreateIndex {
                collection: "orders".into(),
                fields: vec!["user_id".into(), "status".into()],
                if_not_exists: false,
                unique: false,
            }
        );
    }

    #[test]
    fn create_index_mismatched_collections_errors() {
        let tokens = tokenize("create index on orders.user_id, users.id").unwrap();
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn drop_index_stmt() {
        let stmt = parse_str("drop index on users.id");
        assert_eq!(
            stmt,
            Statement::DropIndex { collection: "users".into(), fields: vec!["id".into()], if_exists: false }
        );
    }

    // ---------- error cases ----------

    #[test]
    fn pipeline_must_start_with_from() {
        let tokens = tokenize("where age > 0").unwrap();
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn trailing_pipe_is_error() {
        let tokens = tokenize("from u |").unwrap();
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn in_with_non_field_lhs_errors() {
        let tokens = tokenize("from u | where (a + b) in [1, 2]").unwrap();
        assert!(parse(&tokens).is_err());
    }

    #[test]
    fn unknown_schema_type_errors() {
        let tokens = tokenize("create x { f: dunno }").unwrap();
        assert!(parse(&tokens).is_err());
    }

    // ---------- bulk smoke: every example in asl.txt parses ----------

    #[test]
    fn all_representative_queries_parse() {
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
            "from users | select name, if age >= 18 then \"adult\" else \"minor\" as group_label",
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
        assert!(queries.len() >= 50);
        for q in queries.iter() {
            let toks = tokenize(q).unwrap_or_else(|e| panic!("lex failed on {q:?}: {e}"));
            parse(&toks).unwrap_or_else(|e| panic!("parse failed on {q:?}: {e}"));
        }
    }
}
