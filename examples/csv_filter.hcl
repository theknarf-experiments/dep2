// CSV comparison filter example.
//
// Usage:
//   echo -e "customer,amount\nalice,100\nbob,30\ncharlie,75" > /tmp/dbflow-orders.csv
//   cargo run -- examples/csv_filter.hcl
//
// Only orders with amount > 50 will appear in the output.

data "csv" "orders" {
  path = "/tmp/dbflow-orders.csv"
}

resource "big_order" "rule" {
  customer = data.csv.orders.customer
  amount   = data.csv.orders.amount
  _filter  = data.csv.orders.amount > 50
}

output "big_customers" {
  value = big_order.rule.customer
}
