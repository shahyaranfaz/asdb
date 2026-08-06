use super::{QueryError, QueryResult};

use crate::asl::{BinOp, DocLiteral, Expr, LitValue, StringMatchOp, UnaryOp};
use crate::document::{Document, Value};

use std::cmp::Ordering;

pub fn literal_to_value(lit: &LitValue) -> Value {
    match lit {
        LitValue::Int(n) => Value::Int(*n),
        LitValue::Float(n) => Value::Float(*n),
        LitValue::String(s) => Value::String(s.clone()),
        LitValue::Bool(b) => Value::Bool(*b),
        LitValue::Null => Value::Null,
    }
}

pub fn doc_literal_to_document(lit: &DocLiteral) -> QueryResult<Document> {
    let mut doc = Document::new();
    for (field, expr) in &lit.fields {
        if doc.contains_key(field) {
            return Err(QueryError::Validation(format!(
                "duplicate document field: {field}"
            )));
        }
        doc.insert(field.clone(), evaluate_literal_expr(expr)?);
    }
    Ok(doc)
}

pub fn evaluate(expr: &Expr, doc: &Document) -> QueryResult<Value> {
    match expr {
        Expr::Field(name) => Ok(doc.get(name).cloned().unwrap_or(Value::Null)),
        Expr::Lit(lit) => Ok(literal_to_value(lit)),
        Expr::BinOp(op, left, right) => evaluate_bin_op(*op, left, right, doc),
        Expr::UnaryOp(op, expr) => evaluate_unary_op(*op, expr, doc),
        Expr::In { field, values, negated } => {
            let left = doc.get(field).cloned().unwrap_or(Value::Null);
            let found = values.iter().any(|value| values_equal(&left, &literal_to_value(value)));
            Ok(Value::Bool(if *negated { !found } else { found }))
        }
        Expr::Exists(field) => Ok(Value::Bool(doc.contains_key(field))),
        Expr::Missing(field) => Ok(Value::Bool(!doc.contains_key(field))),
        Expr::StringMatch { field, op, pattern } => {
            let Some(Value::String(value)) = doc.get(field) else {
                return Ok(Value::Bool(false));
            };
            let matched = match op {
                StringMatchOp::Contains => value.contains(pattern),
                StringMatchOp::Starts => value.starts_with(pattern),
                StringMatchOp::Ends => value.ends_with(pattern),
            };
            Ok(Value::Bool(matched))
        }
        Expr::Ternary { cond, then_branch, else_branch } => {
            if evaluate_bool(cond, doc)? {
                evaluate(then_branch, doc)
            } else {
                evaluate(else_branch, doc)
            }
        }
        Expr::ArrayLit(values) => {
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                out.push(evaluate(value, doc)?);
            }
            Ok(Value::Array(out))
        }
        Expr::DocLit(lit) => {
            let mut out = Document::new();
            for (field, value) in &lit.fields {
                out.insert(field.clone(), evaluate(value, doc)?);
            }
            Ok(Value::Document(out))
        }
        Expr::AggCall { name, .. } => Err(QueryError::Unsupported(format!(
            "aggregate evaluation is not implemented yet: {name}"
        ))),
    }
}

pub fn evaluate_bool(expr: &Expr, doc: &Document) -> QueryResult<bool> {
    match evaluate(expr, doc)? {
        Value::Bool(value) => Ok(value),
        other => Err(QueryError::Type(format!(
            "expected bool expression, got {}",
            value_name(&other)
        ))),
    }
}

fn evaluate_literal_expr(expr: &Expr) -> QueryResult<Value> {
    match expr {
        Expr::Lit(lit) => Ok(literal_to_value(lit)),
        Expr::ArrayLit(values) => {
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                out.push(evaluate_literal_expr(value)?);
            }
            Ok(Value::Array(out))
        }
        Expr::DocLit(lit) => Ok(Value::Document(doc_literal_to_document(lit)?)),
        _ => Err(QueryError::Unsupported(
            "insert documents only support literal values for now".to_string(),
        )),
    }
}

fn evaluate_bin_op(op: BinOp, left: &Expr, right: &Expr, doc: &Document) -> QueryResult<Value> {
    match op {
        BinOp::And => Ok(Value::Bool(evaluate_bool(left, doc)? && evaluate_bool(right, doc)?)),
        BinOp::Or => Ok(Value::Bool(evaluate_bool(left, doc)? || evaluate_bool(right, doc)?)),
        BinOp::Eq | BinOp::NotEq | BinOp::Gt | BinOp::GtEq | BinOp::Lt | BinOp::LtEq => {
            let left = evaluate(left, doc)?;
            let right = evaluate(right, doc)?;
            evaluate_comparison(op, &left, &right)
        }
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            let left = evaluate(left, doc)?;
            let right = evaluate(right, doc)?;
            evaluate_arithmetic(op, &left, &right)
        }
    }
}

fn evaluate_unary_op(op: UnaryOp, expr: &Expr, doc: &Document) -> QueryResult<Value> {
    match op {
        UnaryOp::Not => Ok(Value::Bool(!evaluate_bool(expr, doc)?)),
        UnaryOp::Neg => match evaluate(expr, doc)? {
            // checked for the same reason the binary ops are: -(i64::MIN) has
            // no positive counterpart, and plain `-n` panics on it.
            Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(|| {
                QueryError::Arithmetic(format!("integer overflow negating {n}"))
            }),
            Value::Float(n) => Ok(Value::Float(-n)),
            other => Err(QueryError::Type(format!(
                "cannot negate {}",
                value_name(&other)
            ))),
        },
    }
}

/*
evaluate_comparison: the six comparison operators.

A MISSING FIELD DOES NOT MATCH; IT DOES NOT ERROR.

An ordering comparison where either side is Null returns false. This matters
because Field on an absent key evaluates to Null, so without it a single
document lacking a field fails the WHOLE query:

    from users | where age > 18

would error rather than skip, the moment one user document had no age. asl.txt
permits schemaless documents, so that is a normal shape for real data, and it
is also what Mongo does: { age: { $gt: 18 } } simply does not match a document
without an age.

Note what is NOT relaxed. A genuine cross-type comparison, `where name > 5` on
a string field, still errors through compare_values below. That is a mistake in
the query rather than a fact about the data, and it should stay loud.

compare_values itself is left strict on purpose: Sort uses it, and a sort over
a column mixing incomparable types should still fail rather than invent an
order.
*/
fn evaluate_comparison(op: BinOp, left: &Value, right: &Value) -> QueryResult<Value> {
    let ordering_op = matches!(
        op,
        BinOp::Gt | BinOp::GtEq | BinOp::Lt | BinOp::LtEq
    );
    if ordering_op && (matches!(left, Value::Null) || matches!(right, Value::Null)) {
        return Ok(Value::Bool(false));
    }

    let value = match op {
        BinOp::Eq => values_equal(left, right),
        BinOp::NotEq => !values_equal(left, right),
        BinOp::Gt => compare_values(left, right)? == Ordering::Greater,
        BinOp::GtEq => {
            let ordering = compare_values(left, right)?;
            ordering == Ordering::Greater || ordering == Ordering::Equal
        }
        BinOp::Lt => compare_values(left, right)? == Ordering::Less,
        BinOp::LtEq => {
            let ordering = compare_values(left, right)?;
            ordering == Ordering::Less || ordering == Ordering::Equal
        }
        _ => unreachable!(),
    };
    Ok(Value::Bool(value))
}

fn evaluate_arithmetic(op: BinOp, left: &Value, right: &Value) -> QueryResult<Value> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => evaluate_int_arithmetic(op, *a, *b),
        (Value::Int(a), Value::Float(b)) => evaluate_float_arithmetic(op, *a as f64, *b),
        (Value::Float(a), Value::Int(b)) => evaluate_float_arithmetic(op, *a, *b as f64),
        (Value::Float(a), Value::Float(b)) => evaluate_float_arithmetic(op, *a, *b),
        (Value::String(a), Value::String(b)) if op == BinOp::Add => {
            Ok(Value::String(format!("{a}{b}")))
        }
        (Value::String(a), b) if op == BinOp::Add => {
            Ok(Value::String(format!("{a}{}", value_to_string(b)?)))
        }
        (a, Value::String(b)) if op == BinOp::Add => {
            Ok(Value::String(format!("{}{b}", value_to_string(a)?)))
        }
        _ => Err(QueryError::Type(format!(
            "cannot apply arithmetic to {} and {}",
            value_name(left),
            value_name(right)
        ))),
    }
}

/*
evaluate_int_arithmetic: two ints, checked.

Int op Int stays in i64 rather than promoting to f64. That is deliberate and
unchanged: two ints should add to an int, and going through f64 silently loses
precision past 2^53.

WHY CHECKED AND NOT PLAIN OPERATORS.
This used to be `a + b`, `a / b` and so on inline. Both of those panic:

  from users where total / 0 > 1      panics, "attempt to divide by zero"
  <sum past i64::MAX>                 panics in debug, WRAPS SILENTLY in
                                      release

Neither is acceptable from a user query. The first takes the database down;
the second returns a wrong number with no signal, which is worse. Both are
ordinary things for a query over real data to do, so both have to be errors
the caller can report rather than process-level failures.

checked_div and checked_rem return None for two distinct reasons, a zero
divisor and i64::MIN / -1, so the zero case is tested first to give the
accurate message rather than blaming overflow for a division by zero.
*/
fn evaluate_int_arithmetic(op: BinOp, left: i64, right: i64) -> QueryResult<Value> {
    let overflow = || {
        QueryError::Arithmetic(format!(
            "integer overflow evaluating {left} {} {right}",
            op_symbol(op)
        ))
    };

    let result = match op {
        BinOp::Add => left.checked_add(right).ok_or_else(overflow)?,
        BinOp::Sub => left.checked_sub(right).ok_or_else(overflow)?,
        BinOp::Mul => left.checked_mul(right).ok_or_else(overflow)?,
        BinOp::Div | BinOp::Mod => {
            if right == 0 {
                return Err(QueryError::Arithmetic(format!(
                    "division by zero evaluating {left} {} {right}",
                    op_symbol(op)
                )));
            }
            if op == BinOp::Div {
                left.checked_div(right).ok_or_else(overflow)?
            } else {
                left.checked_rem(right).ok_or_else(overflow)?
            }
        }
        _ => unreachable!("caller restricted op to arithmetic"),
    };
    Ok(Value::Int(result))
}

fn op_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        _ => "?",
    }
}

/*
Float arithmetic is deliberately NOT checked. IEEE 754 defines division by
zero as inf / -inf / NaN, and those are legitimate f64 values rather than
failures, so 1.0 / 0.0 stays inf. Only the integer path has cases with no
representable answer at all.
*/
fn evaluate_float_arithmetic(op: BinOp, left: f64, right: f64) -> QueryResult<Value> {
    match op {
        BinOp::Add => Ok(Value::Float(left + right)),
        BinOp::Sub => Ok(Value::Float(left - right)),
        BinOp::Mul => Ok(Value::Float(left * right)),
        BinOp::Div => Ok(Value::Float(left / right)),
        BinOp::Mod => Ok(Value::Float(left % right)),
        _ => unreachable!(),
    }
}

pub(crate) fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
        (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
        _ => left == right,
    }
}

pub(crate) fn compare_values(left: &Value, right: &Value) -> QueryResult<Ordering> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (Value::Int(a), Value::Float(b)) => compare_floats(*a as f64, *b),
        (Value::Float(a), Value::Int(b)) => compare_floats(*a, *b as f64),
        (Value::Float(a), Value::Float(b)) => compare_floats(*a, *b),
        (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
        _ => Err(QueryError::Type(format!(
            "cannot compare {} and {}",
            value_name(left),
            value_name(right)
        ))),
    }
}

fn compare_floats(left: f64, right: f64) -> QueryResult<Ordering> {
    left.partial_cmp(&right).ok_or_else(|| {
        QueryError::Type("cannot compare NaN values".to_string())
    })
}

fn value_to_string(value: &Value) -> QueryResult<String> {
    match value {
        Value::Int(n) => Ok(n.to_string()),
        Value::Float(n) => Ok(n.to_string()),
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Ok("null".to_string()),
        other => Err(QueryError::Type(format!(
            "cannot convert {} to string",
            value_name(other)
        ))),
    }
}

pub(crate) fn value_name(value: &Value) -> &'static str {
    match value {
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::String(_) => "string",
        Value::Bool(_) => "bool",
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::Document(_) => "document",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::asl::{BinOp, LitValue, StringMatchOp};

    fn doc() -> Document {
        let mut doc = Document::new();
        doc.insert("active".to_string(), Value::Bool(true));
        doc.insert("age".to_string(), Value::Int(25));
        doc.insert("name".to_string(), Value::String("alice".to_string()));
        doc.insert("score".to_string(), Value::Float(10.5));
        doc
    }

    fn field(name: &str) -> Expr {
        Expr::Field(name.to_string())
    }

    fn int(n: i64) -> Expr {
        Expr::Lit(LitValue::Int(n))
    }

    #[test]
    fn test_eval_field_and_missing_field() {
        assert_eq!(evaluate(&field("age"), &doc()).unwrap(), Value::Int(25));
        assert_eq!(evaluate(&field("missing"), &doc()).unwrap(), Value::Null);
    }

    #[test]
    fn test_eval_bool_rejects_non_bool() {
        assert!(matches!(evaluate_bool(&field("age"), &doc()), Err(QueryError::Type(_))));
    }

    #[test]
    fn test_eval_comparison_and_boolean_ops() {
        let expr = Expr::BinOp(
            BinOp::And,
            Box::new(Expr::BinOp(
                BinOp::GtEq,
                Box::new(field("age")),
                Box::new(int(18)),
            )),
            Box::new(field("active")),
        );

        assert!(evaluate_bool(&expr, &doc()).unwrap());
    }

    #[test]
    fn test_eval_numeric_arithmetic_coerces_float() {
        let expr = Expr::BinOp(
            BinOp::Add,
            Box::new(field("score")),
            Box::new(int(2)),
        );

        assert_eq!(evaluate(&expr, &doc()).unwrap(), Value::Float(12.5));
    }

    #[test]
    fn test_eval_in_exists_missing_and_string_match() {
        let in_expr = Expr::In {
            field: "age".to_string(),
            values: vec![LitValue::Int(17), LitValue::Int(25)],
            negated: false,
        };
        assert!(evaluate_bool(&in_expr, &doc()).unwrap());

        assert!(evaluate_bool(&Expr::Exists("name".to_string()), &doc()).unwrap());
        assert!(evaluate_bool(&Expr::Missing("country".to_string()), &doc()).unwrap());

        let match_expr = Expr::StringMatch {
            field: "name".to_string(),
            op: StringMatchOp::Starts,
            pattern: "ali".to_string(),
        };
        assert!(evaluate_bool(&match_expr, &doc()).unwrap());
    }

    #[test]
    fn test_eval_ternary() {
        let expr = Expr::Ternary {
            cond: Box::new(Expr::BinOp(
                BinOp::GtEq,
                Box::new(field("age")),
                Box::new(int(18)),
            )),
            then_branch: Box::new(Expr::Lit(LitValue::String("adult".to_string()))),
            else_branch: Box::new(Expr::Lit(LitValue::String("minor".to_string()))),
        };

        assert_eq!(
            evaluate(&expr, &doc()).unwrap(),
            Value::String("adult".to_string())
        );
    }

    #[test]
    fn test_doc_literal_to_document_rejects_field_reference() {
        let lit = DocLiteral {
            fields: vec![("age".to_string(), field("other"))],
        };

        assert!(matches!(
            doc_literal_to_document(&lit),
            Err(QueryError::Unsupported(_))
        ));
    }

    #[test]
    fn test_doc_literal_to_document_nested_literals() {
        let lit = DocLiteral {
            fields: vec![
                ("name".to_string(), Expr::Lit(LitValue::String("alice".to_string()))),
                (
                    "tags".to_string(),
                    Expr::ArrayLit(vec![Expr::Lit(LitValue::String("admin".to_string()))]),
                ),
            ],
        };

        let doc = doc_literal_to_document(&lit).unwrap();
        assert_eq!(
            doc.get("name"),
            Some(&Value::String("alice".to_string()))
        );
        assert_eq!(
            doc.get("tags"),
            Some(&Value::Array(vec![Value::String("admin".to_string())]))
        );
    }

    /*
    The three cases below all PANICKED before checked arithmetic went in:
    "attempt to divide by zero" and "attempt to add with overflow". A user
    query must not be able to take the process down, and in release builds the
    overflow case did not even panic, it wrapped and returned a wrong number.
    */
    #[test]
    fn test_int_division_by_zero_is_an_error_not_a_panic() {
        let expr = Expr::BinOp(BinOp::Div, Box::new(int(1)), Box::new(int(0)));
        assert!(matches!(
            evaluate(&expr, &doc()),
            Err(QueryError::Arithmetic(_))
        ));

        let expr = Expr::BinOp(BinOp::Mod, Box::new(int(1)), Box::new(int(0)));
        assert!(matches!(
            evaluate(&expr, &doc()),
            Err(QueryError::Arithmetic(_))
        ));
    }

    #[test]
    fn test_int_overflow_is_an_error_not_a_wrap() {
        let expr = Expr::BinOp(BinOp::Add, Box::new(int(i64::MAX)), Box::new(int(1)));
        assert!(matches!(
            evaluate(&expr, &doc()),
            Err(QueryError::Arithmetic(_))
        ));

        let expr = Expr::BinOp(BinOp::Mul, Box::new(int(i64::MAX)), Box::new(int(2)));
        assert!(matches!(
            evaluate(&expr, &doc()),
            Err(QueryError::Arithmetic(_))
        ));
    }

    #[test]
    fn test_i64_min_divided_by_negative_one_overflows() {
        // the case checked_div catches that a zero-divisor guard alone misses:
        // -(i64::MIN) is not representable.
        let expr = Expr::BinOp(BinOp::Div, Box::new(int(i64::MIN)), Box::new(int(-1)));
        assert!(matches!(
            evaluate(&expr, &doc()),
            Err(QueryError::Arithmetic(_))
        ));
    }

    #[test]
    fn test_float_division_by_zero_stays_infinite() {
        // NOT an error. IEEE 754 says inf, and inf is a legitimate f64.
        let expr = Expr::BinOp(
            BinOp::Div,
            Box::new(Expr::Lit(LitValue::Float(1.0))),
            Box::new(Expr::Lit(LitValue::Float(0.0))),
        );
        match evaluate(&expr, &doc()).unwrap() {
            Value::Float(x) => assert!(x.is_infinite()),
            other => panic!("expected an infinite float, got {other:?}"),
        }
    }

    #[test]
    fn test_int_arithmetic_still_stays_int() {
        // guards the existing decision: two ints make an int, no f64 promotion.
        let expr = Expr::BinOp(BinOp::Add, Box::new(int(2)), Box::new(int(3)));
        assert_eq!(evaluate(&expr, &doc()).unwrap(), Value::Int(5));

        // and a value past 2^53, which an f64 round-trip would have corrupted.
        let big = (1i64 << 53) + 1;
        let expr = Expr::BinOp(BinOp::Add, Box::new(int(big)), Box::new(int(0)));
        assert_eq!(evaluate(&expr, &doc()).unwrap(), Value::Int(big));
    }

    #[test]
    fn test_string_concatenation_is_unaffected() {
        // the checked-arithmetic change must not disturb the string + path.
        let expr = Expr::BinOp(
            BinOp::Add,
            Box::new(Expr::Lit(LitValue::String("a".to_string()))),
            Box::new(int(5)),
        );
        assert_eq!(
            evaluate(&expr, &doc()).unwrap(),
            Value::String("a5".to_string())
        );
    }

    #[test]
    fn test_negating_i64_min_is_an_error_not_a_panic() {
        let expr = Expr::UnaryOp(UnaryOp::Neg, Box::new(int(i64::MIN)));
        assert!(matches!(
            evaluate(&expr, &doc()),
            Err(QueryError::Arithmetic(_))
        ));
    }

    #[test]
    fn test_missing_field_does_not_match_and_does_not_error() {
        /*
        The schemaless case. Before this, one document without the field made
        the whole query fail, which is not what a document store should do and
        not what Mongo does.
        */
        let expr = Expr::BinOp(BinOp::Gt, Box::new(field("missing")), Box::new(int(18)));
        assert_eq!(evaluate(&expr, &doc()).unwrap(), Value::Bool(false));

        let expr = Expr::BinOp(BinOp::Lt, Box::new(field("missing")), Box::new(int(18)));
        assert_eq!(evaluate(&expr, &doc()).unwrap(), Value::Bool(false));

        // and with the literal on the left, so the relaxation is symmetric
        let expr = Expr::BinOp(BinOp::GtEq, Box::new(int(18)), Box::new(field("missing")));
        assert_eq!(evaluate(&expr, &doc()).unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_genuine_type_mismatches_still_error() {
        // the relaxation above must not swallow a real query mistake: `name`
        // is a string, so ordering it against an int is nonsense.
        let expr = Expr::BinOp(BinOp::Gt, Box::new(field("name")), Box::new(int(5)));
        assert!(matches!(evaluate(&expr, &doc()), Err(QueryError::Type(_))));
    }

    #[test]
    fn test_equality_against_a_missing_field_is_still_false() {
        let expr = Expr::BinOp(BinOp::Eq, Box::new(field("missing")), Box::new(int(18)));
        assert_eq!(evaluate(&expr, &doc()).unwrap(), Value::Bool(false));
    }
}
