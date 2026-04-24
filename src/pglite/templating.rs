use anyhow::{Result, anyhow};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

use crate::pglite::client::Pglite;
use crate::pglite::interface::QueryOptions;
use crate::pglite::types::TEXT;

#[derive(Debug, Clone)]
pub struct TemplatedQuery {
    pub query: String,
    pub params: Vec<Value>,
}

#[derive(Debug, Default, Clone)]
pub struct QueryTemplate {
    sql: String,
    params: Vec<Value>,
}

impl QueryTemplate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_sql(&mut self, sql: impl AsRef<str>) {
        self.sql.push_str(sql.as_ref());
    }

    pub fn push_raw(&mut self, sql: impl AsRef<str>) {
        self.push_sql(sql);
    }

    pub fn push_identifier(&mut self, identifier: &str) {
        self.sql.push_str(&quote_identifier(identifier));
    }

    pub fn push_param(&mut self, value: Value) {
        let placeholder = format!("${}", self.params.len() + 1);
        self.sql.push_str(&placeholder);
        self.params.push(value);
    }

    pub fn build(self) -> TemplatedQuery {
        TemplatedQuery {
            query: self.sql,
            params: self.params,
        }
    }
}

static DOLLAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$(\d+)").expect("invalid regex"));

pub fn quote_identifier(ident: &str) -> String {
    let escaped = ident.replace('"', "\"\"");
    format!("\"{}\"", escaped)
}

pub fn format_query(pg: &mut Pglite, query: &str, params: &[Value]) -> Result<String> {
    if params.is_empty() {
        return Ok(query.to_string());
    }

    let described = pg.describe_query(query, None)?;
    let data_type_ids = described
        .query_params
        .iter()
        .map(|param| param.data_type_id)
        .collect::<Vec<_>>();

    let formatted = DOLLAR_RE
        .replace_all(query, |caps: &regex::Captures| format!("%{}L", &caps[1]))
        .to_string();

    let mut sql = String::from("SELECT format($1");
    for idx in 0..params.len() {
        sql.push_str(", $");
        sql.push_str(&(idx as i32 + 2).to_string());
    }
    sql.push_str(") AS query");

    let mut arguments: Vec<Value> = Vec::with_capacity(params.len() + 1);
    arguments.push(Value::String(formatted));
    arguments.extend(params.iter().cloned());

    let mut param_types = Vec::with_capacity(arguments.len());
    param_types.push(TEXT);
    param_types
        .extend((0..params.len()).map(|idx| data_type_ids.get(idx).copied().unwrap_or(TEXT)));
    let options = QueryOptions {
        param_types,
        ..QueryOptions::default()
    };

    let results = pg.query(&sql, &arguments, Some(&options))?;
    let row = results
        .rows
        .first()
        .ok_or_else(|| anyhow!("format query returned no rows"))?;
    if let Value::Object(map) = row
        && let Some(Value::String(formatted)) = map.get("query")
    {
        return Ok(formatted.clone());
    }

    Err(anyhow!("unexpected format query result"))
}

#[cfg(test)]
mod tests {
    use super::{QueryTemplate, quote_identifier};
    use serde_json::json;

    #[test]
    fn template_builder_adds_params() {
        let mut tpl = QueryTemplate::new();
        tpl.push_sql("SELECT ");
        tpl.push_identifier("foo");
        tpl.push_sql(" WHERE id = ");
        tpl.push_param(json!(42));
        let built = tpl.build();
        assert_eq!(built.query, "SELECT \"foo\" WHERE id = $1");
        assert_eq!(built.params.len(), 1);
    }

    #[test]
    fn quote_identifier_escapes_quotes() {
        assert_eq!(quote_identifier("Foo"), "\"Foo\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
