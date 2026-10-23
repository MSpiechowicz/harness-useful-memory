//! The statement runner the SQLite-dialect code is written against.
//!
//! The notes, search and graph modules speak SQLite's dialect (FTS5 included) through [`Sql`], so
//! the local file ([`super::SqliteStore`], through rusqlite) and a libSQL server
//! ([`crate::store::libsql::LibsqlStore`], over HTTP) run exactly the same statements. Rows come
//! back whole as [`Value`]s; every query here is bounded by a limit, so that stays small.

use crate::store::StoreError;

/// A value bound to a statement or read from a row. The schema stores no blobs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

/// Stored as 0 or 1, like rusqlite does.
impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<&String> for Value {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// The arguments of a statement: `args![a, b]` binds `?1` and `?2`.
macro_rules! args {
    ($($value:expr),* $(,)?) => {
        &[$($crate::store::sqlite::sql::Value::from($value)),*]
    };
}

pub(crate) use args;

/// A value read from a column.
pub(crate) trait FromValue: Sized {
    fn from_value(value: &Value) -> Option<Self>;
}

impl FromValue for i64 {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Integer(number) => Some(*number),
            _ => None,
        }
    }
}

impl FromValue for f64 {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Real(number) => Some(*number),
            Value::Integer(number) => Some(*number as f64),
            _ => None,
        }
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Option<Self> {
        i64::from_value(value).map(|number| number != 0)
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Null => Some(None),
            other => T::from_value(other).map(Some),
        }
    }
}

/// One result row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row(Vec<Value>);

impl Row {
    pub(crate) fn new(values: Vec<Value>) -> Self {
        Self(values)
    }

    /// Column `index` as `T`; a missing column or another type is a backend error.
    pub(crate) fn get<T: FromValue>(&self, index: usize) -> Result<T, StoreError> {
        self.0
            .get(index)
            .and_then(T::from_value)
            .ok_or_else(|| StoreError::Backend(format!("unexpected value in column {index}")))
    }
}

/// What a statement without rows did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Executed {
    /// Rows inserted, updated or deleted.
    pub changes: usize,
    /// The rowid of the latest insert on the connection.
    pub last_insert_rowid: i64,
}

/// Runs statements on one connection (inside a transaction for writes).
pub(crate) trait Sql {
    /// Every row of `sql`.
    fn query(&mut self, sql: &str, args: &[Value]) -> Result<Vec<Row>, StoreError>;

    /// Runs `sql`, which returns no rows.
    fn execute(&mut self, sql: &str, args: &[Value]) -> Result<Executed, StoreError>;

    /// Runs several `;`-separated statements without arguments.
    fn batch(&mut self, sql: &str) -> Result<(), StoreError>;
}

impl dyn Sql + '_ {
    /// The first row of `sql` mapped by `map`, if there is one.
    pub(crate) fn row<T>(
        &mut self,
        sql: &str,
        args: &[Value],
        map: impl FnOnce(&Row) -> Result<T, StoreError>,
    ) -> Result<Option<T>, StoreError> {
        self.query(sql, args)?.first().map(map).transpose()
    }

    /// The first row of `sql` mapped by `map`; no row is a backend error.
    pub(crate) fn one<T>(
        &mut self,
        sql: &str,
        args: &[Value],
        map: impl FnOnce(&Row) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.row(sql, args, map)?
            .ok_or_else(|| StoreError::Backend("Query returned no rows".to_owned()))
    }

    /// The first column of the first row of `sql`, if there is a row.
    pub(crate) fn value<T: FromValue>(&mut self, sql: &str, args: &[Value]) -> Result<Option<T>, StoreError> {
        self.row(sql, args, |row| row.get(0))
    }

    /// The first column of every row of `sql`.
    pub(crate) fn column<T: FromValue>(&mut self, sql: &str, args: &[Value]) -> Result<Vec<T>, StoreError> {
        self.query(sql, args)?.iter().map(|row| row.get(0)).collect()
    }
}
