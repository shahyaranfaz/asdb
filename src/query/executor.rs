/*
executor.rs: run a PhysicalOp tree and produce results.

plans.txt 5.6. Takes the tree the planner built, pulls documents through it,
and returns what the query asked for.

THE VOLCANO MODEL

Every operator exposes one method, `next()`, which returns the next document
or None when it is exhausted. An operator gets its input by calling `next()`
on its child. Execution starts by calling `next()` on the ROOT, and demand
flows down to the leaf.

That inversion is the whole point. `from users order name asc limit 25` over a
million documents never materializes a million documents: Limit asks Sort for
25 and then stops asking. Only the operators that genuinely have to buffer
(Sort, HashAggregate, the build side of HashJoin) hold more than one document
at a time, and each of those is documented below with why.

WHY NOT std::iter::Iterator

The obvious move is to implement Iterator and get map/filter/take for free. It
does not work here, and it is worth knowing why before trying it.

Iterator::next takes only `&mut self`. Our operators need the database as well
(a SeqScan has to read pages; an IndexScan has to walk a btree), and that
means every next() call needs `&mut BufferPool` too. There is nowhere to put
it: storing a `&mut` borrow in the operator struct makes two live operators a
borrow-checker conflict, which rules out HashJoin by construction.

So next() takes an explicit context:

    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Document>>

CONSEQUENCE: operators do NOT implement Iterator, and the adapters do not come
for free. That is a real cost and it is the right trade, because the
alternative shapes all move a compile-time guarantee to runtime. It is the
same conclusion the storage layer reaches independently, see
tests/multi_collection.rs.

ORDER OF WORK

SeqScan, Filter and Projection first: that trio is the first point a real
query runs end to end. Insert next, so there is data to query. Then
Limit/Offset/Sort, then Update/Delete, then IndexScan, then HashAggregate,
then HashJoin.
*/


use super::{QueryError, QueryResult};
use crate::asl::{Assignment, Direction, Expr, OrderKey, SelectItem};
use crate::database::Database;
use crate::document::{Document, Value};
use crate::query::evaluate::{compare_values, evaluate, evaluate_bool, values_equal};
use crate::query::plan::{PhysicalOp, ScanBounds};
use crate::storage::{DocId, PageId};

use std::collections::HashMap;

/*
Row: a document plus, when it has one, where it lives on disk.

The DocId is what makes Update and Delete possible: they need to write back to
the exact record the pipeline selected, and a Document alone does not say
which record that was.

It is Option because not every row corresponds to a stored record. A
projection builds a NEW document, an aggregate builds one per group, and
neither has a home on disk. Setting the id to None at those points is what
stops a later Delete from being handed a stale address, and the executor turns
that into a clear error rather than a silent no-op.
*/
#[derive(Clone, Debug)]
pub struct Row {
    pub doc_id: Option<DocId>,
    pub doc: Document,
}

impl Row {
    fn stored(doc_id: DocId, doc: Document) -> Self {
        Row { doc_id: Some(doc_id), doc }
    }

    fn derived(doc: Document) -> Self {
        Row { doc_id: None, doc }
    }
}

/*
ExecContext: everything an operator needs from outside itself.

Threaded through next() rather than stored, for the borrow reasons in the
header. Holding &mut Database means the executor has exclusive access for the
duration of a query, which is the right granularity for v1: there are no
concurrent transactions to interleave with.
*/
pub struct ExecContext<'a> {
    pub db: &'a mut Database,
}

impl<'a> ExecContext<'a> {
    pub fn new(db: &'a mut Database) -> Self {
        ExecContext { db }
    }
}

/*
QueryOutput: what a finished query hands back.

Two shapes, because the two kinds of statement genuinely differ. A pipeline
ending in a scan produces DOCUMENTS; one ending in insert/update/delete
produces a COUNT of what it touched. Collapsing them would make every caller
check which one it actually got.
*/
#[derive(Clone, Debug, PartialEq)]
pub enum QueryOutput {
    Documents(Vec<Document>),
    Affected(usize),
}

/*
Operator: the one method every physical operator implements.

Not std::iter::Iterator, for the reason in the header.

Ok(None) means exhausted, and once an operator returns None it must keep
returning None. Limit relies on being able to stop asking.
*/
pub trait Operator {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>>;
}

/*
execute: run a plan to completion.

Builds the operator tree, then pulls from the root until it is exhausted. The
only place that decides between the two QueryOutput shapes.
*/
pub fn execute(db: &mut Database, plan: &PhysicalOp) -> QueryResult<QueryOutput> {
    let mutating = matches!(
        plan,
        PhysicalOp::Insert { .. } | PhysicalOp::Update { .. } | PhysicalOp::Delete { .. }
    );

    let mut op = build(plan)?;
    let mut ctx = ExecContext::new(db);
    let mut rows = Vec::new();
    while let Some(row) = op.next(&mut ctx)? {
        rows.push(row.doc);
    }

    Ok(if mutating {
        QueryOutput::Affected(rows.len())
    } else {
        QueryOutput::Documents(rows)
    })
}

/*
build: PhysicalOp (data) -> Operator (running state).

The split matters. A plan is inert and comparable, which is what makes planner
tests possible without a database. An operator holds cursors, buffers and hash
tables. Keeping them apart means a plan can be asserted on without being run.
*/
fn build(plan: &PhysicalOp) -> QueryResult<Box<dyn Operator>> {
    Ok(match plan {
        PhysicalOp::SeqScan { collection } => Box::new(SeqScan::new(collection.clone())),
        PhysicalOp::IndexScan { collection, field, bounds } => {
            Box::new(IndexScan::new(collection.clone(), field.clone(), bounds.clone()))
        }
        PhysicalOp::Filter { input, predicate } => {
            Box::new(Filter { input: build(input)?, predicate: predicate.clone() })
        }
        PhysicalOp::Projection { input, items } => {
            Box::new(Projection { input: build(input)?, items: items.clone() })
        }
        PhysicalOp::Drop { input, fields } => {
            Box::new(DropFields { input: build(input)?, fields: fields.clone() })
        }
        PhysicalOp::Sort { input, keys } => Box::new(Sort::new(build(input)?, keys.clone())),
        PhysicalOp::Limit { input, n } => Box::new(Limit { input: build(input)?, remaining: *n }),
        PhysicalOp::Offset { input, n } => {
            Box::new(Offset { input: build(input)?, to_skip: *n })
        }
        PhysicalOp::HashAggregate { input, group_keys, items } => Box::new(HashAggregate::new(
            build(input)?,
            group_keys.clone(),
            items.clone(),
        )),
        PhysicalOp::HashJoin { left, right, left_field, right_field } => Box::new(HashJoin::new(
            build(left)?,
            build(right)?,
            left_field.clone(),
            right_field.clone(),
        )),
        PhysicalOp::Insert { collection, docs } => {
            Box::new(Insert::new(collection.clone(), docs.clone()))
        }
        PhysicalOp::Update { input, collection, assignments } => Box::new(Update {
            input: build(input)?,
            collection: collection.clone(),
            assignments: assignments.clone(),
        }),
        PhysicalOp::Delete { input, collection } => {
            Box::new(Delete { input: build(input)?, collection: collection.clone(), deleted: None })
        }
    })
}

/*
================================================================
LEAVES
================================================================
*/

/*
SeqScan: every live document in a collection, STREAMED a page at a time.

This used to materialise the whole collection on the first next(), so
`limit 25` over a million documents still read a million documents. It could
not do better, because a streaming cursor means holding a Collection handle
across next() calls, and back then every Collection carried its own
BufferPool, which made two live handles a corruption bug
(tests/multi_collection.rs).

Sharing the pool removed that constraint, so this now holds only a PAGE ID as
its cursor and fetches the next page when the current one is drained. Limit
stops asking, and the pages after it are never read.

A page rather than a record at a time: the unit of IO is a page, so pulling
one record per call would re-pin the same frame for every slot on it. One
page of decoded documents is a small bounded buffer.

The cursor is a page id rather than a live handle on purpose. Between calls
this operator holds no borrow at all, which is what lets a HashJoin drive two
scans without either of them pinning storage.
*/
struct SeqScan {
    collection: String,
    /// Page to fetch next. None once the chain is exhausted.
    cursor: Option<PageId>,
    /// Documents decoded from the current page, not yet emitted.
    page: std::vec::IntoIter<(DocId, Document)>,
    started: bool,
}

impl SeqScan {
    fn new(collection: String) -> Self {
        SeqScan {
            collection,
            cursor: None,
            page: Vec::new().into_iter(),
            started: false,
        }
    }
}

impl Operator for SeqScan {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        loop {
            if let Some((id, doc)) = self.page.next() {
                return Ok(Some(Row::stored(id, doc)));
            }
            // current page drained; is there another?
            if self.started && self.cursor.is_none() {
                return Ok(None);
            }
            let (docs, next) = ctx.db.scan_page(&self.collection, self.cursor)?;
            self.started = true;
            self.cursor = next;
            self.page = docs.into_iter();

            // An empty page in the middle of a chain is legal (every slot on
            // it tombstoned), so keep walking rather than stopping.
            if self.page.len() == 0 && self.cursor.is_none() {
                return Ok(None);
            }
        }
    }
}

/*
IndexScan: documents whose indexed field falls inside `bounds`.

Returns a SUPERSET of the matching rows whenever the original predicate was
exclusive, which is why the planner only drops a conjunct from the Filter when
the bound enforced it exactly. See ScanBounds in plan.rs.

Both bounds are required here. The planner guarantees it (a one-sided
predicate falls back to SeqScan), so an unbounded end reaching this point is a
planner bug rather than a user error.
*/
struct IndexScan {
    collection: String,
    field: String,
    bounds: ScanBounds,
    buffered: Option<std::vec::IntoIter<(DocId, Document)>>,
}

impl IndexScan {
    fn new(collection: String, field: String, bounds: ScanBounds) -> Self {
        IndexScan { collection, field, bounds, buffered: None }
    }
}

impl Operator for IndexScan {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        if self.buffered.is_none() {
            let (Some(low), Some(high)) = (&self.bounds.low, &self.bounds.high) else {
                return Err(QueryError::Unsupported(
                    "index scan requires both bounds; the planner should have chosen a sequential scan"
                        .to_string(),
                ));
            };
            let docs = ctx.db.range_index(&self.collection, &self.field, low, high)?;
            self.buffered = Some(docs.into_iter());
        }
        Ok(self
            .buffered
            .as_mut()
            .expect("just filled")
            .next()
            .map(|(id, doc)| Row::stored(id, doc)))
    }
}

/*
Insert: a leaf whose documents come from the query text.

Goes through Database::insert_doc rather than the collection directly, because
that is what keeps the indexes in step. Writing to the heap alone would leave
every index silently stale.
*/
struct Insert {
    collection: String,
    docs: Vec<Document>,
    written: Option<std::vec::IntoIter<Row>>,
}

impl Insert {
    fn new(collection: String, docs: Vec<Document>) -> Self {
        Insert { collection, docs, written: None }
    }
}

impl Operator for Insert {
    /*
    Writes the WHOLE batch on the first call, then drains the results.

    Inserting one document per next() looks more Volcano-ish and is much
    slower: Database::insert_doc opens the collection and flushes the pool
    twice per document, so a hundred-document batch paid that a hundred times.
    insert_docs pays it once. Measured through the server, batches of 10, 50
    and 200 all ran at the same per-document rate before this, which is the
    signature of fixed per-document cost dominating.

    Nothing is lost by buffering here. Insert is a leaf whose documents all
    come from the query text, so they are already materialized, and execute()
    drains the operator to completion regardless.
    */
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        if self.written.is_none() {
            let docs: Vec<Document> = self.docs.drain(..).collect();
            let doc_ids = ctx.db.insert_docs(&self.collection, &docs)?;
            let rows: Vec<Row> = doc_ids
                .into_iter()
                .zip(docs)
                .map(|(id, doc)| Row::stored(id, doc))
                .collect();
            self.written = Some(rows.into_iter());
        }
        Ok(self.written.as_mut().expect("just filled").next())
    }
}

/*
================================================================
STREAMING OPERATORS
================================================================
*/

/*
Filter: pass through only the rows whose predicate holds.

evaluate_bool is strict, so a non-boolean predicate is an error rather than a
silent false. `where age` (a typo for `where age > 0`) fails loudly instead of
returning a plausible empty result.
*/
struct Filter {
    input: Box<dyn Operator>,
    predicate: Expr,
}

impl Operator for Filter {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        while let Some(row) = self.input.next(ctx)? {
            if evaluate_bool(&self.predicate, &row.doc)? {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

/*
Projection: keep or compute the selected fields.

`items: None` is `select *` and passes the row through untouched, DocId
included. An explicit list builds a new document, so the DocId is dropped: the
result no longer corresponds to a stored record and must not be handed to a
later Update or Delete.
*/
struct Projection {
    input: Box<dyn Operator>,
    items: Option<Vec<SelectItem>>,
}

impl Operator for Projection {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        let Some(row) = self.input.next(ctx)? else {
            return Ok(None);
        };
        let Some(items) = &self.items else {
            return Ok(Some(row)); // select *
        };
        Ok(Some(Row::derived(project(items, &row.doc)?)))
    }
}

/*
project: apply select items to one document.

The output name is the alias when given, otherwise the field name for a plain
field reference. A computed expression with no alias has no obvious name, so
it gets a positional one rather than being dropped silently.
*/
fn project(items: &[SelectItem], doc: &Document) -> QueryResult<Document> {
    let mut out = Document::new();
    for (i, item) in items.iter().enumerate() {
        let name = match (&item.alias, &item.expr) {
            (Some(alias), _) => alias.clone(),
            (None, Expr::Field(f)) => f.clone(),
            (None, _) => format!("_{i}"),
        };
        out.insert(name, evaluate(&item.expr, doc)?);
    }
    Ok(out)
}

/*
Drop: remove named fields, keep everything else.

Its own operator rather than a Projection over the complement, because
computing the complement needs to know every field, and a schemaless
collection does not offer that.
*/
struct DropFields {
    input: Box<dyn Operator>,
    fields: Vec<String>,
}

impl Operator for DropFields {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        let Some(mut row) = self.input.next(ctx)? else {
            return Ok(None);
        };
        for field in &self.fields {
            row.doc.remove(field);
        }
        Ok(Some(row))
    }
}

/*
Limit: stop after n rows.

The stopping is the point. Once the count is reached it returns None WITHOUT
calling its child again, which is what lets the work below it end early.
*/
struct Limit {
    input: Box<dyn Operator>,
    remaining: u64,
}

impl Operator for Limit {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let row = self.input.next(ctx)?;
        if row.is_some() {
            self.remaining -= 1;
        }
        Ok(row)
    }
}

/// Offset: discard the first n rows, then pass everything through.
struct Offset {
    input: Box<dyn Operator>,
    to_skip: u64,
}

impl Operator for Offset {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        while self.to_skip > 0 {
            if self.input.next(ctx)?.is_none() {
                return Ok(None);
            }
            self.to_skip -= 1;
        }
        self.input.next(ctx)
    }
}

/*
================================================================
BUFFERING OPERATORS
================================================================
*/

/*
Sort: order every row before emitting the first.

Must buffer, unavoidably: the smallest value could be the last row read, so
there is no way to know what comes first until everything has been seen.

Ordering goes through the same compare_values the rest of the engine uses, so
a sort and a filter agree about what "less than" means. A missing field sorts
as Null.

compare_values returns an error for incomparable pairs. Sorting a column that
mixes strings and ints is a real error rather than an arbitrary order, so the
comparison failure is captured and reported after the sort rather than being
swallowed inside the comparator (which cannot return a Result).
*/
struct Sort {
    input: Box<dyn Operator>,
    keys: Vec<OrderKey>,
    buffered: Option<std::vec::IntoIter<Row>>,
}

impl Sort {
    fn new(input: Box<dyn Operator>, keys: Vec<OrderKey>) -> Self {
        Sort { input, keys, buffered: None }
    }
}

impl Operator for Sort {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        if self.buffered.is_none() {
            let mut rows = Vec::new();
            while let Some(row) = self.input.next(ctx)? {
                rows.push(row);
            }

            let mut failure: Option<QueryError> = None;
            rows.sort_by(|a, b| {
                for key in &self.keys {
                    let left = a.doc.get(&key.field).unwrap_or(&Value::Null);
                    let right = b.doc.get(&key.field).unwrap_or(&Value::Null);
                    match compare_values(left, right) {
                        Ok(std::cmp::Ordering::Equal) => continue,
                        Ok(ordering) => {
                            return match key.direction {
                                Direction::Asc => ordering,
                                Direction::Desc => ordering.reverse(),
                            }
                        }
                        Err(err) => {
                            failure.get_or_insert(err);
                            return std::cmp::Ordering::Equal;
                        }
                    }
                }
                std::cmp::Ordering::Equal
            });

            if let Some(err) = failure {
                return Err(err);
            }
            self.buffered = Some(rows.into_iter());
        }
        Ok(self.buffered.as_mut().expect("just filled").next())
    }
}

/*
HashAggregate: one output row per distinct group key.

Must buffer for the same reason Sort does: a group's count is not final until
every document has been seen.

Insertion order of first appearance is preserved rather than using a plain
HashMap iteration, so results are deterministic across runs. Non-deterministic
output makes tests flaky and makes a paginated query over an aggregate
meaningless.
*/
struct HashAggregate {
    input: Box<dyn Operator>,
    group_keys: Vec<String>,
    items: Vec<SelectItem>,
    buffered: Option<std::vec::IntoIter<Row>>,
}

impl HashAggregate {
    fn new(input: Box<dyn Operator>, group_keys: Vec<String>, items: Vec<SelectItem>) -> Self {
        HashAggregate { input, group_keys, items, buffered: None }
    }
}

impl Operator for HashAggregate {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        if self.buffered.is_none() {
            // key -> the documents in that group, plus the order groups first
            // appeared so output is deterministic.
            let mut groups: HashMap<String, Vec<Document>> = HashMap::new();
            let mut order: Vec<String> = Vec::new();

            while let Some(row) = self.input.next(ctx)? {
                let key = group_key(&self.group_keys, &row.doc);
                if !groups.contains_key(&key) {
                    order.push(key.clone());
                }
                groups.entry(key).or_default().push(row.doc);
            }

            let mut out = Vec::with_capacity(order.len());
            for key in order {
                let docs = &groups[&key];
                let mut doc = Document::new();
                for (i, item) in self.items.iter().enumerate() {
                    let (name, value) = aggregate_item(item, i, docs, &self.group_keys)?;
                    doc.insert(name, value);
                }
                out.push(Row::derived(doc));
            }
            self.buffered = Some(out.into_iter());
        }
        Ok(self.buffered.as_mut().expect("just filled").next())
    }
}

/*
group_key: a stable string identifying one group.

Formatting the values is enough because Value is not Hash (f64 is not), and a
debug rendering distinguishes Int(1) from String("1"), which is what matters.
*/
fn group_key(fields: &[String], doc: &Document) -> String {
    fields
        .iter()
        .map(|f| format!("{:?}", doc.get(f).unwrap_or(&Value::Null)))
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

/*
aggregate_item: evaluate one select item against a whole group.

An AggCall folds the group. Anything else is a grouped field, so it is read
from the first document: the binder guarantees a non-aggregate select item is
one of the group keys, which by definition has the same value in every
document of the group.
*/
fn aggregate_item(
    item: &SelectItem,
    position: usize,
    docs: &[Document],
    group_keys: &[String],
) -> QueryResult<(String, Value)> {
    if let Expr::AggCall { name, arg } = &item.expr {
        let label = item
            .alias
            .clone()
            .unwrap_or_else(|| match arg {
                Some(field) => format!("{name}_{field}"),
                None => name.clone(),
            });
        return Ok((label, fold_aggregate(name, arg.as_deref(), docs)?));
    }

    let name = match (&item.alias, &item.expr) {
        (Some(alias), _) => alias.clone(),
        (None, Expr::Field(f)) => f.clone(),
        (None, _) => group_keys
            .get(position)
            .cloned()
            .unwrap_or_else(|| format!("_{position}")),
    };
    let value = docs
        .first()
        .map(|doc| evaluate(&item.expr, doc))
        .transpose()?
        .unwrap_or(Value::Null);
    Ok((name, value))
}

/*
fold_aggregate: count / sum / avg / min / max / collect over one group.

Documents missing the field are SKIPPED rather than counted as zero, which is
what makes avg over a sparse field mean "average of the values that exist".
count is the exception: it counts documents, not values, so it ignores the
field entirely.
*/
fn fold_aggregate(name: &str, arg: Option<&str>, docs: &[Document]) -> QueryResult<Value> {
    if name == "count" {
        return Ok(Value::Int(docs.len() as i64));
    }

    let Some(field) = arg else {
        return Err(QueryError::Validation(format!(
            "aggregate {name} requires a field argument"
        )));
    };
    let values: Vec<&Value> = docs.iter().filter_map(|d| d.get(field)).collect();

    match name {
        "collect" => Ok(Value::Array(values.into_iter().cloned().collect())),
        "min" | "max" => {
            let mut best: Option<&Value> = None;
            for value in values {
                best = Some(match best {
                    None => value,
                    Some(current) => {
                        let ordering = compare_values(value, current)?;
                        let take = if name == "min" {
                            ordering == std::cmp::Ordering::Less
                        } else {
                            ordering == std::cmp::Ordering::Greater
                        };
                        if take { value } else { current }
                    }
                });
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        }
        "sum" | "avg" => {
            // an all-int group sums to an Int; any float makes it a Float.
            let mut int_total: i64 = 0;
            let mut float_total = 0.0f64;
            let mut saw_float = false;
            let mut count = 0usize;

            for value in values {
                count += 1;
                match value {
                    Value::Int(n) => {
                        int_total = int_total.checked_add(*n).ok_or_else(|| {
                            QueryError::Arithmetic(format!("integer overflow in {name}({field})"))
                        })?;
                        float_total += *n as f64;
                    }
                    Value::Float(x) => {
                        saw_float = true;
                        float_total += x;
                    }
                    other => {
                        return Err(QueryError::Type(format!(
                            "cannot {name} a {}",
                            crate::query::evaluate::value_name(other)
                        )))
                    }
                }
            }

            if count == 0 {
                return Ok(Value::Null);
            }
            if name == "avg" {
                return Ok(Value::Float(float_total / count as f64));
            }
            Ok(if saw_float {
                Value::Float(float_total)
            } else {
                Value::Int(int_total)
            })
        }
        other => Err(QueryError::Unsupported(format!("unknown aggregate: {other}"))),
    }
}

/*
HashJoin: inner join on one field from each side.

Buffers only the BUILD side (left) into a hash table, then streams the probe
side against it, so memory is proportional to the left input rather than to
both.

Output documents merge the two, with the RIGHT side winning on a field-name
collision. The merged row is derived, so it carries no DocId: it does not
correspond to a single stored record on either side.
*/
struct HashJoin {
    left: Box<dyn Operator>,
    right: Box<dyn Operator>,
    left_field: String,
    right_field: String,
    table: Option<Vec<(Value, Document)>>,
    pending: Vec<Row>,
}

impl HashJoin {
    fn new(
        left: Box<dyn Operator>,
        right: Box<dyn Operator>,
        left_field: String,
        right_field: String,
    ) -> Self {
        HashJoin {
            left,
            right,
            left_field,
            right_field,
            table: None,
            pending: Vec::new(),
        }
    }
}

impl Operator for HashJoin {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        if self.table.is_none() {
            let mut built = Vec::new();
            while let Some(row) = self.left.next(ctx)? {
                let key = row.doc.get(&self.left_field).cloned().unwrap_or(Value::Null);
                built.push((key, row.doc));
            }
            self.table = Some(built);
        }

        loop {
            if let Some(row) = self.pending.pop() {
                return Ok(Some(row));
            }
            let Some(right) = self.right.next(ctx)? else {
                return Ok(None);
            };
            let key = right.doc.get(&self.right_field).cloned().unwrap_or(Value::Null);

            // Null never matches, following the same rule comparisons use.
            if key == Value::Null {
                continue;
            }
            let table = self.table.as_ref().expect("just built");
            for (left_key, left_doc) in table {
                if values_equal(left_key, &key) {
                    let mut merged = left_doc.clone();
                    for (k, v) in &right.doc {
                        merged.insert(k.clone(), v.clone());
                    }
                    self.pending.push(Row::derived(merged));
                }
            }
        }
    }
}

/*
================================================================
MUTATING OPERATORS
================================================================
*/

/*
Update: apply assignments to each row the pipeline selected.

Implemented as delete-then-insert rather than an in-place write, because the
storage layer has no update primitive: records are variable length, so a
changed document may not fit where the old one was. Going through
Database::delete_doc and insert_doc also keeps the indexes correct on both
halves, which an in-place heap write would not.

CONSEQUENCE: the DocId changes. Documented rather than hidden, because a
caller holding an id from before the update will not find the record.

A row with no DocId cannot be written back. That happens when a projection
came first, and it is a planner error rather than a user one, so it reports
clearly instead of silently updating nothing.
*/
struct Update {
    input: Box<dyn Operator>,
    collection: String,
    assignments: Vec<Assignment>,
}

impl Operator for Update {
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        let Some(row) = self.input.next(ctx)? else {
            return Ok(None);
        };
        let Some(doc_id) = row.doc_id else {
            return Err(QueryError::InvalidPipeline(
                "update needs stored rows; a projection before update discards their identity"
                    .to_string(),
            ));
        };

        let mut updated = row.doc.clone();
        for assignment in &self.assignments {
            // evaluated against the ORIGINAL document, so `set a = b, b = a`
            // swaps rather than assigning b to itself.
            let value = evaluate(&assignment.value, &row.doc)?;
            updated.insert(assignment.field.clone(), value);
        }

        ctx.db.delete_doc(&self.collection, doc_id)?;
        let new_id = ctx.db.insert_doc(&self.collection, &updated)?;
        Ok(Some(Row::stored(new_id, updated)))
    }
}

/*
Delete: remove each row the pipeline selected.

Goes through Database::delete_doc, which reads the document before tombstoning
it so the index entries can be removed. Doing it the other way round would
leave index entries pointing at a dead DocId.
*/
struct Delete {
    input: Box<dyn Operator>,
    collection: String,
    deleted: Option<std::vec::IntoIter<Row>>,
}

impl Operator for Delete {
    /*
    Drains the input, then deletes the whole matched set in ONE batch.

    Same reasoning as Insert, and it matters more here because of the TTL
    sweeper: it deletes every aged-out document in a single statement while
    holding the server's global mutex, so the cost is time during which no
    telemetry can be written. Deleting one at a time cost 0.86s for 2400
    documents, which does not scale to a seven-day retention window.

    Buffering is also what makes deleting safe while iterating. Removing rows
    from a collection the input is still scanning is the classic way to skip
    documents or read freed pages; collecting the ids first means the scan has
    finished before anything is removed.
    */
    fn next(&mut self, ctx: &mut ExecContext) -> QueryResult<Option<Row>> {
        if self.deleted.is_none() {
            let mut rows = Vec::new();
            let mut doc_ids = Vec::new();
            while let Some(row) = self.input.next(ctx)? {
                let Some(doc_id) = row.doc_id else {
                    return Err(QueryError::InvalidPipeline(
                        "delete needs stored rows; a projection before delete discards their identity"
                            .to_string(),
                    ));
                };
                doc_ids.push(doc_id);
                rows.push(row);
            }
            ctx.db.delete_docs(&self.collection, &doc_ids)?;
            self.deleted = Some(rows.into_iter());
        }
        Ok(self.deleted.as_mut().expect("just filled").next())
    }
}
