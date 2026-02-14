use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;

use dbflow_plugin::{
    crossbeam_channel, ColumnDef, DataProvider, DataSchema, DataSource, DataType, DataValue,
    Plugin, PluginContext, StreamingDataProvider, StreamingDataSource, StreamingUpdate,
};

pub struct ExecPlugin;

impl Plugin for ExecPlugin {
    fn name(&self) -> &str {
        "exec"
    }

    fn setup(&self, ctx: &mut PluginContext) {
        ctx.register(self.name());
        ctx.register_data_provider(Box::new(ExecBatchProvider));
        ctx.register_streaming_data_provider(Box::new(ExecStreamingProvider));
    }
}

// ---------------------------------------------------------------------------
// Batch data provider — runs command to completion, returns stdout as rows
// ---------------------------------------------------------------------------

struct ExecBatchProvider;

impl DataProvider for ExecBatchProvider {
    fn name(&self) -> &str {
        "exec"
    }

    fn open(&self, config: &HashMap<String, String>) -> Result<Box<dyn DataSource>, String> {
        let command = config
            .get("command")
            .ok_or("exec data provider requires 'command' config attribute")?
            .clone();

        let split_pattern = config
            .get("split")
            .ok_or("exec data provider requires 'split' config attribute")?
            .clone();

        let split_re = Regex::new(&split_pattern)
            .map_err(|e| format!("invalid split regex '{}': {}", split_pattern, e))?;

        let header = config.get("header").map(|s| s.as_str()) == Some("true");

        let explicit_columns: Option<Vec<String>> = config
            .get("columns")
            .map(|c| c.split(',').map(|s| s.trim().to_string()).collect());

        let explicit_types: Option<Vec<DataType>> = config.get("types").map(|t| {
            t.split(',')
                .map(|s| match s.trim() {
                    "integer" => DataType::Integer,
                    "float" => DataType::Float,
                    _ => DataType::String,
                })
                .collect()
        });

        let stream_target = match config.get("stream").map(|s| s.as_str()) {
            Some("stderr") => Stream::Stderr,
            Some("stdout") | None => Stream::Stdout,
            Some(other) => {
                return Err(format!(
                    "unknown stream '{}': expected 'stdout' or 'stderr'",
                    other
                ))
            }
        };

        let timeout_secs: Option<u64> = config
            .get("timeout")
            .map(|s| {
                s.parse::<u64>()
                    .map_err(|_| format!("invalid timeout '{}': must be a positive integer", s))
            })
            .transpose()?;

        // Run the command to completion.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &command]);
        match stream_target {
            Stream::Stdout => {
                cmd.stdout(Stdio::piped()).stderr(Stdio::null());
            }
            Stream::Stderr => {
                cmd.stdout(Stdio::null()).stderr(Stdio::piped());
            }
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn command '{}': {}", command, e))?;

        let output = if let Some(secs) = timeout_secs {
            // Wait with timeout by polling.
            let deadline = Instant::now() + Duration::from_secs(secs);
            let mut child = child;
            loop {
                match child.try_wait() {
                    Ok(Some(_status)) => break child.wait_with_output(),
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(format!("command timed out after {}s", secs));
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => return Err(format!("error waiting for command: {}", e)),
                }
            }
        } else {
            child.wait_with_output()
        }
        .map_err(|e| format!("failed to run command '{}': {}", command, e))?;

        let raw_output = match stream_target {
            Stream::Stdout => String::from_utf8_lossy(&output.stdout).to_string(),
            Stream::Stderr => String::from_utf8_lossy(&output.stderr).to_string(),
        };

        let mut lines: Vec<String> = raw_output
            .lines()
            .map(|l| strip_ansi(l).trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        // Parse header if present.
        let mut header_names: Option<Vec<String>> = None;
        if header && !lines.is_empty() {
            header_names = Some(split_re.split(&lines[0]).map(|s| s.to_string()).collect());
            lines.remove(0);
        }

        // Infer schema from first data line.
        let first_fields: Vec<&str> = lines
            .first()
            .map(|line| split_re.split(line).collect())
            .unwrap_or_default();

        let col_names: Vec<String> = if let Some(ref explicit) = explicit_columns {
            explicit.clone()
        } else if let Some(ref names) = header_names {
            names.clone()
        } else {
            (0..first_fields.len())
                .map(|i| format!("col{}", i))
                .collect()
        };

        let col_types: Vec<DataType> = if let Some(ref types) = explicit_types {
            if types.len() != col_names.len() {
                return Err(format!(
                    "exec types count ({}) does not match columns count ({})",
                    types.len(),
                    col_names.len()
                ));
            }
            types.clone()
        } else if first_fields.is_empty() {
            col_names.iter().map(|_| DataType::String).collect()
        } else {
            first_fields
                .iter()
                .map(|f| {
                    if f.parse::<i64>().is_ok() {
                        DataType::Integer
                    } else if f.parse::<f64>().is_ok() {
                        DataType::Float
                    } else {
                        DataType::String
                    }
                })
                .collect()
        };

        let schema = DataSchema {
            columns: col_names
                .iter()
                .zip(col_types.iter())
                .map(|(name, dt)| ColumnDef {
                    name: name.clone(),
                    data_type: dt.clone(),
                })
                .collect(),
        };

        // Parse all lines into rows.
        let rows: Vec<Vec<DataValue>> = lines
            .iter()
            .map(|line| line_to_values(line, &split_re, &schema))
            .collect();

        Ok(Box::new(ExecBatchSource { schema, rows }))
    }
}

struct ExecBatchSource {
    schema: DataSchema,
    rows: Vec<Vec<DataValue>>,
}

impl DataSource for ExecBatchSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn fetch_all(&self) -> Result<Vec<Vec<DataValue>>, String> {
        Ok(self.rows.clone())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Snapshot,
    Append,
}

#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Stdout,
    Stderr,
}

struct ExecStreamingProvider;

impl StreamingDataProvider for ExecStreamingProvider {
    fn name(&self) -> &str {
        "exec"
    }

    fn open_stream(
        &self,
        config: &HashMap<String, String>,
    ) -> Result<Box<dyn StreamingDataSource>, String> {
        let command = config
            .get("command")
            .ok_or("exec streaming provider requires 'command' config attribute")?
            .clone();

        let split_pattern = config
            .get("split")
            .ok_or("exec streaming provider requires 'split' config attribute")?
            .clone();

        let split_re = Regex::new(&split_pattern)
            .map_err(|e| format!("invalid split regex '{}': {}", split_pattern, e))?;

        let mode = match config.get("mode").map(|s| s.as_str()) {
            Some("append") => Mode::Append,
            Some("batch") => {
                // Decline streaming so the engine falls back to the batch provider.
                return Err("batch mode requested".to_string());
            }
            Some("snapshot") | None => Mode::Snapshot,
            Some(other) => {
                return Err(format!(
                    "unknown mode '{}': expected 'snapshot', 'append', or 'batch'",
                    other
                ))
            }
        };

        let stream = match config.get("stream").map(|s| s.as_str()) {
            Some("stderr") => Stream::Stderr,
            Some("stdout") | None => Stream::Stdout,
            Some(other) => {
                return Err(format!(
                    "unknown stream '{}': expected 'stdout' or 'stderr'",
                    other
                ))
            }
        };

        let header = config.get("header").map(|s| s.as_str()) == Some("true");

        let explicit_columns: Option<Vec<String>> = config
            .get("columns")
            .map(|c| c.split(',').map(|s| s.trim().to_string()).collect());

        // Parse optional timeout in seconds.
        let timeout_secs: Option<u64> = config
            .get("timeout")
            .map(|s| {
                s.parse::<u64>()
                    .map_err(|_| format!("invalid timeout '{}': must be a positive integer", s))
            })
            .transpose()?;

        // Parse optional explicit column types.
        let explicit_types: Option<Vec<DataType>> = config.get("types").map(|t| {
            t.split(',')
                .map(|s| match s.trim() {
                    "integer" => DataType::Integer,
                    "float" => DataType::Float,
                    _ => DataType::String,
                })
                .collect()
        });

        // Spawn subprocess. Read only enough lines for schema inference.
        let mut child = spawn_child(&command, stream)?;

        let mut reader: BufReader<Box<dyn std::io::Read + Send>> = match stream {
            Stream::Stdout => BufReader::new(Box::new(
                child.stdout.take().ok_or("failed to capture stdout")?,
            )),
            Stream::Stderr => BufReader::new(Box::new(
                child.stderr.take().ok_or("failed to capture stderr")?,
            )),
        };

        let mut peeked_lines: Vec<String> = Vec::new();
        let mut header_names: Option<Vec<String>> = None;
        let mut first_data_line: Option<String> = None;

        // Read up to ~12 lines to find header + first data line (skip empties/clears).
        for _ in 0..12 {
            let mut line_buf = String::new();
            match reader.read_line(&mut line_buf) {
                Ok(0) => break,
                Ok(_) => {
                    let raw = line_buf
                        .trim_end_matches('\n')
                        .trim_end_matches('\r')
                        .to_string();
                    peeked_lines.push(raw.clone());

                    for segment in split_at_clears(&raw) {
                        let stripped = strip_ansi(&segment);
                        let trimmed = stripped.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if header && header_names.is_none() {
                            header_names =
                                Some(split_re.split(&trimmed).map(|s| s.to_string()).collect());
                            continue;
                        }
                        if first_data_line.is_none() {
                            first_data_line = Some(trimmed);
                        }
                    }

                    if first_data_line.is_some() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        let first_fields: Vec<&str> = first_data_line
            .as_deref()
            .map(|line| split_re.split(line).collect())
            .unwrap_or_default();

        let col_names: Vec<String> = if let Some(ref explicit) = explicit_columns {
            explicit.clone()
        } else if let Some(ref names) = header_names {
            names.clone()
        } else {
            (0..first_fields.len())
                .map(|i| format!("col{}", i))
                .collect()
        };

        let col_types: Vec<DataType> = if let Some(ref types) = explicit_types {
            if types.len() != col_names.len() {
                return Err(format!(
                    "exec types count ({}) does not match columns count ({})",
                    types.len(),
                    col_names.len()
                ));
            }
            types.clone()
        } else if first_fields.is_empty() {
            col_names.iter().map(|_| DataType::String).collect()
        } else {
            first_fields
                .iter()
                .map(|f| {
                    if f.parse::<i64>().is_ok() {
                        DataType::Integer
                    } else if f.parse::<f64>().is_ok() {
                        DataType::Float
                    } else {
                        DataType::String
                    }
                })
                .collect()
        };

        let schema = DataSchema {
            columns: col_names
                .iter()
                .zip(col_types.iter())
                .map(|(name, dt)| ColumnDef {
                    name: name.clone(),
                    data_type: dt.clone(),
                })
                .collect(),
        };

        Ok(Box::new(ExecStreamingSource {
            schema,
            split_pattern,
            mode,
            header,
            timeout_secs,
            peeked_lines,
            reader,
            child,
        }))
    }
}

fn spawn_child(command: &str, stream: Stream) -> Result<std::process::Child, String> {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", command]);
    match stream {
        Stream::Stdout => {
            cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        }
        Stream::Stderr => {
            cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        }
    }
    cmd.spawn()
        .map_err(|e| format!("failed to spawn command '{}': {}", command, e))
}

struct ExecStreamingSource {
    schema: DataSchema,
    split_pattern: String,
    mode: Mode,
    header: bool,
    timeout_secs: Option<u64>,
    peeked_lines: Vec<String>,
    reader: BufReader<Box<dyn std::io::Read + Send>>,
    child: std::process::Child,
}

impl StreamingDataSource for ExecStreamingSource {
    fn schema(&self) -> &DataSchema {
        &self.schema
    }

    fn run(
        mut self: Box<Self>,
        sender: crossbeam_channel::Sender<StreamingUpdate>,
        shutdown: Arc<AtomicBool>,
    ) {
        let split_re = Regex::new(&self.split_pattern).unwrap();
        let deadline = self
            .timeout_secs
            .map(|s| Instant::now() + Duration::from_secs(s));

        // Chain peeked lines with remaining lines from the still-open reader.
        let peeked = self.peeked_lines.drain(..).map(LineSource::Buffered);
        let remaining = ReaderLines::new(&mut self.reader).map(LineSource::Live);
        let all_lines = peeked.chain(remaining);

        let timed_out = |deadline: Option<Instant>| -> bool {
            deadline.map(|d| Instant::now() >= d).unwrap_or(false)
        };

        match self.mode {
            Mode::Append => {
                let mut skip_header = self.header;
                for line_source in all_lines {
                    if shutdown.load(Ordering::Relaxed) || timed_out(deadline) {
                        break;
                    }
                    let raw_line = line_source.into_string();
                    let stripped = strip_ansi(&raw_line);
                    let trimmed = stripped.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if skip_header {
                        skip_header = false;
                        continue;
                    }
                    let values = line_to_values(trimmed, &split_re, &self.schema);
                    if sender.send(StreamingUpdate::Insert(values)).is_err() {
                        break;
                    }
                }
            }
            Mode::Snapshot => {
                let mut current: HashMap<Vec<String>, usize> = HashMap::new();
                let mut accumulator: Vec<String> = Vec::new();
                let mut skip_header = self.header;

                for line_source in all_lines {
                    if shutdown.load(Ordering::Relaxed) || timed_out(deadline) {
                        break;
                    }
                    let raw_line = line_source.into_string();
                    let segments = split_at_clears(&raw_line);

                    for (i, segment) in segments.iter().enumerate() {
                        if i > 0 {
                            // Clear boundary: finalize current snapshot.
                            if !accumulator.is_empty() {
                                let new = build_multiset(&accumulator, &split_re);
                                if !emit_diff(&current, &new, &self.schema, &sender) {
                                    let _ = self.child.kill();
                                    let _ = self.child.wait();
                                    let _ = sender.send(StreamingUpdate::Eof);
                                    return;
                                }
                                current = new;
                                accumulator.clear();
                            }
                        }

                        let stripped = strip_ansi(segment);
                        let trimmed = stripped.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if skip_header {
                            skip_header = false;
                            continue;
                        }
                        accumulator.push(trimmed);
                    }
                }

                // Final snapshot.
                if !accumulator.is_empty() {
                    let new = build_multiset(&accumulator, &split_re);
                    emit_diff(&current, &new, &self.schema, &sender);
                }
            }
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = sender.send(StreamingUpdate::Eof);
    }
}

enum LineSource {
    Buffered(String),
    Live(String),
}

impl LineSource {
    fn into_string(self) -> String {
        match self {
            LineSource::Buffered(s) | LineSource::Live(s) => s,
        }
    }
}

/// Iterator that reads lines from a BufReader without consuming it.
struct ReaderLines<'a> {
    reader: &'a mut BufReader<Box<dyn std::io::Read + Send>>,
}

impl<'a> ReaderLines<'a> {
    fn new(reader: &'a mut BufReader<Box<dyn std::io::Read + Send>>) -> Self {
        Self { reader }
    }
}

impl Iterator for ReaderLines<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        let mut buf = String::new();
        match self.reader.read_line(&mut buf) {
            Ok(0) => None,
            Ok(_) => Some(
                buf.trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string(),
            ),
            Err(_) => None,
        }
    }
}

fn build_multiset(lines: &[String], split_re: &Regex) -> HashMap<Vec<String>, usize> {
    let mut multiset: HashMap<Vec<String>, usize> = HashMap::new();
    for line in lines {
        let fields: Vec<String> = split_re.split(line).map(|s| s.to_string()).collect();
        *multiset.entry(fields).or_insert(0) += 1;
    }
    multiset
}

fn emit_diff(
    current: &HashMap<Vec<String>, usize>,
    new: &HashMap<Vec<String>, usize>,
    schema: &DataSchema,
    sender: &crossbeam_channel::Sender<StreamingUpdate>,
) -> bool {
    for (row, &old_count) in current {
        let new_count = new.get(row).copied().unwrap_or(0);
        if old_count > new_count {
            let values = fields_to_values(row, schema);
            for _ in 0..(old_count - new_count) {
                if sender
                    .send(StreamingUpdate::Delete(values.clone()))
                    .is_err()
                {
                    return false;
                }
            }
        }
    }
    for (row, &new_count) in new {
        let old_count = current.get(row).copied().unwrap_or(0);
        if new_count > old_count {
            let values = fields_to_values(row, schema);
            for _ in 0..(new_count - old_count) {
                if sender
                    .send(StreamingUpdate::Insert(values.clone()))
                    .is_err()
                {
                    return false;
                }
            }
        }
    }
    true
}

/// Parse a string field into a DataValue according to the column type.
/// Empty fields yield NULL. Parse failures for numeric types also yield NULL.
fn parse_field(s: &str, col_type: &DataType) -> DataValue {
    if s.is_empty() {
        return DataValue::Null;
    }
    match col_type {
        DataType::Integer => match s.parse::<i64>() {
            Ok(v) => DataValue::Integer(v),
            Err(_) => DataValue::Null,
        },
        DataType::Float => match s.parse::<f64>() {
            Ok(v) => DataValue::Float(v),
            Err(_) => DataValue::Null,
        },
        DataType::String => DataValue::String(s.to_string()),
    }
}

fn line_to_values(line: &str, split_re: &Regex, schema: &DataSchema) -> Vec<DataValue> {
    let fields: Vec<&str> = split_re.split(line).collect();
    fields
        .iter()
        .zip(schema.columns.iter())
        .map(|(s, col)| parse_field(s, &col.data_type))
        .collect()
}

fn fields_to_values(fields: &[String], schema: &DataSchema) -> Vec<DataValue> {
    fields
        .iter()
        .zip(schema.columns.iter())
        .map(|(s, col)| parse_field(s, &col.data_type))
        .collect()
}

fn strip_ansi(s: &str) -> String {
    let re = Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap();
    re.replace_all(s, "").to_string()
}

/// Split a string at ANSI clear-screen sequences.
/// Returns segments between clear boundaries.
fn split_at_clears(s: &str) -> Vec<String> {
    let clear_re = Regex::new(r"\x1b\[(2J|H|J)").unwrap();
    let mut segments = Vec::new();
    let mut last = 0;
    for m in clear_re.find_iter(s) {
        segments.push(s[last..m.start()].to_string());
        last = m.end();
    }
    segments.push(s[last..].to_string());
    segments
}
