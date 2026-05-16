# Getting started

rterm читает конфигурацию из двух мест:

- **`config.toml`** — декларативные настройки (шрифт, окно, шелл, палитра, кейбинды).
- **`init.lua` + `plugins/*.lua`** — императивная логика (хуки событий, кастомные действия, реакции).

Оба источника подхватываются «горячо» — изменения применяются на сохранение без перезапуска. При перезагрузке плагины получают событие `reload`.

## Установка и запуск

```bash
git clone <repo> rterm
cd rterm
cargo run -p rterm-app              # GUI
cargo run -p rterm-app --release    # оптимизированный GUI
cargo run -p rterm-app -- --smoke   # headless smoke-тест (без окна)
```

Полная сборка занимает ~1 минуту на ноутбуке среднего уровня. Зависимости (winit / wgpu / glyphon / cosmic-text / mlua) тяжёлые — после `cargo clean` rebuild ≈ 3 мин.

## Где лежат файлы

| Платформа | Путь |
|-----------|------|
| Linux (XDG) | `~/.config/rterm/` |
| macOS | `~/Library/Application Support/rterm/` (или `~/.config/rterm/`, если задан `XDG_CONFIG_HOME`) |
| Windows | `%APPDATA%\rterm\` |

Точные пути, которые rterm видит прямо сейчас:

```bash
rterm --print-paths --json
```

Переопределить путь к `config.toml` для одного запуска: `--config <path>` или переменная `RTERM_CONFIG_PATH`.

## Быстрый старт

Сгенерировать закомментированный шаблон со всеми полями:

```bash
rterm --print-default-config > ~/.config/rterm/config.toml
```

Раскомментируй нужные строки; всё остальное rterm возьмёт из встроенных дефолтов.

Проверить, что конфиг и Lua валидны без запуска GUI:

```bash
rterm --check
```

Распечатать «эффективный» конфиг после слияния дефолтов и CLI-флагов:

```bash
rterm --print-config
```

## Hot-reload

rterm раз в секунду опрашивает mtime у `config.toml`, `init.lua` и всех файлов в `plugins/*.lua`:

- **`config.toml`** перечитывается, применяются новые цвета / шрифт / opacity / клавиши.
- **`*.lua`** — Lua-стейт пересоздаётся, прежние плагины отключаются, новые загружаются заново; всем регистрированным хукам приходит событие `reload`.

См. [Plugins guide](plugins.md) про то, как правильно регистрировать обработчики, чтобы они переживали reload.

## Переменные окружения

| Переменная | Допустимые значения | Назначение |
|------------|---------------------|------------|
| `RTERM_CONFIG_PATH` | путь до файла `.toml` | Альтернативный путь до `config.toml`, перекрывает дефолтное расположение. |
| `RTERM_SMOKE_COMMAND` | любая shell-команда | В режиме `--smoke` заменяет встроенную `echo hello rterm`. |
| `RUST_LOG` | спецификация tracing-фильтра | Уровни логирования, напр. `RUST_LOG=rterm=info,wgpu_hal=warn`. |
| `WGPU_BACKEND` | `vulkan` `gl` `metal` `dx12` `primary` `secondary` | Выбор GPU-бэкенда. На WSL2 авто-дефолт = `gl` (Mesa Vulkan виснет в init). |
| `WGPU_PRESENT_MODE` | `fifo` `mailbox` `immediate` `autovsync` `autonovsync` | Режим презентации. На WSL2 авто-дефолт = `fifo` (llvmpipe заклинивает на `AutoVsync`). |
| `WGPU_DEBUG` | `0` / `1` / `true` / `false` | `1` включает validation-слои wgpu и debug-калбэки. |
| `WAYLAND_DISPLAY` | имя Wayland-сокета | Если не задан → winit падает на X11. |
| `SHELL` | путь до шелла | Резервный шелл, когда `[shell] program = ""`. |

## CLI

```
rterm [OPTIONS]
  --config <path>                     Загрузить указанный config.toml
  --smoke [--json]                    Headless PTY+parser sanity-проход
  --render-test                       Открыть окно, показать один clear-only frame, выйти
  --list-actions [prefix] [--labels|--json]
  --list-events  [prefix] [--json]
  --list-keybindings [substr] [--json]
  --list-fonts   [substr] [--json]
  --print-config                      Эффективный config.toml на stdout
  --print-default-config              Закомментированный шаблон
  --print-paths [--json]              Резолвлённые пути config / plugins / cache
  --check                             Валидация config + Lua, non-zero на ошибке
  --font-size <pt>                    Перекрыть размер шрифта на запуск
  --font-family <s>                   Перекрыть семейство шрифта на запуск
  --version [--json]
  --help
```

## Куда идти дальше

- Хочешь настроить шрифт, окно, шелл, цвета — [Configuration](config.md).
- Хочешь свои сочетания клавиш — [Keybindings](keybindings.md).
- Хочешь сменить тему или сохранить выбор — [Themes & Appearance](themes.md).
- Хочешь автоматизацию через Lua — [Plugins guide](plugins.md).
