# Minimal PostgreSQL LISTEN/NOTIFY streaming example.
#
# 1. Start Postgres: docker compose up -d
# 2. Run dbflow:     cargo run -- examples/postgres_hello.hcl
# 3. Send NOTIFY:    docker compose exec postgres \
#                      psql -U postgres -c "NOTIFY greetings, 'hello world'"
# 4. See output:     output "messages": hello world
# 5. Stop:           Ctrl-C

data "postgres" "messages" {
    connection = "host=localhost user=postgres password=postgres dbname=postgres"
    channel    = "greetings"
}

output "messages" {
    value = data.postgres.messages.value
}
