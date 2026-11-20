#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
# Requires protoc and protoc-gen-go; ordinary tests use the committed Go binding.
protoc -I src/main/protobuf --go_out=. --go_opt=module=snakes-protocol --go_opt=Msnakes.proto=snakes-protocol/protocol src/main/protobuf/snakes.proto
