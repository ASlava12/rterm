# Plugins guide

rterm встраивает Lua 5.4 (через `mlua` с вендоренным интерпретатором, скомпилированным внутрь бинарника). Плагины — императивный слой настройки: реагировать на события, регистрировать действия, дёргать пользовательский API.

Защита от зависаний: каждый вызов хэндлера/действия ограничен бюджетом ~2 секунды (загрузка скрипта — 10), после чего исполнение прерывается Lua-ошибкой с записью в лог. `while true do end` в хэндлере больше не замораживает терминал.

Декларативные настройки (шрифт, цвета, кейбинды) живут в [Configuration](config.md). Полный справочник API — [Plugins API reference](plugins-api.md).

## Файлы

| Путь | Что это |
|------|---------|
| `~/.config/rterm/init.lua` | Главный файл. Всегда загружается первым. |
| `~/.config/rterm/plugins/*.lua` | Дополнительные модули. Загружаются в алфавитном порядке после `init.lua`. |

Все три источника (`config.toml`, `init.lua`, `plugins/*.lua`) горячо перезагружаются по mtime. Перед перезагрузкой хост сбрасывает все подписки (`rterm.on`), действия (`register_action`) и матч-правила (`add_match`), и скрипты регистрируют их заново — поэтому хуки обычно ставят в начале файла на верхнем уровне, без условий вроде «уже подписан». Важно: сама Lua-VM не пересоздаётся — глобальные переменные (`_G.*`) переживают перезагрузку; не полагайтесь на их отсутствие при инициализации. Плагин с синтаксической ошибкой пропускается с warn-логом, остальные продолжают загружаться.

Точные пути показывает `rterm --print-paths --json`.

## «Hello, rterm»

`~/.config/rterm/init.lua`:

```lua
rterm.log("init.lua loaded")

rterm.on("startup", function()
  rterm.log("rterm запустился, версия " .. rterm.version())
end)

rterm.on("bell", function()
  rterm.notify("Терминал звякнул", "проверь активную панель")
end)

-- Кастомное действие, доступное в палитре команд и кейбиндах
rterm.register_action("my_clear_and_ls", function()
  rterm.send_input("clear && ls -la\n")
end)
```

Чтобы повесить кейбинд на новое действие, в `config.toml`:

```toml
[[keybindings]]
keys   = "Ctrl+Shift+L"
action = "my_clear_and_ls"
```

## Жизненный цикл

1. **Старт** — `rterm` инициализирует Lua-стейт, выполняет `init.lua`, затем все `plugins/*.lua` по алфавиту.
2. **`startup`** — синхронное событие после полной инициализации плагинов, ещё ДО первого окна.
3. **`ready`** — фоновое событие после открытия окна и первого фрейма.
4. **Работа** — события сыплются по мере действий пользователя и шелл-вывода.
5. **Hot-reload** — на изменение `init.lua` / `plugins/*.lua` / `config.toml`: всем регистрированным хукам приходит `reload`, затем Lua-стейт пересоздаётся и инициализация повторяется заново.
6. **`shutdown`** — последнее событие при закрытии окна. Полезно сохранить состояние во внешний файл.

## События

Каждый хук — это `function(payload) ... end`. Параметр `payload` — строка (часто JSON или просто текст), зависит от типа события. Внутри хука можно вызывать любой `rterm.*` API.

Подписка:

```lua
rterm.on("pane.exit", function(payload)
  -- payload — JSON со структурой { tab = N, pane = N, uid = N, exit_code = N }
  rterm.log("pane exited: " .. payload)
end)
```

Отписка одной строкой убирает ВСЕ хуки, привязанные к этому событию:

```lua
rterm.off("pane.exit")  -- возвращает число удалённых обработчиков
```

### Список событий

Живой канонический список: `rterm --list-events --json`. На момент написания:

**Жизненный цикл**
`startup` · `ready` · `reload` · `shutdown` · `frame.tick`

**Ввод и буфер обмена**
`key` · `paste` · `copy` · `selection.end` · `osc52.write`

**Окно**
`window.focus` · `window.resize` · `resize` · `title` · `theme`

**Вкладки**
`tab.new` · `tab.close` · `tab.switch` · `tab.move` · `tab.title`
`tab.activity` · `tab.silence` · `tab.unread` · `tab.read` · `tab.progress`
`tab.drag_start` · `tab.drag_end` · `tab.alt_enter` · `tab.alt_leave`

**Панели**
`pane.split` · `pane.close` · `pane.focus` · `pane.exit` · `pane.swap`
`pane.resize` · `pane.title` · `pane.cwd` · `pane.output` · `pane.silence`
`pane.alt_enter` · `pane.alt_leave` · `pane.reverse_screen`
`pane.scrollback_enter` · `pane.scrollback_leave`
`pane.cursor_blink` · `pane.cursor_shape` · `pane.cursor_visible`
`pane.mouse_mode` · `pane.bell_mute` · `pane.zoom`
`pane.command_finish` · `pane.slow_command` · `pane.shell_exit`

**Прокрутка и поиск**
`scroll` · `scrollback.clear` · `scrollback.save`
`search.start` · `search.step` · `search.end`

**Шелл-интеграция (OSC 133)**
`prompt.jump` · `command.jump` · `output.line` · `cwd`

**Палитра команд**
`palette.open` · `palette.close`

**Линки и матчи**
`link.hover` · `link.unhover` · `link.open` · `match`

**Уведомления**
`notification` · `progress` · `bell` · `attention` · `shell.exit`

### Формат полезной нагрузки

Большая часть событий, относящихся к конкретной вкладке/панели, отдаёт JSON-строку с полями `tab`, `pane`, `uid`. Точная схема каждого события — в [crates/rterm-render/src/lib.rs](../crates/rterm-render/src/lib.rs), функции вида `pane_event_payload`, `tab_event_payload`. Простой способ узнать формат — подписаться и распечатать:

```lua
rterm.on("pane.exit", function(payload)
  rterm.log("pane.exit -> " .. payload)
end)
```

Логи смотреть через `RUST_LOG=rterm::plugin=info cargo run -p rterm-app`.

## Actions

```lua
rterm.register_action("name", function() ... end)
rterm.unregister_action("name")
rterm.run_action("name")  -- программно выстрелить действие по имени
```

Зарегистрированное действие появляется и в палитре команд (`Ctrl+Shift+P`), и в `config.toml` под `[[keybindings]] action = "name"`. Удалить регистрацию можно `rterm.unregister_action`.

Полный список встроенных действий — `rterm --list-actions` (или [Keybindings](keybindings.md#actions)).

## Идиомы

### Авто-копирование на отпускание мыши

```lua
rterm.on("selection.end", function(text)
  rterm.copy(text)
end)
```

(rterm намеренно не делает этого по умолчанию — пользователи не любят, когда выделение клобберит PRIMARY на каждом тыке мыши.)

### Подсветить долгие команды и уведомить

```lua
rterm.set_slow_command_ms(15000)

rterm.on("pane.slow_command", function(payload)
  rterm.notify("Долгая команда", payload)
end)
```

### Реакция на URL в выводе

```lua
rterm.add_match("github_pr", "https?://github%.com/[%w-]+/[%w-]+/pull/%d+", {
  regex = true,
  on = function(text)
    rterm.log("Заметил PR: " .. text)
  end,
})
```

### Кастомная команда «обновить и собрать»

```lua
rterm.register_action("rebuild", function()
  rterm.send_input("cargo update && cargo build --workspace\n")
end)
```

В `config.toml`:

```toml
[[keybindings]]
keys   = "Ctrl+Shift+B"
action = "rebuild"
```

### Состояние между запусками

```lua
local state_file = rterm.cache_dir() .. "/my_state.json"

rterm.on("startup", function()
  local f = io.open(state_file, "r")
  if f then
    local content = f:read("*a")
    f:close()
    rterm.log("Прошлая сессия: " .. content)
  end
end)

rterm.on("shutdown", function()
  local f = io.open(state_file, "w")
  if f then
    f:write("last_tab=" .. rterm.active_tab() .. "\n")
    f:close()
  end
end)
```

## Отладка

- **Логи Lua**: `RUST_LOG=rterm::plugin=info cargo run -p rterm-app` (или просто `rterm`, если установлен).
- **Проверка синтаксиса без GUI**: `rterm --check` — пройдёт по `config.toml` + всем `*.lua`, и выйдет с ненулевым кодом при ошибке.
- **Список встроенных**: `rterm --list-events --json` / `--list-actions --json`. На любом ответе есть `name` и `summary`, удобно генерить документацию плагинов.
- **Висит ли хук**: проверь `rterm.handler_counts()` в `init.lua`.

## Что внутри Lua недоступно

- Прямого доступа к PTY байтам нет — пиши через `rterm.send_input` / `rterm.send_to_pane`.
- `os.exit()` уронит весь rterm (это один процесс, один Lua-стейт). Используй `rterm.run_action("quit")` для штатного выхода.
- Долгие синхронные операции в хуках блокируют ввод. Для тяжёлых задач — пиши во внешний скрипт через `send_input` или используй `coroutine`.
- Сетевой и файловый I/O — стандартный Lua (`io`, `socket` если линкуется отдельно), не через `rterm.*`. Будь осторожен с правами и таймаутами.

## Дальше

- [Plugins API reference](plugins-api.md) — полная справка по `rterm.*`.
