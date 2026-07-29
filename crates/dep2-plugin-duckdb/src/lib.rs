//! A generic DuckDB batch source: any SQL query becomes a dep2 relation.
//!
//! ```text
//! --source 'pat=duckdb:sql=SELECT id, title FROM "patents.parquet"'
//! --source 'asg=duckdb:db=patents.db;sql_file=queries/assignments.sql'
//! ```
//!
//! WHY THIS RATHER THAN MORE CSV. The csv plugin is fine for a file someone
//! hand-wrote; it is the wrong shape for an extract measured in gigabytes.
//! DuckDB reads Parquet natively — typed, compressed, columnar — and can do the
//! filtering and joining BEFORE any row reaches the engine, so a program that
//! wants ten thousand rows out of a hundred million never materializes the
//! difference. It also reads csv, json, and remote files, which makes this a
//! superset of the csv batch provider rather than a competitor to it.
//!
//! The provider is deliberately dumb: one query, one relation, no schema
//! configuration. Column names come from the result set and types are inferred
//! from the first row, which is the same contract the csv provider offers, so a
//! program can switch between them by changing only the source spec.
//!
//! LINKING. By default this links the system libduckdb (Homebrew ships
//! `libduckdb.dylib` and `duckdb.h`). The `bundled` feature compiles DuckDB's
//! C++ amalgamation instead — self-contained, and minutes of build time. The
//! whole crate sits behind a `duckdb` feature on the `dep2` binary so that
//! nobody who does not want DuckDB pays for it even once.

use std::collections::HashMap;

use std::sync::Arc;

use dep2_plugin::reconcile::{multiset, reconcile, CanonRow, CanonValue, Multiset};
use dep2_plugin::{
    ColumnDef, DataProvider, DataSchema, DataSource, DataType, DataValue, Plugin, PluginContext,
    Source, StreamOutput, StreamingDataProvider, StreamingDataSource, ValueSink,
};
use duckdb::types::{TimeUnit, ValueRef};
use duckdb::Connection;

const KNOWN_KEYS: &[&str] = &["db", "sql", "sql_file", "read_only"];

pub struct DuckDbPlugin;

impl Plugin for DuckDbPlugin {
    fn name(&self) -> &str {
        "duckdb"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_data_provider(Box::new(DuckDbProvider));
        ctx.register_streaming_data_provider(Box::new(DuckDbProvider));
    }
}

struct DuckDbProvider;

fn validate_config(config: &HashMap<String, String>) -> Result<(), String> {
    for key in config.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "duckdb: unknown config attribute '{}' (known: {})",
                key,
                KNOWN_KEYS.join(", ")
            ));
        }
    }
    Ok(())
}

/// The query text, from `sql` or from `sql_file`.
///
/// `sql_file` exists because a source spec is `;`-separated and lives on a
/// command line: any real analytical query contains quotes, commas and
/// newlines, and inlining it turns into an escaping exercise. Keeping the query
/// in a file also lets it be diffed and reviewed like the rest of the program.
fn query_text(config: &HashMap<String, String>) -> Result<String, String> {
    match (config.get("sql"), config.get("sql_file")) {
        (Some(_), Some(_)) => Err("duckdb: give either 'sql' or 'sql_file', not both".to_string()),
        (Some(sql), None) => Ok(sql.clone()),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|e| format!("duckdb: cannot read sql_file '{}': {}", path, e)),
        (None, None) => Err("duckdb: requires 'sql' or 'sql_file'".to_string()),
    }
}

/// DuckDB's type for a value, narrowed to what the engine models.
///
/// Everything integral becomes `Integer` and everything fractional `Float`.
///
/// DATES AND TIMESTAMPS BECOME EPOCH SECONDS, deliberately. The engine has no
/// date type, and `date_epoch` already speaks epoch seconds, so a DATE column
/// arrives as a number that can be compared and min-ed directly instead of
/// round-tripping through text. Anyone wanting the printed form can
/// `CAST(d AS VARCHAR)` in the query and get an ISO string.
///
/// A DECIMAL literal is not a DOUBLE — DuckDB types `1.5` as DECIMAL(2,1) — so
/// it is unscaled by hand rather than falling through to text, which is what a
/// naive match does and what made a float column arrive as a string.
///
/// Anything still unhandled is rendered as text rather than dropped, so an
/// unexpected type shows up in the data instead of vanishing from it.
fn value_of(v: ValueRef<'_>) -> DataValue {
    match v {
        ValueRef::Null => DataValue::Null,
        ValueRef::Boolean(b) => DataValue::Bool(b),
        ValueRef::TinyInt(i) => DataValue::Integer(i as i64),
        ValueRef::SmallInt(i) => DataValue::Integer(i as i64),
        ValueRef::Int(i) => DataValue::Integer(i as i64),
        ValueRef::BigInt(i) => DataValue::Integer(i),
        ValueRef::UTinyInt(i) => DataValue::Integer(i as i64),
        ValueRef::USmallInt(i) => DataValue::Integer(i as i64),
        ValueRef::UInt(i) => DataValue::Integer(i as i64),
        // Saturating rather than wrapping: a value too large for the engine is
        // clamped visibly instead of silently changing sign.
        ValueRef::UBigInt(i) => DataValue::Integer(i.min(i64::MAX as u64) as i64),
        ValueRef::HugeInt(i) => {
            DataValue::Integer(i.clamp(i64::MIN as i128, i64::MAX as i128) as i64)
        }
        ValueRef::Float(f) => DataValue::Float(f as f64),
        ValueRef::Double(f) => DataValue::Float(f),
        ValueRef::Decimal(d) => match d.to_string().parse::<f64>() {
            Ok(f) => DataValue::Float(f),
            Err(_) => DataValue::String(d.to_string()),
        },
        ValueRef::Date32(days) => DataValue::Integer(days as i64 * 86_400),
        ValueRef::Timestamp(unit, v) => DataValue::Integer(match unit {
            TimeUnit::Second => v,
            TimeUnit::Millisecond => v / 1_000,
            TimeUnit::Microsecond => v / 1_000_000,
            TimeUnit::Nanosecond => v / 1_000_000_000,
        }),
        ValueRef::Text(t) => DataValue::String(String::from_utf8_lossy(t).into_owned()),
        other => DataValue::String(format!("{:?}", other)),
    }
}

fn type_of(v: &DataValue) -> DataType {
    match v {
        DataValue::Integer(_) | DataValue::Bool(_) => DataType::Integer,
        DataValue::Float(_) => DataType::Float,
        _ => DataType::String,
    }
}

/// Run the configured query and return its schema and rows.
///
/// Shared by the batch and streaming providers so the two cannot disagree about
/// types: `--source` goes through the streaming path and a data block through
/// the batch one, and a program that switches between them should not see the
/// same query typed differently.
fn run_query(
    config: &HashMap<String, String>,
) -> Result<(DataSchema, Vec<Vec<DataValue>>), String> {
    validate_config(config)?;
    let sql = query_text(config)?;

    // An in-memory database is the default because the common case is
    // querying files directly — `SELECT * FROM 'x.parquet'` needs no
    // database at all.
    let conn = match config.get("db").map(String::as_str) {
        None | Some(":memory:") => Connection::open_in_memory(),
        Some(path) => Connection::open(path),
    }
    .map_err(|e| format!("duckdb: cannot open database: {}", e))?;

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("duckdb: cannot prepare query: {}", e))?;

    // Column metadata is only available AFTER execution — asking a
    // prepared-but-unexecuted statement for its column names panics inside
    // duckdb-rs rather than returning an error.
    let mut rows_iter = stmt
        .query([])
        .map_err(|e| format!("duckdb: query failed: {}", e))?;

    let names: Vec<String> = rows_iter
        .as_ref()
        .map(|s| s.column_names().iter().map(|n| n.to_string()).collect())
        .unwrap_or_default();
    if names.is_empty() {
        return Err("duckdb: query returned no columns".to_string());
    }

    let mut rows: Vec<Vec<DataValue>> = Vec::new();
    while let Some(row) = rows_iter
        .next()
        .map_err(|e| format!("duckdb: error reading row: {}", e))?
    {
        let mut out = Vec::with_capacity(names.len());
        for i in 0..names.len() {
            let v = row
                .get_ref(i)
                .map_err(|e| format!("duckdb: error reading column {}: {}", i, e))?;
            out.push(value_of(v));
        }
        rows.push(out);
    }

    // Types come from the first row, matching the csv provider's contract.
    // A query returning nothing still has to declare a schema, and every
    // column being String is the honest answer when there is no value to
    // look at.
    let col_types: Vec<DataType> = match rows.first() {
        Some(first) => first.iter().map(type_of).collect(),
        None => names.iter().map(|_| DataType::String).collect(),
    };

    let schema = DataSchema {
        columns: names
            .iter()
            .zip(col_types.iter())
            .map(|(name, dt)| ColumnDef {
                name: name.clone(),
                data_type: dt.clone(),
            })
            .collect(),
    };

    Ok((schema, rows))
}

impl DataProvider for DuckDbProvider {
    fn name(&self) -> &str {
        "duckdb"
    }

    fn open(&self, config: &HashMap<String, String>) -> Result<Box<dyn DataSource>, String> {
        let (schema, rows) = run_query(config)?;
        Ok(Box::new(DuckDbSource { schema, rows }))
    }
}

// ---------------------------------------------------------------------------
// Streaming provider
// ---------------------------------------------------------------------------

/// The `--source` path goes through the streaming provider, so a batch-only
/// plugin is invisible from the command line however well it works.
///
/// A query result is a single work unit: there is nothing to shard, and running
/// it once per worker would multiply every row by the worker count. The rows are
/// materialized at open and shared by `Arc`, so the query executes exactly once
/// no matter how many workers the engine starts.
impl StreamingDataProvider for DuckDbProvider {
    fn name(&self) -> &str {
        "duckdb"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        let (schema, rows) = run_query(config)?;
        let canon: Vec<CanonRow> = rows
            .iter()
            .map(|r| r.iter().map(canon_of).collect())
            .collect();
        Ok(Box::new(DuckDbStream {
            schema,
            rows: Arc::new(canon),
        }))
    }
}

fn canon_of(v: &DataValue) -> CanonValue {
    match v {
        DataValue::String(s) => CanonValue::str(s),
        DataValue::Str(s) => CanonValue::str(s),
        DataValue::Integer(i) => CanonValue::Int(*i),
        DataValue::Bool(b) => CanonValue::Int(*b as i64),
        DataValue::Float(f) => CanonValue::Float(f.to_bits()),
        DataValue::Null => CanonValue::Null,
    }
}

struct DuckDbStream {
    schema: DataSchema,
    rows: Arc<Vec<CanonRow>>,
}

impl StreamingDataSource for DuckDbStream {
    fn outputs(&self) -> Vec<StreamOutput> {
        // The relation name comes from the source spec, which the engine fills.
        vec![StreamOutput {
            relation: String::new(),
            schema: self.schema.clone(),
        }]
    }

    fn seed_units(&self) -> Vec<String> {
        vec!["duckdb".to_string()]
    }

    fn open(&self) -> Box<dyn Source> {
        Box::new(DuckDbWorker {
            rows: Arc::clone(&self.rows),
            current: Multiset::new(),
        })
    }
}

struct DuckDbWorker {
    rows: Arc<Vec<CanonRow>>,
    current: Multiset,
}

impl Source for DuckDbWorker {
    fn ingest(&mut self, _unit: &str, sink: &mut dyn ValueSink) {
        // Reconciled rather than pushed blindly, so a second ingest of the same
        // unit is a no-op as the trait requires.
        let next = multiset(self.rows.iter().cloned());
        reconcile(sink, "", &self.current, &next);
        self.current = next;
    }
}

struct DuckDbSource {
    schema: DataSchema,
    rows: Vec<Vec<DataValue>>,
}

impl DataSource for DuckDbSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn fetch_all(&self) -> Result<Vec<Vec<DataValue>>, String> {
        Ok(self.rows.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_query_becomes_a_typed_relation() {
        let src = DuckDbProvider
            .open(&cfg(&[(
                "sql",
                "SELECT 'US1' AS patent, 42 AS pta, 1.5 AS ratio",
            )]))
            .unwrap();
        let cols: Vec<&str> = src
            .schema()
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cols, vec!["patent", "pta", "ratio"]);
        assert_eq!(
            src.schema()
                .columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect::<Vec<_>>(),
            vec![DataType::String, DataType::Integer, DataType::Float]
        );
        assert_eq!(
            src.fetch_all().unwrap(),
            vec![vec![
                DataValue::String("US1".into()),
                DataValue::Integer(42),
                DataValue::Float(1.5),
            ]]
        );
    }

    #[test]
    fn an_empty_result_still_declares_its_columns() {
        // A relation whose query matches nothing must still have a schema, or
        // the engine cannot bind it at all.
        let src = DuckDbProvider
            .open(&cfg(&[("sql", "SELECT 1 AS a, 'x' AS b WHERE false")]))
            .unwrap();
        assert_eq!(src.schema().columns.len(), 2);
        assert!(src.fetch_all().unwrap().is_empty());
    }

    #[test]
    fn nulls_survive_as_nulls_rather_than_empty_strings() {
        // An empty CSV field loads as NULL and silently zeroes any count over
        // it; that trap is worth not reproducing here by accident.
        let src = DuckDbProvider
            .open(&cfg(&[("sql", "SELECT NULL AS a, 7 AS b")]))
            .unwrap();
        assert_eq!(
            src.fetch_all().unwrap(),
            vec![vec![DataValue::Null, DataValue::Integer(7)]]
        );
    }

    #[test]
    fn duckdb_specific_types_do_not_fall_through_to_text() {
        // Each of these arrived as a Debug-formatted string before it was
        // handled, which type-checks and silently corrupts the column.
        let src = DuckDbProvider
            .open(&cfg(&[(
                "sql",
                "SELECT 1.5 AS dec_lit, DATE '2030-01-01' AS d, \
                 TIMESTAMP '2030-01-01 00:00:00' AS ts, CAST(7 AS HUGEINT) AS h",
            )]))
            .unwrap();
        assert_eq!(
            src.fetch_all().unwrap(),
            vec![vec![
                DataValue::Float(1.5),
                // 2030-01-01 as epoch seconds, directly comparable in a program.
                DataValue::Integer(1_893_456_000),
                DataValue::Integer(1_893_456_000),
                DataValue::Integer(7),
            ]]
        );
    }

    #[test]
    fn config_is_validated_rather_than_ignored() {
        assert!(DuckDbProvider.open(&cfg(&[("db", ":memory:")])).is_err());
        let e = DuckDbProvider
            .open(&cfg(&[("sql", "SELECT 1"), ("typo", "x")]))
            .map(|_| ())
            .unwrap_err();
        assert!(e.contains("unknown config attribute"), "{}", e);
        let e = DuckDbProvider
            .open(&cfg(&[("sql", "SELECT 1"), ("sql_file", "q.sql")]))
            .map(|_| ())
            .unwrap_err();
        assert!(e.contains("not both"), "{}", e);
    }

    #[test]
    fn a_parquet_file_can_be_queried_directly() {
        // The reason this plugin exists: no database, no import step, just a
        // query over a file on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT 'US9' AS patent, 2033 AS year) TO '{}' (FORMAT PARQUET)",
            path.display()
        ))
        .unwrap();

        let src = DuckDbProvider
            .open(&cfg(&[(
                "sql",
                &format!("SELECT patent, year FROM '{}'", path.display()),
            )]))
            .unwrap();
        assert_eq!(
            src.fetch_all().unwrap(),
            vec![vec![
                DataValue::String("US9".into()),
                DataValue::Integer(2033)
            ]]
        );
    }
}
