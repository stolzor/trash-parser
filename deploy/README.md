# Долгий инкрементальный сбор через systemd (user-таймер)

Периодический `run` под пользователем (без root). Манифест resumable + дедуп по
`(platform, id)` → каждый прогон **инкрементален**: новые загрузки добираются,
сделанное пропускается. Trending/поиск со временем даёт свежие видео — набор
растёт и остаётся актуальным.

## Установка (на сервере, один раз)

```bash
# 0. Собрать release-бинарь
cd ~/detox-parser                       # ← путь к репозиторию
cargo build --release -p detox-parser-cli

# 1. Env с кредами (НЕ в git)
cp config/detox.env.example config/detox.env
$EDITOR config/detox.env                # проверь YTDLP_BIN и креды MinIO

# 2. Юниты в пользовательский каталог systemd
mkdir -p ~/.config/systemd/user
cp deploy/detox-collect.service deploy/detox-collect.timer ~/.config/systemd/user/
$EDITOR ~/.config/systemd/user/detox-collect.service   # поправь WorkingDirectory,
                                                        # если репо НЕ в ~/detox-parser

# 3. Чтобы таймер работал и после logout/reboot
loginctl enable-linger "$USER"

# 4. Включить и запустить таймер
systemctl --user daemon-reload
systemctl --user enable --now detox-collect.timer
```

## Управление

```bash
systemctl --user list-timers detox-collect.timer   # когда следующий прогон
systemctl --user start detox-collect.service       # прогнать прямо сейчас
journalctl --user -u detox-collect.service -f      # смотреть лог live
systemctl --user status detox-collect.service      # результат последнего прогона
```

Прогресс сбора — как обычно: `detox-parser status` (или через docker exec в
Postgres). Темп задаётся в `config/seeds.youtube.toml` (`[concurrency]`) — пока
идёт сбор с одного серверного IP держим `meta_workers=1, requests_per_sec=1`
(бережно против бот-гейта). С rotating residential темп можно поднять.

## Интервал

`OnUnitActiveSec=6h` в `detox-collect.timer` — прогон каждые 6 ч после
завершения предыдущего. Правь под нужную частоту (`3h`, `12h`, …).
