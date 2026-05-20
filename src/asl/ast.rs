/*
ast.rs: the syntax tree the parser builds and the planner consumes.

phase 4.3 of plans.txt. these are pure data types, no behaviour. nothing in
this file talks to storage, indexes, or the buffer pool. that separation is
what lets the binder/planner phases pick up cleanly later: the parser produces
an ast, the binder walks the ast and resolves names against the catalog, the
planner turns the bound ast into physical operators.

a few design notes:

  - the plan listed Pipeline = Vec<Stage>, but DDL statements like
    "create users { ... }" and "drop index on users.id" don't really fit the
    pipe-and-stage model. so we have one outer Statement enum that is either
    a Pipeline (a Vec<Stage>) or one of four DDL variants. the parser returns
    Statement and downstream code matches on that.

  - Expr is recursive (BinOp holds Box<Expr>). Box is needed because Rust
    needs a known size for enum variants and Expr inside Expr would be
    infinite. Box<Expr> is a heap pointer, which has fixed size.

  - LitValue mirrors document::value::Value but only the leaf-typed cases
    (Int, Float, String, Bool, Null). we do NOT reuse Value here because (a)
    Value::Document uses HashMap, which loses field insertion order, and
    insertion-order matters for "insert {...}" literals; (b) keeping the ast
    independent of the runtime value type makes the parser easier to test in
    isolation. document/array literals get their own ast nodes (DocLiteral,
    ArrayLiteral) with Vec keys/values, preserving order.

  - exists/missing/in/contains/starts/ends are special grammar forms. they
    operate on a named field, not on an arbitrary subexpression, so they get
    their own Expr variants instead of being shoehorned into BinOp.
*/

/*
Statement: the top-level result of parsing one ASL input.

a query is a Pipeline. DDL ("create users {...}", "drop collection users",
"create index on users.id", "drop index on users.id") is one of the other
variants.
*/
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Pipeline(Pipeline),
    CreateCollection { name: String, schema: Vec<SchemaField> },
    DropCollection { name: String },
    CreateIndex { collection: String, fields: Vec<String> },
    DropIndex { collection: String, fields: Vec<String> },
}

pub type Pipeline = Vec<Stage>;

/*
Stage: one segment of a pipeline.

the first stage is always a source (From). subsequent stages transform the
stream. Insert/Update/Delete are terminal in the runtime model but we don't
enforce that here, the binder/validator will.
*/
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    From {
        collection: String,
        index_hint: Option<String>,
    },
    Where {
        expr: Expr,
    },
    Select {
        // None means `select *`, Some(...) means an explicit list.
        items: Option<Vec<SelectItem>>,
    },
    Drop {
        fields: Vec<String>,
    },
    Order {
        keys: Vec<OrderKey>,
    },
    Limit {
        n: i64,
    },
    Offset {
        n: i64,
    },
    Group {
        fields: Vec<String>,
    },
    Join {
        collection: String,
        left_field: String,
        right_field: String,
    },
    Insert {
        // a single document literal or a batch. always a Vec to keep the
        // executor's life simple, batch of one is fine.
        docs: Vec<DocLiteral>,
    },
    Update {
        assignments: Vec<Assignment>,
    },
    Delete,
}

/*
SelectItem: one expression in the projection list, with an optional alias.

the spec lists three forms: "select <field>", "select <field> as <alias>",
and aggregate forms like "select count" / "select sum total". we represent
all three as expressions; the parser disambiguates a bare aggregate
identifier (e.g. count) at parse time and the planner enforces aggregate
context.
*/
#[derive(Clone, Debug, PartialEq)]
pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderKey {
    pub field: String,
    pub direction: Direction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    pub field: String,
    pub value: Expr,
}

/*
SchemaField: one declared field in "create users { ... }".

required and unique are inline constraint flags. ty is one of int, float,
string, bool, any. unknown type names from the source become a parse error
rather than a SchemaType::Other variant, so we don't smuggle invalid schemas
through to the planner.
*/
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaField {
    pub name: String,
    pub ty: SchemaType,
    pub required: bool,
    pub unique: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaType {
    Int,
    Float,
    String,
    Bool,
    Any,
}

/*
DocLiteral: a { key: val, ... } literal as it appears in source.

we use Vec<(String, Expr)> instead of HashMap<String, Expr> so that field
insertion order is preserved. that matters for "insert {...}" because the
storage layer hashes whatever we hand it and we want deterministic ordering
when we round-trip through formatters.

values are Expr, not Value, so that future-us can support things like
"insert { score: a + b }". for the phase 4 milestone the values will only be
literal expressions, but the door is open.
*/
#[derive(Clone, Debug, PartialEq)]
pub struct DocLiteral {
    pub fields: Vec<(String, Expr)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Field(String),
    Lit(LitValue),
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    /*
    membership: <field> in [vals] or <field> not in [vals].
    we only allow literal value lists on the right (per spec), which is why
    the values are Vec<LitValue>, not Vec<Expr>.
    */
    In {
        field: String,
        values: Vec<LitValue>,
        negated: bool,
    },
    Exists(String),
    Missing(String),
    StringMatch {
        field: String,
        op: StringMatchOp,
        pattern: String,
    },
    Ternary {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /*
    composite literals. only legal inside an insert stage or inside another
    composite literal, but the parser produces them anywhere a value is
    accepted and the binder will reject them where they're not allowed.
    */
    ArrayLit(Vec<Expr>),
    DocLit(DocLiteral),
    /*
    aggregate function call inside a select item.

    count        -> AggCall { name: "count", arg: None }
    sum total    -> AggCall { name: "sum",   arg: Some("total") }

    these only have meaning inside a select that follows a group, but the
    parser produces them anywhere a select-item appears. the binder will
    enforce the surrounding-group rule. we keep name as a String rather than
    an enum so the planner can pick up new aggregates without changing the
    ast type.
    */
    AggCall { name: String, arg: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum LitValue {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    NotEq,
    Gt,
    GtEq,
    Lt,
    LtEq,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringMatchOp {
    Contains,
    Starts,
    Ends,
}
