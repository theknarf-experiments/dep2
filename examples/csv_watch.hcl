// CSV file watching example.
//
// Usage:
//   cargo run -- examples/csv_watch.hcl
//
// Then edit /tmp/dbflow-demo.csv in another terminal to see
// inserts and retractions appear in real-time.
//
// Create the demo CSV first:
//   echo -e "name,city\nAlice,NYC\nBob,LA" > /tmp/dbflow-demo.csv

data "csv" "people" {
  path  = "/tmp/dbflow-demo.csv"
  watch = "true"
}

output "names" {
  value = data.csv.people.name
}
