# DbFlow

An HCL-based front-end for incremental Datalog. Define data sources, derivation rules, and outputs in a declarative HCL syntax — DbFlow compiles them to Datalog and executes them using differential dataflow.

## Quick Start

```bash
cargo build --release

# Batch mode: derive facts and print outputs
dbflow examples/variables.hcl

# Streaming mode: continuously process data from external sources
dbflow examples/csv_watch.hcl

# Print the generated Datalog without executing
dbflow examples/recursion.hcl --emit-dl
```

## CLI Usage

```
dbflow [OPTIONS] <INPUT>

Arguments:
  <INPUT>  Input HCL file

Options:
      --emit-dl          Print generated Datalog and exit
  -f, --facts <DIR>      Path to EDB .facts files
  -c, --csvs <DIR>       Output directory for IDB result .csv files
  -w, --workers <N>      Number of worker threads [default: 1]
```

When a program references streaming data sources (CSV, Kafka, PostgreSQL, exec), DbFlow enters streaming mode and runs continuously until interrupted with Ctrl-C.

## HCL Syntax

### Variables

Compile-time constants, referenced with `var.<name>`.

```hcl
variable "threshold" {
  default = 80
}

variable "region" {
  default = "us-west"
}
```

### Resources

Define facts and derivation rules. Each resource has a type name and a label. Attributes can be literals, references to other resources, data block fields, or expressions.

```hcl
# EDB: a literal fact
resource "server" "web1" {
  ip     = "10.0.0.5"
  region = var.region
}

# IDB: a derived rule joining two sources
resource "monitor" "rule" {
  ip     = server.web1.ip
  region = server.web1.region
}
```

### Data Blocks

Declare external data sources backed by plugins. Each plugin provides its own configuration keys.

```hcl
data "csv" "orders" {
  path = "/tmp/orders.csv"
}

data "exec" "procs" {
  command = "ps aux --no-header"
  split   = "\\s+"
  mode    = "snapshot"
  columns = "user,pid,cpu,mem,command"
}
```

Reference data fields with `data.<provider>.<label>.<column>`.

### Outputs

Expose derived values as program output.

```hcl
output "result" {
  value = monitor.rule.ip
}
```

### Modules

Include other HCL files and pass variables.

```hcl
module "network" {
  source    = "./network.hcl"
  threshold = 100
}
```

## Expressions

| Expression | Example | Description |
|---|---|---|
| Literal | `"hello"`, `42`, `true` | String, integer, or boolean constant |
| Reference | `server.web1.ip` | Field from another resource |
| Data reference | `data.csv.orders.amount` | Field from a data block |
| Variable | `var.threshold` | Compile-time variable |
| Negation | `!blocked.b1.ip` | Antijoin (filter out matching rows) |
| Comparison | `data.csv.orders.amount > 50` | Filter predicate (`==`, `!=`, `<`, `<=`, `>`, `>=`) |
| Aggregate | `sum(data.csv.orders.amount)` | Aggregation (`count`, `sum`, `min`, `max`) |
| Arithmetic | `data.csv.orders.amount + data.csv.orders.tax` | Arithmetic (`+`, `-`, `*`, `/`, `%`) |

Comparisons are used in attributes prefixed with `_` (e.g., `_filter`) to exclude them from the output schema:

```hcl
resource "expensive" "rule" {
  customer = data.csv.orders.customer
  _filter  = data.csv.orders.amount > 1000
}
```

## Data Providers

### CSV

Watches a CSV file for changes. Inserts and retractions are computed by diffing the file contents on each modification.

```hcl
data "csv" "sales" {
  path = "/tmp/sales.csv"
}
```

- `path` (required): Path to the CSV file. First row is treated as column headers. Column types are inferred from the first data row.

### Kafka

Consumes messages from a Kafka topic as a streaming data source.

```hcl
data "kafka" "events" {
  brokers  = "localhost:9092"
  topic    = "events"
  group_id = "dbflow-consumer"
}
```

- `brokers` (required): Kafka broker addresses
- `topic` (required): Topic to consume
- `group_id` (optional, default `"dbflow-consumer"`): Consumer group ID

### PostgreSQL

Listens for notifications on a PostgreSQL channel via `LISTEN`/`NOTIFY`.

```hcl
data "postgres" "alerts" {
  connection = "host=localhost user=postgres password=postgres dbname=postgres"
  channel    = "alerts"
}
```

- `connection` (required): PostgreSQL connection string
- `channel` (required): NOTIFY channel name

### Exec

Runs a shell command and streams its output as data. Supports two modes: snapshot (diff between refreshes) and append (each line is an insert).

```hcl
data "exec" "procs" {
  command = "watch -t -n2 'ps aux --no-header'"
  split   = "\\s+"
  mode    = "snapshot"
  columns = "user,pid,cpu,mem,vsz,rss,tty,stat,start,time,command"
}
```

- `command` (required): Shell command, executed via `sh -c`
- `split` (required): Regex for splitting each output line into columns
- `mode` (optional, default `"snapshot"`): `"snapshot"` detects ANSI clear-screen sequences and diffs snapshots; `"append"` treats each line as an insert
- `stream` (optional, default `"stdout"`): `"stdout"` or `"stderr"`
- `columns` (optional): Comma-separated column names. If omitted, auto-generated as `col0`, `col1`, ...
- `header` (optional, default `"false"`): `"true"` treats the first line as column names

## Examples

**Filter high-value orders from a CSV:**

```hcl
data "csv" "orders" {
  path = "/tmp/orders.csv"
}

resource "big_order" "rule" {
  customer = data.csv.orders.customer
  _filter  = data.csv.orders.amount > 50
}

output "big_customers" {
  value = big_order.rule.customer
}
```

**Aggregate sales by region:**

```hcl
data "csv" "sales" {
  path = "/tmp/sales.csv"
}

resource "region_totals" "all" {
  region = data.csv.sales.region
  total  = sum(data.csv.sales.amount)
}

output "totals" {
  value = region_totals.all.total
}
```

**Transitive closure (reachability):**

```hcl
resource "edge" "ab" { src = "a"  dst = "b" }
resource "edge" "bc" { src = "b"  dst = "c" }
resource "edge" "cd" { src = "c"  dst = "d" }

resource "path" "direct" {
  from = edge.ab.src
  to   = edge.ab.dst
}

resource "path" "transitive" {
  from = path.direct.from
  to   = edge.bc.dst
}

output "reachable" {
  value = path.direct.to
}
```

**Monitor high-CPU processes:**

```hcl
data "exec" "procs" {
  command = "watch -t -n2 'ps aux --no-header'"
  split   = "\\s+"
  mode    = "snapshot"
  columns = "user,pid,cpu,mem,vsz,rss,tty,stat,start,time,command"
}

resource "busy" "rule" {
  user = data.exec.procs.user
  pid  = data.exec.procs.pid
  _filter = data.exec.procs.cpu > 5
}

output "busy_processes" {
  value = busy.rule.pid
}
```

## Architecture

```
dbflow (CLI)
├── dbflow-core          HCL parsing, compilation to Datalog
├── dbflow-plugin        Plugin trait definitions (DataProvider, StreamingDataProvider)
├── dbflow-plugin-csv    CSV file watching plugin
├── dbflow-plugin-kafka  Kafka consumer plugin
├── dbflow-plugin-postgres  PostgreSQL LISTEN/NOTIFY plugin
├── dbflow-plugin-exec   Subprocess streaming plugin
└── flowlog/             Datalog engine (parsing, planning, executing)
    ├── parsing          Datalog parser
    ├── catalog          Rule and atom representations
    ├── strata           Stratification (dependency analysis)
    ├── optimizing       Query optimization
    ├── planning         Execution plan generation
    ├── reading          EDB loading, arrangements, string interning
    ├── executing        Differential dataflow execution
    └── macros           Code generation macros
```

## Building

```bash
cargo build --release
```

The Kafka plugin requires `libcurl4-openssl-dev` on Debian/Ubuntu.

## Testing

```bash
# Run all e2e tests
cargo test --test e2e

# Run tests for a specific plugin
cargo test --test e2e e2e_exec
cargo test --test e2e e2e_csv
```
