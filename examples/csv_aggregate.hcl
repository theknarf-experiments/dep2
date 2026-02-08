// CSV aggregate example.
//
// Usage:
//   echo -e "region,amount\nus,100\nus,200\neu,50\neu,75" > /tmp/dbflow-sales.csv
//   cargo run -- examples/csv_aggregate.hcl
//
// Shows sum of amounts grouped by region.

data "csv" "sales" {
  path = "/tmp/dbflow-sales.csv"
}

resource "region_totals" "all" {
  region = data.csv.sales.region
  total  = sum(data.csv.sales.amount)
}

output "totals" {
  value = region_totals.all.total
}
