# Transitive closure / reachability example.
#
# Base resources define direct edges in a graph.
# The "path" resources derive transitive reachability by referencing
# both "edge" and "path" blocks, creating a recursive definition.

resource "edge" "e1" {
  src = 1
  dst = 2
}

resource "edge" "e2" {
  src = 2
  dst = 3
}

resource "edge" "e3" {
  src = 3
  dst = 4
}

resource "path" "direct" {
  from = edge.e1.src
  to   = edge.e1.dst
}

resource "path" "transitive" {
  from = path.direct.from
  to   = edge.e2.dst
}
