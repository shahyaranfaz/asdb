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
            Value::Int(n) => Ok(Value::Int(-n)),
            Value::Float(n) => Ok(Value::Float(-n)),
            other => Err(QueryError::Type(format!(
                "cannot negate {}",
                value_name(&other)
            ))),
        },
    }
}

fn evaluate_comparison(op: BinOp, left: &Value, right: &Value) -> QueryResult<Value> {
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
        (Value::Int(a), Value::Int(b)) => match op {
            BinOp::Add => Ok(Value::Int(a + b)),
            BinOp::Sub => Ok(Value::Int(a - b)),
            BinOp::Mul => Ok(Value::Int(a * b)),
            BinOp::Div => Ok(Value::Int(a / b)),
            BinOp::Mod => Ok(Value::Int(a % b)),
            _ => unreachable!(),
        },
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

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
        (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
        _ => left == right,
    }
}

fn compare_values(left: &Value, right: &Value) -> QueryResult<Ordering> {
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

fn value_name(value: &Value) -> &'static str {
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
}
