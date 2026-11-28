#!/usr/bin/env bash
set -euo pipefail

neo4j_image="neo4j:5.26.34-community@sha256:2c48628bc28c68d7e2787f76262dfc9d7c21a8f7f290e30da901c489aa7fe50d"
memgraph_image="memgraph/memgraph:3.14.0@sha256:a732421217951066e42f5749111bc3d0c7b51b12c756be04850d4dafa84cc05e"
falkordb_image="falkordb/falkordb:6.2.1@sha256:56160275dcd432daf441e761873c0747016ae222d9be4218433f5d009bf02079"
neo4j_port="${BENCH_NEO4J_PORT:-17687}"
memgraph_port="${BENCH_MEMGRAPH_PORT:-17688}"
falkordb_port="${BENCH_FALKORDB_PORT:-16379}"
names=(rhizodb-bench-neo4j rhizodb-bench-memgraph rhizodb-bench-falkordb)

wait_for() {
  local name="$1" ready="$2"
  for _ in $(seq 1 120); do
    if eval "$ready" > /dev/null 2>&1; then
      return 0
    fi
    if [[ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" != "true" ]]; then
      docker logs "$name" >&2 || true
      echo "$name stopped before it was ready" >&2
      return 1
    fi
    sleep 1
  done
  echo "$name was not ready after two minutes" >&2
  return 1
}

stop() {
  docker rm -f "${names[@]}" > /dev/null 2>&1 || true
}

start() {
  stop
  docker run -d --name rhizodb-bench-neo4j -p "127.0.0.1:$neo4j_port:7687" \
    -e NEO4J_AUTH=none -e NEO4J_server_memory_heap_max__size=2G \
    -e NEO4J_server_memory_pagecache_size=1G "$neo4j_image" > /dev/null
  docker run -d --name rhizodb-bench-memgraph -p "127.0.0.1:$memgraph_port:7687" \
    "$memgraph_image" --telemetry-enabled=false > /dev/null
  docker run -d --name rhizodb-bench-falkordb -p "127.0.0.1:$falkordb_port:6379" \
    "$falkordb_image" > /dev/null
  wait_for rhizodb-bench-neo4j \
    "docker exec rhizodb-bench-neo4j cypher-shell -a bolt://localhost:7687 'RETURN 1'"
  wait_for rhizodb-bench-memgraph \
    "docker logs rhizodb-bench-memgraph 2>&1 | grep -q 'You are running Memgraph'"
  wait_for rhizodb-bench-falkordb \
    "docker exec rhizodb-bench-falkordb redis-cli PING | grep -q PONG"
  echo "export BENCH_NEO4J_URL=bolt://127.0.0.1:$neo4j_port"
  echo "export BENCH_MEMGRAPH_URL=bolt://127.0.0.1:$memgraph_port"
  echo "export BENCH_FALKORDB_URL=redis://127.0.0.1:$falkordb_port"
}

case "${1:-}" in
  start) start ;;
  stop) stop ;;
  *)
    echo "usage: servers.sh start|stop" >&2
    exit 2
    ;;
esac
