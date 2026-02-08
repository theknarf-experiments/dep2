# Debezium CDC streaming example.
#
# Starts an HTTP server on port 8080 that receives Debezium change events
# for the "public.users" table and outputs user names.
#
# Configure Debezium Server with:
#   sink.type=http
#   sink.http.url=http://localhost:8080
#   debezium.format.value=json
#   debezium.format.value.schemas.enable=false

data "debezium" "users" {
  listen  = "0.0.0.0:8080"
  table   = "public.users"
  columns = "id,name,email"
  types   = "integer,string,string"
}

resource "active_user" "rule" {
  name  = data.debezium.users.name
  email = data.debezium.users.email
}

output "users" {
  value = active_user.rule.name
}
