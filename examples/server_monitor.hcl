# Server monitoring example from the design doc.
#
# Base resources (EDB): servers with IPs and data centers.
# Derived resource (IDB): monitor that references a server's IP.

resource "server" "web1" {
  ip = "10.0.0.5"
  dc = "us-west"
}

resource "server" "web2" {
  ip = "10.0.0.6"
  dc = "us-east"
}

resource "monitor" "m1" {
  target_ip = server.web1.ip
}

output "monitors" {
  value = monitor.m1.target_ip
}
