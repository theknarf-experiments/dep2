# Variable substitution example.
#
# Variables are compile-time constants that get inlined into resource blocks.

variable "threshold" {
  default = 80
}

variable "region" {
  default = "us-west"
}

resource "config" "main" {
  limit  = var.threshold
  region = var.region
}
