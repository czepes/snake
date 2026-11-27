# Змейка по сети (протокол v2) — Rust + TUI

Многопользовательская «Змейка»: игры в локальной сети (multicast `239.192.0.4:9193`) и на сервере курса
(`snakes.ippolitov.me:9193`) в одном списке. Терминальный интерфейс на ratatui.

## Запуск

    cargo run --release                       # обычный запуск
    cargo run --release -- --name Вася        # своё имя
    cargo run --release -- --no-animations    # без анимаций интерфейса
    cargo run --release -- --no-server        # только локальная сеть
    cargo run --release -- --no-multicast --server 127.0.0.1:45101   # только «сервер» (проверка unicast-пути)
    cargo run --release -- --port 45101       # фиксированный порт игрового сокета
    cargo run --release -- --log trace.log    # журнал протокола (или SNAKE_LOG=trace.log)

## Устройство

    wire.rs      кодек protobuf (вручную), типы сообщений, проверки
    rules.rs     правила: тор, ход, столкновения, очки, размещение, еда, конвертация змеек
    reliable.rs  ACK, повторы 100 мс, окно дедупликации 1024, Ping, тайм-аут связи 1000 мс
    catalog.rs   каталог игр (LAN + сервер), TTL 3 с
    node.rs      сетевое ядро без ввода-вывода: MASTER/DEPUTY/NORMAL, присоединение, failover, выход
    net.rs       UDP-сокеты, multicast на всех интерфейсах, DNS-поток
    ui.rs        TUI;
    ui/animations.rs  эффекты tachyonfx: еда, волна по телу, появление/уход змеек, переходы экранов
    main.rs — цикл (потоков два: основной и DNS)
