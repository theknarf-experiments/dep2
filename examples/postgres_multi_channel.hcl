# Multiple PostgreSQL LISTEN channels feeding into the same program.

data "postgres" "orders" {
    connection = "host=localhost user=postgres password=postgres dbname=postgres"
    channel    = "orders"
}

data "postgres" "payments" {
    connection = "host=localhost user=postgres password=postgres dbname=postgres"
    channel    = "payments"
}

output "order_stream" {
    value = data.postgres.orders.value
}

output "payment_stream" {
    value = data.postgres.payments.value
}
