# Kafka streaming with a derived resource.
#
# Messages flow through an "alert" resource that tags them,
# showing that Datalog IDB rules work on live streaming data.

data "kafka" "events" {
    brokers  = "localhost:9092"
    topic    = "events"
    group_id = "dbflow-alert"
}

resource "alert" "critical" {
    message = data.kafka.events.value
}

output "alerts" {
    value = alert.critical.message
}
