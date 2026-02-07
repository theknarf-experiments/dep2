# Multiple Kafka topics feeding into the same program.

data "kafka" "orders" {
    brokers  = "localhost:9092"
    topic    = "orders"
    group_id = "dbflow-multi"
}

data "kafka" "payments" {
    brokers  = "localhost:9092"
    topic    = "payments"
    group_id = "dbflow-multi"
}

output "order_stream" {
    value = data.kafka.orders.value
}

output "payment_stream" {
    value = data.kafka.payments.value
}
