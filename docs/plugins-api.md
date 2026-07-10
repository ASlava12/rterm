# Plugins API reference

Полный справочник по `rterm.*` Lua API. Обзорное чтение и идиомы — [Plugins guide](plugins.md).

Все функции — методы глобальной таблицы `rterm`. Имена в snake_case.

## UID vs `(tab, pane)` индексы

**UID панели** — монотонно возрастающее `u64`, гарантированно уникальное в рамках одной сессии rterm. Остаётся валидным даже после перестановок и закрытия соседних панелей.

**`(tab, pane)` индексы** — 1-based позиции в текущей раскладке. Сдвигаются при reorder, split, close.

Эмпирика:

- Используй **UID** для длительных подписок («пинай меня, когда панель сборки закончит»).
- Используй **`(tab, pane)`** для разовых операций «прямо сейчас».

Конвертация: `rterm.uid_of(tab, pane)`, `rterm.indices_of_uid(uid)`.

## Базовое

| Функция | Возвращает | Описание |
|---------|------------|----------|
| `rterm.log(msg)` | — | Записать в трейсинг с таргетом `rterm::plugin`. |
| `rterm.on(event, fn)` | — | Подписать `fn(payload)` на событие. Можно несколько хуков на одно событие. |
| `rterm.off(event)` | `int` | Удалить ВСЕ хуки на это событие, вернуть число удалённых. |
| `rterm.handler_count(event)` | `int` | Сколько обработчиков сейчас подписано на событие. |
| `rterm.handler_counts()` | `{event=N, …}` | Карта по всем событиям, у которых есть подписчики. |
| `rterm.emit_event(name, payload)` | — | Вручную выстрелить событие. `payload` опциональный. |
| `rterm.builtin_events()` | `{string}` | Канонический список имён событий, которые rterm может выстрелить. |
| `rterm.builtin_actions()` | `{string}` | То же для действий. |
| `rterm.builtin_action_label(name)` / `rterm.builtin_action_labels()` | строка / таблица | Человекочитаемые подписи. |
| `rterm.version()` / `rterm.version_info()` | string / table | Версия + таргет/профиль. |
| `rterm.platform()` | `"linux"` / `"macos"` / `"windows"` / … | Целевая ОС. |
| `rterm.executable_path()` / `rterm.executable_args()` | string / `{string}` | Что запустилось и с какими CLI-аргументами. |
| `rterm.now_ms()` / `rterm.session_uptime_ms()` | `int` | Текущее время и сколько rterm живёт, миллисекунды. |
| `rterm.config_dir()` / `rterm.config_path()` / `rterm.cache_dir()` | string | Резолвлённые пути. |

## Actions и кейбинды

| Функция | Описание |
|---------|----------|
| `rterm.register_action(name, fn)` | Зарегистрировать кастомное действие. |
| `rterm.unregister_action(name)` | Снять регистрацию. |
| `rterm.run_action(name)` | Программно выстрелить действие (включая встроенные). |
| `rterm.list_actions()` | Все доступные действия (встроенные + кастомные). |

## Вкладки

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

## Панели

Большая часть аксессоров есть в трёх формах:

- `rterm.X()` — для фокусной панели.
- `rterm.X_of(tab, pane)` — по 1-based индексам.
- `rterm.X_by_uid(uid)` — по UID.

Так покрыты: `cursor`, `size`, `idle`, `scroll_offset`, `scrollback_len`, `alt_screen`, `reverse_screen`, `cwd`, `title`, `foreground_process`, `foreground_pgid`, `shell_pid`, `last_exit_code`, `bell_muted`, `progress`, `scrollback_text`, `terminal_text`, `copy_pane`, `send_to_pane`.

| Действие | Функция |
|----------|---------|
| Сплит | `rterm.split("h" \| "v" \| "auto")` |
| Закрыть | `rterm.kill_pane()` / `rterm.kill_pane_by_uid(uid)` |
| Фокус | `rterm.focus_pane(idx)` / `rterm.focus_pane_by_uid(uid)` |
| Установить заголовок | `rterm.set_pane_title(s)` / `rterm.set_pane_title_by_uid(uid, s)` |
| Заглушить колокольчик | `rterm.set_pane_bell_muted(true)` / `rterm.set_pane_bell_muted_by_uid(uid, true)` |
| Активная панель | `rterm.active_pane()` (1-based) / `rterm.active_pane_uid()` |
| Список панелей | `rterm.list_panes(tab)` |
| Конвертация UID ↔ indices | `rterm.uid_of(tab, pane)` / `rterm.indices_of_uid(uid)` |
| Найти | `rterm.find_pane(predicate)` |
| Количество | `rterm.pane_count(tab)` |

## Ввод / вывод PTY

| Функция | Описание |
|---------|----------|
| `rterm.send_input(s)` | Записать в фокусную панель как будто пользователь набрал. |
| `rterm.send_to_pane(tab, pane, s)` / `rterm.send_to_pane_by_uid(uid, s)` | То же для конкретной панели. |
| `rterm.copy_pane()` / `rterm.copy_pane_by_uid(uid)` | Скопировать содержимое панели в clipboard. |
| `rterm.copy(text)` / `rterm.paste()` / `rterm.read_clipboard()` | Системный буфер обмена. |
| `rterm.terminal_text()` / `rterm.scrollback_text(max_lines)` | Видимое содержимое / скроллбэк фокусной панели. |

## Прокрутка и поиск

| Функция | Описание |
|---------|----------|
| `rterm.scroll(delta)` | Прокрутить фокусную панель: `+N` вниз, `-N` вверх. |
| `rterm.scroll_offset()` / `rterm.scroll_to_line(n)` / `rterm.scroll_to_live()` | Управление позицией скроллбэка. |
| `rterm.scrollback_limit()` / `rterm.set_scrollback(n)` | Размер кольцевого буфера. |
| `rterm.start_search(query, regex)` | Открыть оверлей поиска с заданной строкой. |
| `rterm.is_search_active()` / `rterm.search_query()` / `rterm.search_regex_mode()` | Состояние поиска. |
| `rterm.search_matches()` / `rterm.find_match(query, regex)` | Список совпадений / точный поиск. |
| `rterm.command_marks()` / `rterm.prompt_marks()` | OSC 133-отметки для прыжков по командам. |

## Внешний вид

| Функция | Описание |
|---------|----------|
| `rterm.set_font_size(pt)` / `rterm.font_size()` / `rterm.font_family()` | Шрифт. |
| `rterm.cell_width()` / `rterm.line_height()` | Геометрия ячейки в пикселях. |
| `rterm.set_opacity(v)` / `rterm.opacity()` | Прозрачность окна (0.0..=1.0). |
| `rterm.set_palette(table)` | Полный swap палитры — см. [Themes](themes.md#произвольные-палитры-через-rtermsetpalette). |
| `rterm.palette_color(idx)` / `rterm.named_palette()` | Чтение палитры. |
| `rterm.nearest_palette_index([r,g,b])` | Ближайший индекс в текущей палитре. |
| `rterm.themes()` / `rterm.current_theme()` / `rterm.set_theme(name)` | Встроенные темы. |
| `rterm.set_window_title(s)` | Заголовок окна (перекрывает OSC 0/2). |
| `rterm.set_cursor_blink(bool)` | Глобальное мигание курсора. |
| `rterm.set_show_scrollbar(bool)` | Видимость скроллбара. |
| `rterm.theme()` / `rterm.is_dark()` / `rterm.is_light()` | Цвета темы и хелперы. |
| `rterm.hex_to_rgb("#rrggbb")` / `rterm.rgb_to_hex([r,g,b])` | Конверсия цветов. |
| `rterm.contrast_ratio(c1, c2)` / `rterm.contrast_grade(...)` / `rterm.contrast_fg(bg)` | WCAG-контраст. |
| `rterm.cursor_shape_names()` / `rterm.mouse_mode_names()` | Enum-перечисления. |

## Уведомления и фокус

| Функция | Описание |
|---------|----------|
| `rterm.bell()` | Сэмулировать BEL (фиксируется `bell_visual` / `bell_urgent`). |
| `rterm.notify(title, body)` | Системное уведомление. |
| `rterm.attention()` | Дёрнуть urgency-хинт окна вне зависимости от BEL. |
| `rterm.window_focused()` | `true`, если окно сейчас в фокусе. |
| `rterm.shell()` / `rterm.pid()` / `rterm.shell_pid()` | Сведения о шелле фокусной панели. |
| `rterm.cwd()` / `rterm.title()` | Cwd и заголовок фокусной панели. |

## URL и матчи

| Функция | Описание |
|---------|----------|
| `rterm.add_match(name, pattern, opts)` | Подписать «матч» на содержимое скроллбэка. `opts = { regex = true, on = function(text) ... end }`, где `text` — совпавшая строка вывода. Событие `match` стреляет каждый раз, когда `pattern` находится в новом выводе; per-rule `on`-колбэк (если задан) вызывается тогда же. |
| `rterm.remove_match(name)` / `rterm.remove_all_matches()` | Снять регистрации. |
| `rterm.list_matches()` / `rterm.match_rules()` | Что зарегистрировано. |
| `rterm.open_url(url)` | Открыть URL через системный обработчик. |

## Снепшот целиком

```lua
local snap = rterm.snapshot()
-- snap.tabs[i] = { title, panes = { { uid, cursor, size, cwd, ... }, ... } }
```

**Тяжёлая** функция — собирает полное состояние всех вкладок и панелей. Не вызывай в `frame.tick`. Большинство «лёгких» вопросов отвечается точечными аксессорами (`rterm.cwd_of(t, p)`, `rterm.size_by_uid(uid)`, …) дешевле.

## Setters для `[terminal]` опций

Пишут в runtime-конфиг; изменения сохраняются на время сессии (не пишутся в `config.toml`):

| Функция | Эффект |
|---------|--------|
| `rterm.set_scrollback(n)` | Размер кольцевого буфера. |
| `rterm.set_tab_silence_ms(ms)` | Порог `tab.silence`. |
| `rterm.set_slow_command_ms(ms)` | Порог `pane.slow_command`. |
| `rterm.set_scroll_on_output(bool)` | Snap к live на новом выводе. |
| `rterm.set_bell_visual(bool)` / `rterm.set_bell_urgent(bool)` | BEL-каналы. |
