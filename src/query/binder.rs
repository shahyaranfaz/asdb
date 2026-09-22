use super::{QueryError, QueryResult};

use crate::asl::{DocLiteral, Expr, Pipeline, SchemaField, SelectItem, Stage, Statement};
use crate::database::Database;

use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq)]
pub enum BoundStatement {
    Pipeline(Pipeline),
    CreateCollection { name: String, schema: Vec<SchemaField> },
    DropCollection { name: String },
    CreateIndex { collection: String, fields: Vec<String>, unique: bool },
    DropIndex { collection: String, field: String },
    /// A guarded DDL statement whose object was already in the state asked for.
    NoOp,
}

pub fn bind(db: &Database, statement: Statement) -> QueryResult<BoundStatement> {
    match statement {
        Statement::Pipeline(stages) => bind_pipeline(db, stages),
        Statement::CreateCollection { name, schema, if_not_exists } => {
            if db.has_collection(&name) {
                if if_not_exists {
                    return Ok(BoundStatement::NoOp);
                }
                return Err(QueryError::Validation(format!("collection already exists: {name}")));
            }
            validate_schema(&schema)?;
            Ok(BoundStatement::CreateCollection { name, schema })
        }
        Statement::DropCollection { name, if_exists } => {
            if !db.has_collection(&name) {
                if if_exists {
                    return Ok(BoundStatement::NoOp);
                }
                return Err(QueryError::Validation(format!("collection not found: {name}")));
            }
            Ok(BoundStatement::DropCollection { name })
        }
        Statement::CreateIndex { collection, fields, if_not_exists, unique } => {
            let fields = bind_index_fields(db, &collection, fields, unique)?;
            if db.has_compound_index(&collection, &fields) {
                if if_not_exists {
                    return Ok(BoundStatement::NoOp);
                }
                return Err(QueryError::Validation(format!(
                    "index already exists: {collection}.{}", fields.join(", ")
                )));
            }
            Ok(BoundStatement::CreateIndex { collection, fields, unique })
        }
        Statement::DropIndex { collection, fields, if_exists } => {
            let field = bind_single_index_field(db, &collection, fields)?;
            if !db.has_index(&collection, &field) {
                if if_exists {
                    return Ok(BoundStatement::NoOp);
                }
                return Err(QueryError::Validation(format!(
                    "index not found: {collection}.{field}"
                )));
            }
            Ok(BoundStatement::DropIndex { collection, field })
        }
    }
}

fn bind_single_index_field(db: &Database, collection: &str,
                           fields: Vec<String>) -> QueryResult<String> {
    if !db.has_collection(collection) {
        return Err(QueryError::Validation(format!("collection not found: {collection}")));
    }
    if fields.len() != 1 {
        return Err(QueryError::Unsupported(
            "composite runtime indexes are not supported yet".to_string(),
        ));
    }
    Ok(fields.into_iter().next().unwrap())
}

/*
bind_index_fields: several fields are allowed only for a unique index.

A compound index exists to enforce uniqueness across a combination. The planner
still looks indexes up one field at a time, so a non-unique compound index would
be written and maintained on every insert while never answering a query, which
is cost without benefit.
*/
fn bind_index_fields(db: &Database, collection: &str, fields: Vec<String>,
                     unique: bool) -> QueryResult<Vec<String>> {
    if !db.has_collection(collection) {
        return Err(QueryError::Validation(format!("collection not found: {collection}")));
    }
    if fields.is_empty() {
        return Err(QueryError::Validation("an index needs at least one field".to_string()));
    }
    if fields.len() > 1 && !unique {
        return Err(QueryError::Unsupported(
            "composite indexes are supported only as unique constraints".to_string(),
        ));
    }
    Ok(fields)
}

fn bind_pipeline(db: &Database, stages: Pipeline) -> QueryResult<BoundStatement> {
    if stages.is_empty() {
        return Err(QueryError::InvalidPipeline("pipeline is empty".to_string()));
    }

    let Stage::From { collection, index_hint } = &stages[0] else {
        return Err(QueryError::InvalidPipeline(
            "pipeline must start with from".to_string(),
        ));
    };

    if !db.has_collection(collection) {
        return Err(QueryError::Validation(format!("collection not found: {collection}")));
    }

    if let Some(field) = index_hint {
        if !db.has_index(collection, field) {
            return Err(QueryError::Validation(format!(
                "index not found: {collection}.{field}"
            )));
        }
    }

    let mut saw_group = false;
    for (i, stage) in stages.iter().enumerate() {
        match stage {
            Stage::From { .. } if i != 0 => {
                return Err(QueryError::InvalidPipeline(
                    "from must appear only as the first stage".to_string(),
                ));
            }
            Stage::Insert { docs } => {
                ensure_terminal(&stages, i, "insert")?;
                validate_insert_docs(docs)?;
            }
            Stage::Update { assignments } => {
                ensure_terminal(&stages, i, "update")?;
                if assignments.is_empty() {
                    return Err(QueryError::InvalidPipeline(
                        "update requires at least one assignment".to_string(),
                    ));
                }
            }
            Stage::Upsert { doc } => {
                ensure_terminal(&stages, i, "upsert")?;
                if doc.fields.is_empty() {
                    return Err(QueryError::InvalidPipeline(
                        "upsert requires a document with at least one field".to_string(),
                    ));
                }
            }
            Stage::Delete => ensure_terminal(&stages, i, "delete")?,
            Stage::Group { .. } => saw_group = true,
            Stage::Join { collection, .. } => {
                if !db.has_collection(collection) {
                    return Err(QueryError::Validation(format!(
                        "collection not found: {collection}"
                    )));
                }
            }
            Stage::Select { items: Some(items) } => validate_select_items(items, saw_group)?,
            Stage::Limit { n } | Stage::Offset { n } if *n < 0 => {
                return Err(QueryError::InvalidPipeline(
                    "limit and offset must be non-negative".to_string(),
                ));
            }
            _ => {}
        }
    }
    Ok(BoundStatement::Pipeline(stages))
}

fn ensure_terminal(stages: &[Stage], index: usize, name: &str) -> QueryResult<()> {
    if index != stages.len() - 1 {
        return Err(QueryError::InvalidPipeline(format!("{name} must be terminal")));
    }
    Ok(())
}

fn validate_schema(schema: &[SchemaField]) -> QueryResult<()> {
    let mut names = HashSet::new();
    for field in schema {
        if !names.insert(field.name.as_str()) {
            return Err(QueryError::Validation(format!(
                "duplicate schema field: {}",
                field.name
            )));
        }
    }
    Ok(())
}

fn validate_insert_docs(docs: &[DocLiteral]) -> QueryResult<()> {
    if docs.is_empty() {
        return Err(QueryError::InvalidPipeline(
            "insert requires at least one document".to_string(),
        ));
    }
    for doc in docs {
        let mut fields = HashSet::new();
        for (field, _) in &doc.fields {
            if !fields.insert(field.as_str()) {
                return Err(QueryError::Validation(format!(
                    "duplicate document field: {field}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_select_items(items: &[SelectItem], saw_group: bool) -> QueryResult<()> {
    for item in items {
        validate_select_expr(&item.expr, saw_group)?;
    }
    Ok(())
}

fn validate_select_expr(expr: &Expr, saw_group: bool) -> QueryResult<()> {
    match expr {
        Expr::AggCall { .. } if !saw_group => Err(QueryError::InvalidPipeline(
            "aggregate select requires a preceding group stage".to_string(),
        )),
        Expr::BinOp(_, left, right) => {
            validate_select_expr(left, saw_group)?;
            validate_select_expr(right, saw_group)
        }
        Expr::UnaryOp(_, expr) => validate_select_expr(expr, saw_group),
        Expr::Ternary { cond, then_branch, else_branch } => {
            validate_select_expr(cond, saw_group)?;
            validate_select_expr(then_branch, saw_group)?;
            validate_select_expr(else_branch, saw_group)
        }
        Expr::ArrayLit(values) => {
            for value in values {
                validate_select_expr(value, saw_group)?;
            }
            Ok(())
        }
        Expr::DocLit(doc) => {
            for (_, value) in &doc.fields {
                validate_select_expr(value, saw_group)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::asl::{
        BinOp, Expr, LitValue, SchemaType, SelectItem, UnaryOp,
    };

    use tempfile::NamedTempFile;

    fn db_with_users() -> (Database, NamedTempFile) {
        let tmp = NamedTempFile::new().unwrap();
        let mut db = Database::create(tmp.path()).unwrap();
        db.create_collection("users").unwrap();
        (db, tmp)
    }

    #[test]
    fn test_bind_rejects_missing_collection() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Database::create(tmp.path()).unwrap();
        let stmt = Statement::Pipeline(vec![Stage::From {
            collection: "users".to_string(),
            index_hint: None,
        }]);

        assert!(matches!(bind(&db, stmt), Err(QueryError::Validation(_))));
    }

    #[test]
    fn test_bind_validates_index_hint() {
        let (mut db, _tmp) = db_with_users();
        let stmt = Statement::Pipeline(vec![Stage::From {
            collection: "users".to_string(),
            index_hint: Some("age".to_string()),
        }]);
        assert!(matches!(bind(&db, stmt), Err(QueryError::Validation(_))));

        db.create_index("users", "age").unwrap();
        let stmt = Statement::Pipeline(vec![Stage::From {
            collection: "users".to_string(),
            index_hint: Some("age".to_string()),
        }]);
        assert!(bind(&db, stmt).is_ok());
    }

    #[test]
    fn test_bind_rejects_duplicate_schema_fields() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Database::create(tmp.path()).unwrap();
        let stmt = Statement::CreateCollection {
            name: "users".to_string(),
            schema: vec![
                SchemaField {
                    name: "id".to_string(),
                    ty: SchemaType::Int,
                    required: false,
                    unique: false,
                },
                SchemaField {
                    name: "id".to_string(),
                    ty: SchemaType::String,
                    required: false,
                    unique: false,
                },
            ],
            if_not_exists: false,
        };

        assert!(matches!(bind(&db, stmt), Err(QueryError::Validation(_))));
    }

    #[test]
    fn test_bind_rejects_non_terminal_delete() {
        let (db, _tmp) = db_with_users();
        let stmt = Statement::Pipeline(vec![
            Stage::From {
                collection: "users".to_string(),
                index_hint: None,
            },
            Stage::Delete,
            Stage::Limit { n: 1 },
        ]);

        assert!(matches!(bind(&db, stmt), Err(QueryError::InvalidPipeline(_))));
    }

    #[test]
    fn test_bind_rejects_aggregate_select_without_group() {
        let (db, _tmp) = db_with_users();
        let stmt = Statement::Pipeline(vec![
            Stage::From {
                collection: "users".to_string(),
                index_hint: None,
            },
            Stage::Select {
                items: Some(vec![SelectItem {
                    expr: Expr::AggCall {
                        name: "count".to_string(),
                        arg: None,
                    },
                    alias: None,
                }]),
            },
        ]);

        assert!(matches!(bind(&db, stmt), Err(QueryError::InvalidPipeline(_))));
    }

    #[test]
    fn test_guarded_create_on_existing_collection_is_a_no_op() {
        let (db, _tmp) = db_with_users();
        let stmt = Statement::CreateCollection {
            name: "users".to_string(),
            schema: vec![],
            if_not_exists: true,
        };

        assert!(matches!(bind(&db, stmt), Ok(BoundStatement::NoOp)));
    }

    #[test]
    fn test_guarded_drop_of_missing_collection_is_a_no_op() {
        let (db, _tmp) = db_with_users();
        let stmt = Statement::DropCollection { name: "ghosts".to_string(), if_exists: true };

        assert!(matches!(bind(&db, stmt), Ok(BoundStatement::NoOp)));
    }

    #[test]
    fn test_unguarded_create_on_existing_collection_still_errors() {
        let (db, _tmp) = db_with_users();
        let stmt = Statement::CreateCollection {
            name: "users".to_string(),
            schema: vec![],
            if_not_exists: false,
        };

        assert!(matches!(bind(&db, stmt), Err(QueryError::Validation(_))));
    }

    #[test]
    fn test_bind_rejects_composite_runtime_index() {
        let (db, _tmp) = db_with_users();
        let stmt = Statement::CreateIndex {
            collection: "users".to_string(),
            fields: vec!["age".to_string(), "id".to_string()],
                if_not_exists: false,
                unique: false,
        };

        assert!(matches!(bind(&db, stmt), Err(QueryError::Unsupported(_))));
    }

    #[test]
    fn test_bind_accepts_valid_pipeline() {
        let (db, _tmp) = db_with_users();
        let stmt = Statement::Pipeline(vec![
            Stage::From {
                collection: "users".to_string(),
                index_hint: None,
            },
            Stage::Where {
                expr: Expr::BinOp(
                    BinOp::GtEq,
                    Box::new(Expr::Field("age".to_string())),
                    Box::new(Expr::Lit(LitValue::Int(18))),
                ),
            },
            Stage::Select {
                items: Some(vec![SelectItem {
                    expr: Expr::UnaryOp(
                        UnaryOp::Not,
                        Box::new(Expr::Lit(LitValue::Bool(false))),
                    ),
                    alias: Some("adult".to_string()),
                }]),
            },
        ]);

        assert!(bind(&db, stmt).is_ok());
    }
}
