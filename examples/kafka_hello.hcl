# Minimal Kafka streaming example.
#
# 1. Start Kafka:   docker compose up -d
# 2. Run dbflow:    cargo run -- examples/kafka_hello.hcl
# 3. Produce msgs:  echo "hello world" | docker compose exec -T kafka \
#                     /opt/kafka/bin/kafka-console-producer.sh \
#                     --bootstrap-server localhost:9092 --topic greetings
# 4. See output:    output "messages": hello world
# 5. Stop:          Ctrl-C

data "kafka" "messages" {
    brokers  = "localhost:9092"
    topic    = "greetings"
    group_id = "dbflow-hello"
}

output "messages" {
    value = data.kafka.messages.value
}
