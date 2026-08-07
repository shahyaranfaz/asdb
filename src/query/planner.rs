/*
planner.rs: bound pipeline -> physical operator tree.

plans.txt 5.5. The planner decides HOW to answer a query. In v1 there is
exactly one decision worth making and everything else is a mechanical
one-stage-to-one-operator mapping.

NO COST MODEL. That is deliberate for v1, not an omission. A cost model needs
statistics (row counts, value distributions, index selectivity) that asdb does
not collect, and guessing without them is worse than a simple deterministic
rule: at least the rule is predictable and explainable when a query is slow.
*/

use super::{QueryError, QueryResult};
use crate::asl::{BinOp, Expr, Pipeline, SelectItem, Stage};
use crate::database::Database;
use crate::query::evaluate::{doc_literal_to_document, literal_to_value};
use crate::document::Value;
use crate::query::plan::{PhysicalOp, ScanBounds};

/*
plan: a bound Pipeline -> a PhysicalOp tree.

BUILD BOTTOM-UP. The source stage becomes the leaf, then each subsequent
stage wraps what has been built so far, so the final tree reads inside-out
relative to the query text. See plan.rs for the worked example.

`db` is needed for one thing only: asking whether a field has an index. The
binder already validated names, so nothing here re-checks existence.

An Insert pipeline is the one shape that does not start from a scan: its
documents come from the query text, so it produces a leaf directly and the
From stage names the target rather than a source.
*/
pub fn plan(db: &Database, pipeline: &Pipeline) -> QueryResult<PhysicalOp> {
    let Some(Stage::From { collection, .. }) = pipeline.first() else {
        return Err(QueryError::InvalidPipeline(
            "pipeline must start with a from stage".to_string(),
        ));
    };

    // An insert anywhere in the pipeline makes this a write of literals, not a
    // read. Detected up front because it changes what the leaf is.
    if let Some(Stage::Insert { docs }) = pipeline.iter().find(|s| matches!(s, Stage::Insert { .. })) {
        let mut out = Vec::with_capacity(docs.len());
        for lit in docs {
            out.push(doc_literal_to_document(lit)?);
        }
        return Ok(PhysicalOp::Insert { collection: collection.clone(), docs: out });
    }

    /*
    The index rule needs the Where predicate BEFORE the leaf is built, because
    it decides whether the leaf is a SeqScan or an IndexScan. So the predicate
    is looked up first, the leaf is built from it, and plan_source reports back
    which conjuncts it absorbed so the Filter can skip them.
    */
    let predicate = pipeline.iter().find_map(|s| match s {
        Stage::Where { expr } => Some(expr),
        _ => None,
    });

    let (mut tree, absorbed) = plan_source(db, collection, predicate);

    for stage in pipeline.iter().skip(1) {
        match stage {
            Stage::Where { expr } => {
                // whatever the index already enforced exactly is dropped here.
                if let Some(residual) = residual_predicate(expr, &absorbed) {
                    tree = PhysicalOp::Filter { input: Box::new(tree), predicate: residual };
                }
            }
            /*
            Group absorbs the Select that follows it. Handled by looking
            forward from Group rather than backward from Select, so the Select
            arm below can assume it is a plain projection.
            */
            Stage::Group { fields } => {
                let items = match next_select_items(pipeline, stage) {
                    Some(items) => items,
                    None => {
                        return Err(QueryError::InvalidPipeline(
                            "group must be followed by a select".to_string(),
                        ))
                    }
                };
                tree = PhysicalOp::HashAggregate {
                    input: Box::new(tree),
                    group_keys: fields.clone(),
                    items,
                };
            }
            Stage::Select { items } => {
                // already folded into the HashAggregate above
                if follows_group(pipeline, stage) {
                    continue;
                }
                tree = PhysicalOp::Projection { input: Box::new(tree), items: items.clone() };
            }
            other => tree = plan_stage(tree, other)?,
        }
    }

    Ok(tree)
}

/*
residual_predicate: what the Filter still has to test.

Rebuilds the predicate from the conjuncts the index did NOT fully enforce.
Returns None when the index enforced all of them, which is the case where the
Filter disappears entirely.
*/
fn residual_predicate(expr: &Expr, absorbed: &[usize]) -> Option<Expr> {
    let conjuncts = split_conjuncts(expr);
    let kept: Vec<&Expr> = conjuncts
        .iter()
        .enumerate()
        .filter(|(i, _)| !absorbed.contains(i))
        .map(|(_, e)| *e)
        .collect();

    let mut iter = kept.into_iter();
    let first = iter.next()?.clone();
    Some(iter.fold(first, |acc, next| {
        Expr::BinOp(BinOp::And, Box::new(acc), Box::new(next.clone()))
    }))
}

/// The select items of the Select stage immediately after `stage`, if any.
fn next_select_items(pipeline: &Pipeline, stage: &Stage) -> Option<Vec<SelectItem>> {
    let idx = pipeline.iter().position(|s| std::ptr::eq(s, stage))?;
    match pipeline.get(idx + 1) {
        Some(Stage::Select { items }) => Some(items.clone().unwrap_or_default()),
        _ => None,
    }
}

/// True when `stage` is a Select directly preceded by a Group.
fn follows_group(pipeline: &Pipeline, stage: &Stage) -> bool {
    let Some(idx) = pipeline.iter().position(|s| std::ptr::eq(s, stage)) else {
        return false;
    };
    idx > 0 && matches!(pipeline.get(idx - 1), Some(Stage::Group { .. }))
}

/*
plan_source: the From stage -> the leaf operator.

THE ONE RULE (index selection):

  If the Where predicate contains an equality or range test on a field that
  has an index, emit IndexScan with those bounds instead of SeqScan, and
  REMOVE that conjunct from the Filter above it. Otherwise SeqScan plus the
  full Filter.

  The "remove that conjunct" half is easy to forget and costs real work if you
  do. Leaving it in is not WRONG (the rows already satisfy it, so the filter
  passes them) but it re-tests every row for nothing, which quietly erases
  part of the benefit of using the index at all.

  BUT removing it is only valid for an INCLUSIVE bound:

      age >= 18   push low = 18, REMOVE the conjunct. The index enforced it
                  exactly.
      age >  18   push low = 18 anyway, KEEP the conjunct. The index
                  over-fetches the age == 18 rows and the Filter drops them.

  Same bound emitted either way, because an index scan only owes the caller a
  cheap superset. See the ScanBounds invariant in plan.rs.

  Only conjuncts joined by `and` can be pushed down. `where age > 18 or
  country == "CA"` cannot use an index on age, because the or-branch still
  needs every other row. Splitting on `and` and testing each conjunct is the
  whole algorithm.

Returns the leaf plus the INDICES of the conjuncts it absorbed, so the caller
can rebuild the Filter without them. Returning indices rather than expressions
avoids comparing Exprs for equality, which would misbehave when a predicate
repeats the same test twice.

ONE-SIDED BOUNDS ARE CLOSED SYNTHETICALLY, see close_bounds. `age >= 18` gets
a high end of i64::MAX rather than falling back to a full scan. This matters
most for the TTL sweeper, whose query is exactly one-sided:

    from telemtry_snapshots | where receivedAt < <cutoff> | delete

Without it that is a full scan of the collection every sweep, forever. With
it, it touches only the expired range.
*/
fn plan_source(db: &Database, collection: &str, predicate: Option<&Expr>) -> (PhysicalOp, Vec<usize>) {
    let seq = PhysicalOp::SeqScan { collection: collection.to_string() };
    let Some(predicate) = predicate else {
        return (seq, Vec::new());
    };

    for (i, conjunct) in split_conjuncts(predicate).iter().enumerate() {
        let Some(field) = indexed_field_of(conjunct) else {
            continue;
        };
        if !db.has_index(collection, &field) {
            continue;
        }
        let Some((bounds, exact)) = as_index_bound(conjunct, &field) else {
            continue;
        };
        let Some(bounds) = close_bounds(bounds) else {
            continue;
        };

        let scan = PhysicalOp::IndexScan {
            collection: collection.to_string(),
            field,
            bounds,
        };
        // only drop the conjunct when the index enforced it EXACTLY. An
        // over-fetching bound must keep its conjunct or the answer is wrong.
        let absorbed = if exact { vec![i] } else { Vec::new() };
        return (scan, absorbed);
    }

    (seq, Vec::new())
}

/// The field name a comparison tests, when one side is a plain field.
fn indexed_field_of(expr: &Expr) -> Option<String> {
    let Expr::BinOp(_, left, right) = expr else {
        return None;
    };
    match (left.as_ref(), right.as_ref()) {
        (Expr::Field(f), Expr::Lit(_)) | (Expr::Lit(_), Expr::Field(f)) => Some(f.clone()),
        _ => None,
    }
}

/*
split_conjuncts: flatten an `and` chain into its parts.

`a and b and c` parses as a leaning tree of BinOp::And. The index rule needs
the parts as a flat list so it can test each one for pushdown, so this walks
the And spine and collects the leaves. Anything that is not an And is a single
conjunct and comes back as a one-element list.

It deliberately does NOT descend into Or. An or-branch cannot be pushed down,
so treating `a or b` as one opaque conjunct is exactly right.
*/
fn split_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinOp(BinOp::And, left, right) => {
            let mut out = split_conjuncts(left);
            out.extend(split_conjuncts(right));
            out
        }
        other => vec![other],
    }
}

/*
as_index_bound: can this single conjunct drive an index scan on `field`?

Returns the bounds to push down, and whether those bounds enforce the conjunct
EXACTLY (so the planner can drop it from the residual Filter).

The bool is the "inclusive" question from plan.rs, answered once here rather
than carried around inside the plan:

    field == v    exact bounds,  fully enforced -> true
    field >= v    low = v,       fully enforced -> true
    field >  v    low = v,       over-fetches   -> false
    field <= v    high = v,      fully enforced -> true
    field <  v    high = v,      over-fetches   -> false

Comparisons are only pushable when one side is the indexed field and the other
is a literal. `where a > b` over two fields cannot use an index on either.
*/
fn as_index_bound(expr: &Expr, field: &str) -> Option<(ScanBounds, bool)> {
    let Expr::BinOp(op, left, right) = expr else {
        return None;
    };

    // normalise so the field is on the left: `5 < age` becomes `age > 5`.
    let (op, value) = match (left.as_ref(), right.as_ref()) {
        (Expr::Field(f), Expr::Lit(lit)) if f == field => (*op, literal_to_value(lit)),
        (Expr::Lit(lit), Expr::Field(f)) if f == field => (flip(*op), literal_to_value(lit)),
        _ => return None,
    };

    match op {
        BinOp::Eq => Some((ScanBounds::exact(value), true)),
        BinOp::GtEq => Some((ScanBounds { low: Some(value), high: None }, true)),
        BinOp::Gt => Some((ScanBounds { low: Some(value), high: None }, false)),
        BinOp::LtEq => Some((ScanBounds { low: None, high: Some(value) }, true)),
        BinOp::Lt => Some((ScanBounds { low: None, high: Some(value) }, false)),
        _ => None,
    }
}


/*
close_bounds: give a one-sided range a concrete other end.

The btree's range_scan takes two concrete keys, so an open end needs a
sentinel. The key encoding supplies one for free: keys are TYPE-TAGGED and
order preserving (see document/key.rs), so the smallest and largest key of a
given type bracket every value of that type and nothing else. An Int bound
therefore closes with i64::MIN or i64::MAX, and a Float bound with the
infinities.

CAVEAT, and it is inherited rather than introduced: because the sentinel
carries a type tag, the scan only covers values of THAT type. If an indexed
field holds Ints in some documents and Floats in others, a range scan bounded
by Ints will not see the Floats. That is already true of the two-sided path
(`where age >= 5 and age <= 10` has the same blind spot), so this does not
widen the exposure, but it is a real property of indexing a schemaless field
and it is why mixed-type indexed columns should be avoided.

Strings return None and keep the sequential-scan fallback. There is no
greatest string, and inventing one (a long run of 0xFF) would be a guess that
silently truncates real data above it.
*/
fn close_bounds(bounds: ScanBounds) -> Option<ScanBounds> {
    match (&bounds.low, &bounds.high) {
        (Some(_), Some(_)) => Some(bounds),
        (Some(low), None) => type_max(low).map(|high| ScanBounds { low: bounds.low.clone(), high: Some(high) }),
        (None, Some(high)) => type_min(high).map(|low| ScanBounds { low: Some(low), high: bounds.high.clone() }),
        (None, None) => None,
    }
}

fn type_min(like: &Value) -> Option<Value> {
    match like {
        Value::Int(_) => Some(Value::Int(i64::MIN)),
        Value::Float(_) => Some(Value::Float(f64::NEG_INFINITY)),
        _ => None,
    }
}

fn type_max(like: &Value) -> Option<Value> {
    match like {
        Value::Int(_) => Some(Value::Int(i64::MAX)),
        Value::Float(_) => Some(Value::Float(f64::INFINITY)),
        _ => None,
    }
}

/// Mirror a comparison so its operands can be swapped: `5 < age` == `age > 5`.
fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Gt => BinOp::Lt,
        BinOp::GtEq => BinOp::LtEq,
        BinOp::Lt => BinOp::Gt,
        BinOp::LtEq => BinOp::GtEq,
        other => other, // Eq and NotEq are symmetric
    }
}

/*
STAGE MAPPING (everything except the source, all mechanical):

    Where     -> Filter
    Select    -> Projection { items }
    Drop      -> Drop { fields }
    Order     -> Sort
    Limit     -> Limit
    Offset    -> Offset
    Group     -> HashAggregate, ABSORBING the Select that follows it
    Join      -> HashJoin, with the tree so far as the build side
    Insert    -> Insert (a leaf, it has no input)
    Update    -> Update wrapping the tree so far
    Delete    -> Delete wrapping the tree so far

Group is the one stage that is not one-to-one: `group status select status,
count` becomes a single HashAggregate, not an aggregate followed by a
projection. The binder already guaranteed that Select is well-formed for the
grouping, so this can fold them without re-checking.

Where, Group and Select are handled in plan() itself, because each needs to
see more than the current stage: Where consults what the index absorbed, and
Group and Select have to agree about which of them owns the projection.
*/
fn plan_stage(input: PhysicalOp, stage: &Stage) -> QueryResult<PhysicalOp> {
    let collection = input.collection_name().unwrap_or_default().to_string();
    let input = Box::new(input);

    let op = match stage {
        Stage::Drop { fields } => PhysicalOp::Drop { input, fields: fields.clone() },
        Stage::Order { keys } => PhysicalOp::Sort { input, keys: keys.clone() },

        // the binder rejects negatives, so this is defence in depth rather
        // than the primary check.
        Stage::Limit { n } => PhysicalOp::Limit { input, n: non_negative(*n, "limit")? },
        Stage::Offset { n } => PhysicalOp::Offset { input, n: non_negative(*n, "offset")? },

        Stage::Join { collection: right, left_field, right_field } => PhysicalOp::HashJoin {
            left: input,
            right: Box::new(PhysicalOp::SeqScan { collection: right.clone() }),
            left_field: left_field.clone(),
            right_field: right_field.clone(),
        },

        Stage::Update { assignments } => PhysicalOp::Update {
            input,
            collection,
            assignments: assignments.clone(),
        },
        Stage::Delete => PhysicalOp::Delete { input, collection },

        Stage::From { .. } => {
            return Err(QueryError::InvalidPipeline(
                "from may only appear as the first stage".to_string(),
            ))
        }
        Stage::Where { .. } | Stage::Select { .. } | Stage::Group { .. } | Stage::Insert { .. } => {
            unreachable!("handled in plan()")
        }
    };
    Ok(op)
}

fn non_negative(n: i64, what: &str) -> QueryResult<u64> {
    u64::try_from(n).map_err(|_| {
        QueryError::Validation(format!("{what} must not be negative, got {n}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asl::LitValue;
    use crate::document::Value;

    fn field(name: &str) -> Expr {
        Expr::Field(name.to_string())
    }

    fn int(n: i64) -> Expr {
        Expr::Lit(LitValue::Int(n))
    }

    fn bin(op: BinOp, left: Expr, right: Expr) -> Expr {
        Expr::BinOp(op, Box::new(left), Box::new(right))
    }

    #[test]
    fn test_split_conjuncts_flattens_and_chains() {
        let expr = bin(
            BinOp::And,
            bin(
                BinOp::And,
                bin(BinOp::Gt, field("a"), int(1)),
                bin(BinOp::Lt, field("b"), int(2)),
            ),
            bin(BinOp::Eq, field("c"), int(3)),
        );
        assert_eq!(split_conjuncts(&expr).len(), 3);
    }

    #[test]
    fn test_split_conjuncts_does_not_descend_into_or() {
        // an or-branch is one opaque conjunct: it cannot be pushed down.
        let expr = bin(
            BinOp::Or,
            bin(BinOp::Gt, field("a"), int(1)),
            bin(BinOp::Lt, field("b"), int(2)),
        );
        assert_eq!(split_conjuncts(&expr).len(), 1);
    }

    #[test]
    fn test_inclusive_bound_is_fully_enforced() {
        let (bounds, exact) =
            as_index_bound(&bin(BinOp::GtEq, field("age"), int(18)), "age").unwrap();
        assert_eq!(bounds.low, Some(Value::Int(18)));
        assert_eq!(bounds.high, None);
        assert!(exact, ">= is enforced exactly by the index, so drop the conjunct");
    }

    #[test]
    fn test_exclusive_bound_pushes_the_same_value_but_is_not_exact() {
        // the case that makes a flag-free ScanBounds correct: `age > 18`
        // pushes low = 18, identical to `>=`, and relies on the Filter above
        // to drop the age == 18 rows the index over-fetches.
        let (bounds, exact) =
            as_index_bound(&bin(BinOp::Gt, field("age"), int(18)), "age").unwrap();
        assert_eq!(bounds.low, Some(Value::Int(18)));
        assert!(!exact, "> over-fetches, so the conjunct must be KEPT");
    }

    #[test]
    fn test_equality_produces_closed_bounds() {
        let (bounds, exact) =
            as_index_bound(&bin(BinOp::Eq, field("age"), int(18)), "age").unwrap();
        assert_eq!(bounds, ScanBounds::exact(Value::Int(18)));
        assert!(exact);
    }

    #[test]
    fn test_reversed_operands_are_normalised() {
        // `5 < age` must behave as `age > 5`, not be rejected.
        let (bounds, exact) =
            as_index_bound(&bin(BinOp::Lt, int(5), field("age")), "age").unwrap();
        assert_eq!(bounds.low, Some(Value::Int(5)));
        assert!(!exact);
    }

    #[test]
    fn test_non_pushable_predicates_are_rejected() {
        // wrong field
        assert!(as_index_bound(&bin(BinOp::Gt, field("name"), int(1)), "age").is_none());
        // field vs field, no literal to bound with
        assert!(as_index_bound(&bin(BinOp::Gt, field("age"), field("other")), "age").is_none());
        // not a comparison
        assert!(as_index_bound(&bin(BinOp::Add, field("age"), int(1)), "age").is_none());
    }

    /*
    The plan-shape assertions (unindexed -> SeqScan + Filter, indexed -> a
    bare IndexScan with the conjunct dropped) need a real Database, so they
    live in tests/query_end_to_end.rs alongside the query that proves both
    paths return the same answer.
    */
}
