# PostgreSQL streaming with a derived resource.
#
# Notifications flow through an "alert" resource that tags them,
# showing that Datalog IDB rules work on live streaming data.

data "postgres" "events" {
    connection = "host=localhost user=postgres password=postgres dbname=postgres"
    channel    = "events"
}

resource "alert" "critical" {
    message = data.postgres.events.value
}

output "alerts" {
    value = alert.critical.message
}
