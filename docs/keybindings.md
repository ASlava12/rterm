# Keybindings

Пользовательские биндинги задаются массивом таблиц `[[keybindings]]` в `config.toml` и **перекрывают** встроенные сочетания. Полный синтаксис ключа — ниже; полный список действий — `rterm --list-actions`.

## Пример

```toml
[[keybindings]]
keys   = "Ctrl+T"
action = "new_tab"

[[keybindings]]
keys   = "F2"
action = "search"

[[keybindings]]
keys   = "Ctrl+Shift+Enter"
action = "split_auto"
```

## `keys` format

### Модификаторы

| Модификатор | Алиасы |
|-------------|--------|
| `Ctrl` | `Control` |
| `Shift` | — |
| `Alt` | `Option` |
| `Super` | `Cmd`, `Meta`, `Win` |

Можно комбинировать через `+`: `Ctrl+Shift+T`, `Alt+Super+Enter`.

### Именованные клавиши

| Клавиша | Алиасы |
|---------|--------|
| `Enter` | `Return` |
| `Escape` | `Esc` |
| `Tab` | — |
| `Space` | — |
| `Backspace` | — |
| `Delete` | `Del` |
| `Insert` | `Ins` |
| `Home` | — |
| `End` | — |
| `PageUp` | `PgUp` |
| `PageDown` | `PgDn` |
| `Up` / `Down` / `Left` / `Right` | `ArrowUp` / `ArrowDown` / `ArrowLeft` / `ArrowRight` |
| `F1` … `F12` | — |

### Алиасы пунктуации

Когда буквальный символ ломает парсер (`Ctrl++`, `Ctrl+-`), используй алиас:

| Алиас | Соответствие |
|-------|--------------|
| `Plus` | `+` |
| `Minus` / `Dash` | `-` |
| `Equal` | `=` |
| `Comma` | `,` |
| `Period` / `Dot` | `.` |
| `Slash` | `/` |
| `Backslash` | `\` |
| `Semicolon` | `;` |
| `Colon` | `:` |
| `Apostrophe` / `Quote` | `'` |

## Дефолтные кейбинды

| Сочетание | Действие |
|-----------|----------|
| `Ctrl+Shift+T` / `W` | Новая / закрыть вкладку |
| `Ctrl+Shift+←` / `→` | Переключение вкладок |
| `Ctrl+Shift+Tab` | К предыдущей вкладке |
| `Ctrl+Shift+,` / `.` | Сдвинуть вкладку влево / вправо |
| `Ctrl+Shift+D` / `E` | Горизонтальный (─) / вертикальный (│) сплит |
| `Ctrl+Shift+X` | Закрыть панель |
| `Ctrl+Shift+Z` | Раскрыть/свернуть фокусную панель (tmux zoom) |
| `Ctrl+Shift+{` / `}` | Поменять панель с предыдущей / следующей |
| `Alt+←/↑/→/↓` | Фокус на соседнюю панель пространственно |
| `Alt+1..9` | Фокус на N-ю панель (DFS-порядок) |
| `Alt+Shift+←/↑/→/↓` | Изменить размер фокусной панели |
| `Ctrl+Shift+V` / `Shift+Insert` | Вставка |
| `Ctrl+Shift+C` / `Ctrl+Insert` | Копировать выделение |
| `Ctrl+Shift+Y` | Копировать URL под курсором |
| `Ctrl+Shift+F` | Поиск по скроллбэку |
| `Ctrl+Shift+P` | Открыть палитру команд |
| `Ctrl+Shift+H` | Подсказка по горячим клавишам |
| `Ctrl+Shift+K` | Очистить скроллбэк |
| `Ctrl+Shift+=` / `-` / `0` | Шрифт больше / меньше / сбросить |
| `Shift+PgUp/PgDn` | Скроллбэк постранично |
| `Shift+Home/End` | Скроллбэк в начало / к live-выводу |
| `Ctrl+Alt+↑/↓` | Перейти к предыдущему / следующему промпту (требует OSC 133) |

Полная справка с подписями на русском/английском: `rterm --list-keybindings`.

## Полный список actions

`rterm --list-actions` — авторитетный источник. На момент написания доступны:

### Вкладки

```
new_tab           close_tab          next_tab           prev_tab
goto_first_tab    goto_last_tab      toggle_last_tab
move_tab_left     move_tab_right
rename_tab
```

### Панели

```
split_horizontal  split_vertical     split_auto         close_pane
focus_next_pane   focus_prev_pane    focus_first_pane   focus_last_pane
swap_pane_next    swap_pane_prev
resize_pane_left  resize_pane_right  resize_pane_up     resize_pane_down
zoom_pane         balance_panes      reset_pane         toggle_bell_mute
```

### Буфер обмена / выделение

```
paste             copy               clear_selection
copy_hovered_url  open_hovered_url
```

### Прокрутка / скроллбэк

```
scroll_page_up    scroll_page_down
scroll_half_page_up   scroll_half_page_down
scroll_line_up    scroll_line_down
scroll_home       scroll_end
clear_scrollback  save_scrollback
```

### Shell-integration

```
jump_prev_prompt  jump_next_prompt
jump_prev_command jump_next_command
```

### Внешний вид и окно

```
font_increase     font_decrease      font_reset
opacity_increase  opacity_decrease   opacity_reset
cycle_theme       cycle_theme_prev
open_settings
snap_window_left  snap_window_right
snap_window_top   snap_window_bottom
maximize_toggle   minimize_window    restore_window
toggle_guake
```

`toggle_guake` — Guake-style выкатывание окна (см. [config.md `[guake]`](config.md#guake)). No-op пока в конфиге не выставлено `[guake] enabled = true`.

### Прочее

```
search            toggle_help        open_palette       quit
```

Кастомные действия, зарегистрированные через `rterm.register_action(name, fn)` в Lua, также валидны в `action`. См. [Plugins guide](plugins.md#actions).
