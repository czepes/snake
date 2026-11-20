# Змейка по сети

Задание: [snakes_task.md](snakes_task.md). Схема:
[snakes.proto](src/main/protobuf/snakes.proto).

Примеры и тесты протокола: `go test ./...` (Go 1.24 или новее).
GitLab CI выполняет Go-тесты с детектором гонок и `go vet`.
Java и Maven для сборки и тестирования не нужны.

Go-типы в `protocol/snakes.pb.go` включены в репозиторий. После изменения
схемы выполните `scripts/generate.sh` с установленными `protoc` и
`protoc-gen-go` (проверенные версии: 3.7.1 и v1.36.10).
Java options в схеме сохранены для совместимости сторонних клиентов.
