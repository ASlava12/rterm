# Плагины rterm на Lua

rterm встраивает Lua 5.4 (через `mlua` + LuaJIT, скомпилированный внутрь бинарника). Плагины — это императивный слой настройки: реагировать на события, регистрировать действия, дёргать пользовательский API. Декларативные настройки (шрифт, цвета, кейбинды) живут в [config.md](config.md).

## Файлы

| Путь | Что это |
|------|---------|
| `~/.config/rterm/init.lua` | Главный файл. Всегда загружается первым. |
| `~/.config/rterm/plugins/*.lua` | Дополнительные модули. Загружаются в алфавитном порядке после `init.lua`. |

Все три источника (`config.toml`, `init.lua`, `plugins/*.lua`) горячо перезагружаются по mtime. После переинициализации Lua-стейт чистый, и подписки регистрируются заново — поэтому в начале файла обычно ставят все хуки на верхнем уровне, без условий вроде «уже подписан».

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
5. **Hot-reload** — на изменение `init.lua` / `plugins/*.lua` / `config.toml`: всем регистрированным хукам приходит событие `reload`, затем Lua-стейт пересоздаётся и инициализация повторяется заново.
6. **`shutdown`** — последнее событие при закрытии окна. Полезно сохранить состояние во внешний файл.

## События

Каждый хук — это `function(payload) ... end`. Параметр `payload` — строка (часто JSON или просто текст), зависит от типа события. Внутри хука можно вызывать любой `rterm.*` API.

Подписка:

```lua
rterm.on("pane.exit", function(payload)
  -- payload — JSON со структурой { tab = N, pane = N, uid = N, exit_code = N }
  local ok, data = pcall(function()
    -- любой парсер JSON в Lua; в rterm встроенного нет
  end)
end)
```

Отписка одной строкой убирает ВСЕ хуки, привязанные к этому событию:

```lua
rterm.off("pane.exit")  -- возвращает количество удалённых обработчиков
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

## Действия

```lua
rterm.register_action("name", function() ... end)
rterm.unregister_action("name")
rterm.run_action("name")  -- программно выстрелить действие по имени
```

Зарегистрированное действие появляется и в палитре команд (`Ctrl+Shift+P`), и в `config.toml` под `[[keybindings]] action = "name"`. Удалить регистрацию можно `rterm.unregister_action`.

Полный список встроенных действий — `rterm --list-actions` (или см. [config.md](config.md#keybindings)).

## API: краткий справочник

Все функции — методы глобальной таблицы `rterm`. Имена в snake_case. UID панели — это монотонно возрастающее `u64`, гарантированно уникальное в рамках одной сессии rterm; он остаётся валидным даже после перестановок и закрытия соседних панелей, тогда как `(tab, pane)` индексы сдвигаются. Используйте UID для длительных подписок, индексы — для разовых операций «прямо сейчас».

### Базовое

| Функция | Возвращает | Описание |
|---------|------------|----------|
| `rterm.log(msg)` | — | Записать в трейсинг с таргетом `rterm::plugin`. |
| `rterm.on(event, fn)` | — | Подписать `fn(payload)` на событие. Можно несколько хуков на одно событие. |
| `rterm.off(event)` | `int` | Удалить ВСЕ хуки на это событие, вернуть число удалённых. |
| `rterm.handler_count(event)` | `int` | Сколько обработчиков сейчас подписано на событие. |
| `rterm.handler_counts()` | `{event=N, ...}` | Карта по всем событиям, у которых есть подписчики. |
| `rterm.emit_event(name, payload)` | — | Вручную выстрелить событие. `payload` опциональный. |
| `rterm.builtin_events()` | `{string}` | Канонический список имён событий, которые rterm может выстрелить. |
| `rterm.builtin_actions()` | `{string}` | То же для действий. |
| `rterm.builtin_action_label(name)` / `rterm.builtin_action_labels()` | строка / таблица | Человекочитаемые подписи. |
| `rterm.version()` / `rterm.version_info()` | string / table | Версия + таргет/профиль. |
| `rterm.platform()` | `"linux"` / `"macos"` / `"windows"` | Целевая ОС. |
| `rterm.executable_path()` / `rterm.executable_args()` | string / `{string}` | Что запустилось и с какими CLI-аргументами. |
| `rterm.now_ms()` / `rterm.session_uptime_ms()` | `int` | Текущее время и сколько rterm живёт, миллисекунды. |
| `rterm.config_dir()` / `rterm.config_path()` / `rterm.cache_dir()` | string | Резолвлённые пути. |

### Действия и кейбинды

| Функция | Описание |
|---------|----------|
| `rterm.register_action(name, fn)` | Зарегистрировать кастомное действие. |
| `rterm.unregister_action(name)` | Снять регистрацию. |
| `rterm.run_action(name)` | Программно выстрелить действие (включая встроенные). |
| `rterm.list_actions()` | Все доступные действия (встроенные + кастомные). |

### Вкладки

| Функция | Описание |
|---------|----------|
| `rterm.new_tab()` | Открыть новую вкладку. |
| `rterm.kill_tab(idx)` | Закрыть вкладку по 1-based индексу. |
| `rterm.focus_tab(idx)` | Сделать вкладку активной. |
| `rterm.tab_count()` / `rterm.tabs()` | Количество и список заголовков. |
| `rterm.active_tab()` | 1-based индекс активной вкладки. |
| `rterm.find_tab(predicate)` | Линейный поиск по `predicate(tab_info)`. |
| `rterm.set_tab_title(title)` / `rterm.set_tab_title_by_index(idx, title)` | Пин заголовка. |
| `rterm.dragging_tab()` | Индекс перетаскиваемой вкладки или `nil`. |

### Панели

Большая часть аксессоров есть в трёх формах:

- `rterm.X()` — для фокусной панели;
- `rterm.X_of(tab, pane)` — по 1-based индексам;
- `rterm.X_by_uid(uid)` — по UID.

Так покрыты: `cursor`, `size`, `idle`, `scroll_offset`, `scrollback_len`, `alt_screen`, `reverse_screen`, `cwd`, `title`, `foreground_process`, `foreground_pgid`, `shell_pid`, `last_exit_code`, `bell_muted`, `progress`, `scrollback_text`, `terminal_text`, `copy_pane`, `send_to_pane`.

| Действие | Функция |
|---------|---------|
| Сплит | `rterm.split("h" \| "v" \| "auto")` |
| Закрыть | `rterm.kill_pane()` / `rterm.kill_pane_by_uid(uid)` |
| Фокус | `rterm.focus_pane(idx)` / `rterm.focus_pane_by_uid(uid)` |
| Установить заголовок | `rterm.set_pane_title(s)` / `rterm.set_pane_title_by_uid(uid, s)` |
| Заглушить колокольчик | `rterm.set_pane_bell_muted(true)` / `rterm.set_pane_bell_muted_by_uid(uid, true)` |
| Активная панель | `rterm.active_pane()` (1-based) / `rterm.active_pane_uid()` |
| Список панелей | `rterm.list_panes(tab)` |
| Конвертация | `rterm.uid_of(tab, pane)` / `rterm.indices_of_uid(uid)` |
| Найти | `rterm.find_pane(predicate)` |
| Количество | `rterm.pane_count(tab)` |

### Ввод / вывод PTY

| Функция | Описание |
|---------|----------|
| `rterm.send_input(s)` | Записать в фокусную панель как будто пользователь набрал. |
| `rterm.send_to_pane(tab, pane, s)` / `rterm.send_to_pane_by_uid(uid, s)` | То же для конкретной панели. |
| `rterm.copy_pane()` / `rterm.copy_pane_by_uid(uid)` | Скопировать содержимое панели в clipboard. |
| `rterm.copy(text)` / `rterm.paste()` / `rterm.read_clipboard()` | Системный буфер обмена. |
| `rterm.terminal_text()` / `rterm.scrollback_text(max_lines)` | Видимое содержимое / скроллбэк фокусной панели. |

### Прокрутка и поиск

| Функция | Описание |
|---------|----------|
| `rterm.scroll(delta)` | Прокрутить фокусную панель: `+N` вниз, `-N` вверх. |
| `rterm.scroll_offset()` / `rterm.scroll_to_line(n)` / `rterm.scroll_to_live()` | Управление позицией скроллбэка. |
| `rterm.scrollback_limit()` / `rterm.set_scrollback(n)` | Размер кольцевого буфера. |
| `rterm.start_search(query, regex)` | Открыть оверлей поиска с заданной строкой. |
| `rterm.is_search_active()` / `rterm.search_query()` / `rterm.search_regex_mode()` | Состояние поиска. |
| `rterm.search_matches()` / `rterm.find_match(query, regex)` | Список совпадений / точный поиск. |
| `rterm.command_marks()` / `rterm.prompt_marks()` | OSC 133-отметки для прыжков по командам. |

### Внешний вид

| Функция | Описание |
|---------|----------|
| `rterm.set_font_size(pt)` / `rterm.font_size()` / `rterm.font_family()` | Шрифт. |
| `rterm.cell_width()` / `rterm.line_height()` | Геометрия ячейки в пикселях. |
| `rterm.set_opacity(v)` / `rterm.opacity()` | Прозрачность окна. |
| `rterm.set_palette(idx, [r,g,b])` / `rterm.palette_color(idx)` / `rterm.named_palette()` | Палитра. |
| `rterm.nearest_palette_index([r,g,b])` | Ближайший индекс в текущей палитре. |
| `rterm.set_window_title(s)` | Заголовок окна (перекрывает OSC 0/2). |
| `rterm.set_cursor_blink(bool)` | Глобальное мигание курсора. |
| `rterm.set_show_scrollbar(bool)` | Видимость скроллбара. |
| `rterm.theme()` / `rterm.is_dark()` / `rterm.is_light()` | Цвета темы и хелперы. |
| `rterm.hex_to_rgb("#rrggbb")` / `rterm.rgb_to_hex([r,g,b])` | Конверсия. |
| `rterm.contrast_ratio(c1, c2)` / `rterm.contrast_grade(...)` / `rterm.contrast_fg(bg)` | WCAG-контраст. |
| `rterm.cursor_shape_names()` / `rterm.mouse_mode_names()` | Enum-перечисления. |

### Уведомления и фокус

| Функция | Описание |
|---------|----------|
| `rterm.bell()` | Сэмулировать BEL (фиксируется `bell_visual` / `bell_urgent`). |
| `rterm.notify(title, body)` | Системное уведомление. |
| `rterm.attention()` | Дёрнуть urgency-хинт окна вне зависимости от BEL. |
| `rterm.window_focused()` | `true`, если окно сейчас в фокусе. |
| `rterm.shell()` / `rterm.pid()` / `rterm.shell_pid()` | Сведения о шелле фокусной панели. |
| `rterm.cwd()` / `rterm.title()` | Cwd и заголовок фокусной панели. |

### URL и матчи

| Функция | Описание |
|---------|----------|
| `rterm.add_match(name, pattern, opts)` | Подписать «матч» на содержимое скроллбэка. `opts = { regex = true, on = function(text, row, col) ... end }`. Событие `match` стреляет каждый раз, когда `pattern` находится в новом выводе. |
| `rterm.remove_match(name)` / `rterm.remove_all_matches()` | Снять регистрации. |
| `rterm.list_matches()` / `rterm.match_rules()` | Что зарегистрировано. |
| `rterm.open_url(url)` | Открыть URL через системный обработчик. |

### Снепшот целиком

```lua
local snap = rterm.snapshot()
-- snap.tabs[i] = { title, panes = { { uid, cursor, size, cwd, ... }, ... } }
```

Тяжёлая функция — собирает полное состояние всех вкладок и панелей. Не вызывайте в `frame.tick`. Большинство «лёгких» вопросов можно ответить точечными аксессорами (`rterm.cwd_of(t, p)`, `rterm.size_by_uid(uid)`, ...) дешевле.

## Идиомы и примеры

### Авто-копирование на отпускание мыши

```lua
rterm.on("selection.end", function(text)
  rterm.copy(text)
end)
```

(rterm намеренно не делает этого по умолчанию — см. инвариант в README.)

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

### Восстановление состояния между запусками

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
- **Список встроенных** — `rterm --list-events --json` / `--list-actions --json`. На любом ответе есть `name` и `summary`, удобно генерить документацию плагинов.
- **Не уверены, что хук висит** — проверить через `rterm.handler_counts()` в `init.lua`.

## Что ВНУТРИ Lua недоступно

- Прямого доступа к PTY байтам нет — пишите через `rterm.send_input` / `rterm.send_to_pane`.
- `os.exit()` уронит весь rterm (это Lua-стейт в одном процессе). Используйте `rterm.run_action("quit")` для штатного выхода.
- Долгие синхронные операции в хуках блокируют ввод. Для тяжёлых задач — пишите во внешний скрипт через `send_input` или используйте `coroutine`.
- Сетевой и файловый I/O — стандартный Lua (`io`, `socket` если линкуется отдельно), не через `rterm.*`. Будьте осторожны с правами и таймаутами.

См. также [config.md](config.md) — декларативная часть конфигурации.
