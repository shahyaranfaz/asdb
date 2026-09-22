/*
plan.rs: the physical operator tree, as data.

Produced by planner.rs, consumed by executor.rs. It is a separate module from
both so the executor does not have to depend on planning logic, and so a plan
can be built by hand in a test without going anywhere near the binder.

WHY A TREE AND NOT A LIST OF STAGES

A bound pipeline is a flat sequence. The plan is nested, built bottom-up, with
each stage wrapping what came before it:

    from users            ->  SeqScan { users }
    where age > 18        ->  Filter  { input: SeqScan }
    order name asc        ->  Sort    { input: Filter }
    limit 25              ->  Limit   { input: Sort }

giving Limit(Sort(Filter(SeqScan))). Execution pulls from the root and demand
flows down to the leaf, which is why the tree reads inside-out relative to the
query text.

Children are Box<PhysicalOp> for the same reason Expr boxes itself: a
recursive enum needs indirection or it has no finite size.
*/

use crate::asl::{Assignment, DocLiteral, Expr, OrderKey, SelectItem};
use crate::document::{Document, Value};

/*
ScanBounds: the key range an IndexScan walks.

INVARIANT: BOTH ENDS ARE ALWAYS INCLUSIVE. None means unbounded on that side.

There is no `inclusive: bool` here on purpose, and it is worth writing down
why, because adding one looks like an obvious improvement.

An index scan does not have to return exactly the matching rows. It only has
to return a cheap SUPERSET, because the Filter above it re-tests everything it
is handed. So an exclusive predicate can push down an inclusive bound and let
the Filter clean up:

    age >= 18   low = Some(18), and the conjunct can be DROPPED from the
                Filter, because the index enforced it exactly
    age >  18   low = Some(18) as well, but the conjunct must be KEPT. The
                index over-fetches the age == 18 rows and the Filter drops
                them.

Same bound in both cases. The thing that differs is a decision the planner
makes and acts on immediately, not a property the executor ever needs to read
back, so carrying a flag here would be carrying information nobody consumes.

The one combination that is silently wrong is pushing an EXCLUSIVE bound down
and also dropping the conjunct: `age > 29` would start returning age == 29
rows, with no error, just a bad answer. Keeping bounds unconditionally
inclusive makes that combination unrepresentable.
*/
#[derive(Clone, Debug, PartialEq)]
pub struct ScanBounds {
    pub low: Option<Value>,
    pub high: Option<Value>,
}

impl ScanBounds {
    pub fn unbounded() -> Self {
        ScanBounds { low: None, high: None }
    }

    /// Exact-match lookup, the `field == value` case.
    pub fn exact(value: Value) -> Self {
        ScanBounds { low: Some(value.clone()), high: Some(value) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PhysicalOp {
    /*
    Leaves. Neither has an input; they are where documents enter the tree.
    */
    SeqScan {
        collection: String,
    },
    IndexScan {
        collection: String,
        field: String,
        bounds: ScanBounds,
    },

    /*
    Single-input operators.
    */
    Filter {
        input: Box<PhysicalOp>,
        predicate: Expr,
    },
    Projection {
        input: Box<PhysicalOp>,
        /*
        None means `select *`, matching Stage::Select's own shape. Keeping the
        distinction rather than expanding the star here means the executor can
        pass the document through untouched, which also does the right thing
        for schemaless collections where the planner cannot know the columns.
        */
        items: Option<Vec<SelectItem>>,
    },
    /*
    Drop is its own operator rather than a Projection with the complement,
    for the same reason: computing the complement needs a schema, and there
    may not be one.
    */
    Drop {
        input: Box<PhysicalOp>,
        fields: Vec<String>,
    },
    Sort {
        input: Box<PhysicalOp>,
        keys: Vec<OrderKey>,
    },
    Limit {
        input: Box<PhysicalOp>,
        n: u64,
    },
    Offset {
        input: Box<PhysicalOp>,
        n: u64,
    },
    HashAggregate {
        input: Box<PhysicalOp>,
        group_keys: Vec<String>,
        /*
        The select items from the Select stage that FOLLOWS the group. The
        planner folds those two stages into this one operator, because
        `group status select status, count` is a single aggregation, not an
        aggregation followed by a projection.
        */
        items: Vec<SelectItem>,
    },

    /*
    The only two-input operator, and the reason Collection handles have to be
    able to coexist. See tests/multi_collection.rs.

    `left` is the build side: it is drained into the hash table first, then
    `right` is streamed against it.
    */
    HashJoin {
        left: Box<PhysicalOp>,
        right: Box<PhysicalOp>,
        left_field: String,
        right_field: String,
    },

    /*
    Mutating operators.

    Insert is a LEAF: its documents come from the query text, not from an
    input. Update and Delete wrap an input, because the rows they act on are
    whatever the pipeline above them selected.
    */
    Insert {
        collection: String,
        docs: Vec<Document>,
    },
    Update {
        input: Box<PhysicalOp>,
        collection: String,
        assignments: Vec<Assignment>,
    },
    /*
    Upsert: update every row the input produced with this document's fields,
    or insert the document when the input produced none.

    It wraps the input rather than being a leaf like Insert, because deciding
    between the two paths means knowing whether anything matched.
    */
    Upsert {
        input: Box<PhysicalOp>,
        collection: String,
        doc: DocLiteral,
    },
    Delete {
        input: Box<PhysicalOp>,
        collection: String,
    },
}

impl PhysicalOp {
    /*
    collection_name: which collection this subtree ultimately reads from.

    Walks down to the leaf. The executor needs this to open the right heap,
    and Update/Delete need it to write back. Returns None for a HashJoin,
    which has two leaves and therefore no single answer.
    */
    pub fn collection_name(&self) -> Option<&str> {
        match self {
            PhysicalOp::SeqScan { collection }
            | PhysicalOp::IndexScan { collection, .. }
            | PhysicalOp::Insert { collection, .. }
            | PhysicalOp::Update { collection, .. }
            | PhysicalOp::Upsert { collection, .. }
            | PhysicalOp::Delete { collection, .. } => Some(collection),

            PhysicalOp::Filter { input, .. }
            | PhysicalOp::Projection { input, .. }
            | PhysicalOp::Drop { input, .. }
            | PhysicalOp::Sort { input, .. }
            | PhysicalOp::Limit { input, .. }
            | PhysicalOp::Offset { input, .. }
            | PhysicalOp::HashAggregate { input, .. } => input.collection_name(),

            PhysicalOp::HashJoin { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_bounds_are_closed_on_both_ends() {
        let b = ScanBounds::exact(Value::Int(18));
        assert_eq!(b.low, Some(Value::Int(18)));
        assert_eq!(b.high, Some(Value::Int(18)));
    }

    #[test]
    fn test_collection_name_walks_to_the_leaf() {
        let plan = PhysicalOp::Limit {
            n: 25,
            input: Box::new(PhysicalOp::Filter {
                predicate: Expr::Field("age".to_string()),
                input: Box::new(PhysicalOp::SeqScan { collection: "users".to_string() }),
            }),
        };
        assert_eq!(plan.collection_name(), Some("users"));
    }

    #[test]
    fn test_join_has_no_single_collection() {
        let plan = PhysicalOp::HashJoin {
            left: Box::new(PhysicalOp::SeqScan { collection: "users".to_string() }),
            right: Box::new(PhysicalOp::SeqScan { collection: "orders".to_string() }),
            left_field: "id".to_string(),
            right_field: "user_id".to_string(),
        };
        assert_eq!(plan.collection_name(), None);
    }
}
