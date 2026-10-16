//! [`Sql`] over a rusqlite connection (or a transaction, which derefs to one).

use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::{Connection, ToSql, params_from_iter};

use super::sql::{Executed, Row, Sql, Value};
use crate::store::StoreError;

/// A borrowed connection of the local database.
pub(super) struct Local<'c>(pub(super) &'c Connection);

impl Sql for Local<'_> {
    fn query(&mut self, sql: &str, args: &[Value]) -> Result<Vec<Row>, StoreError> {
        let mut statement = self.0.prepare_cached(sql)?;
        let columns = statement.column_count();
        let mut rows = statement.query(params_from_iter(args))?;
        let mut out = Vec::new();

        while let Some(row) = rows.next()? {
            let values = (0..columns)
                .map(|index| row.get_ref(index).map_err(StoreError::from).and_then(value_of))
                .collect::<Result<_, _>>()?;
            out.push(Row::new(values));
        }

        Ok(out)
    }

    fn execute(&mut self, sql: &str, args: &[Value]) -> Result<Executed, StoreError> {
        let changes = self.0.prepare_cached(sql)?.execute(params_from_iter(args))?;

        Ok(Executed {
            changes,
            last_insert_rowid: self.0.last_insert_rowid(),
        })
    }

    fn batch(&mut self, sql: &str) -> Result<(), StoreError> {
        Ok(self.0.execute_batch(sql)?)
    }
}

impl ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Self::Null => ToSqlOutput::Borrowed(ValueRef::Null),
            Self::Integer(number) => ToSqlOutput::Borrowed(ValueRef::Integer(*number)),
            Self::Real(number) => ToSqlOutput::Borrowed(ValueRef::Real(*number)),
            Self::Text(text) => ToSqlOutput::Borrowed(ValueRef::Text(text.as_bytes())),
        })
    }
}

fn value_of(value: ValueRef<'_>) -> Result<Value, StoreError> {
    match value {
        ValueRef::Null => Ok(Value::Null),
        ValueRef::Integer(number) => Ok(Value::Integer(number)),
        ValueRef::Real(number) => Ok(Value::Real(number)),
        ValueRef::Text(text) => Ok(Value::Text(String::from_utf8_lossy(text).into_owned())),
        ValueRef::Blob(_) => Err(StoreError::Backend("unexpected blob value".to_owned())),
    }
}
