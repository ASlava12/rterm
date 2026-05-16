# Конфигурация rterm

rterm читает конфигурацию из двух мест:

- **`config.toml`** — декларативные настройки (шрифт, окно, шелл, палитра, кейбинды).
- **`init.lua` + `plugins/*.lua`** — императивная логика (хуки событий, кастомные действия, реакции).

Оба файла подхватываются «горячо» — изменения применяются на сохранение без перезапуска rterm. Плагины при перезагрузке получают событие `reload`.

## Где лежат файлы

| Платформа | Путь |
|-----------|------|
| Linux (XDG) | `~/.config/rterm/` |
| macOS | `~/Library/Application Support/rterm/` (или `~/.config/rterm/`, если стоит `XDG_CONFIG_HOME`) |
| Windows | `%APPDATA%\rterm\` |

Полные пути, которые rterm видит сейчас, печатает команда:

```bash
rterm --print-paths --json
```

Переопределить путь к `config.toml` для одного запуска: `--config <path>` или переменная окружения `RTERM_CONFIG_PATH`.

## Быстрый старт

```bash
rterm --print-default-config > ~/.config/rterm/config.toml
```

— положит закомментированный шаблон со всеми полями. Раскомментируйте нужные строки, остальное rterm возьмёт из встроенных дефолтов.

Проверить, что конфиг + Lua валидны, не запуская GUI:

```bash
rterm --check
```

Вывести то, что rterm реально соберёт после слияния с дефолтами и CLI-флагами:

```bash
rterm --print-config
```

## Полная схема `config.toml`

### `[font]`

| Ключ | Тип | По умолчанию | Описание |
|------|-----|--------------|----------|
| `family` | string | `""` (системный моноширинный) | Имя установленного шрифта: `"JetBrains Mono"`, `"Fira Code"`, `"MesloLGS NF"`. |
| `size` | float | `13.0` | Размер в пунктах. CLI-флаг `--font-size N` перекрывает. |
| `bold_is_bright` | bool | `true` | Поведение xterm: жирный текст в одном из 8 базовых цветов автоматически переходит в bright-вариант. Поставьте `false` для тем, где жирный используется только как выделение без смены цвета. |

Список установленных моноширинных семейств: `rterm --list-fonts`.

### `[window]`

| Ключ | Тип | По умолчанию | Описание |
|------|-----|--------------|----------|
| `width` | int | `1024` | Стартовая ширина окна, пиксели. |
| `height` | int | `640` | Стартовая высота окна, пиксели. |
| `opacity` | float | `1.0` | От `0.0` (прозрачно) до `1.0` (непрозрачно). Требует поддержки прозрачности у композитора — Wayland / macOS / Windows с включённым DWM. |

### `[shell]`

| Ключ | Тип | По умолчанию | Описание |
|------|-----|--------------|----------|
| `program` | string | `""` (наследует `$SHELL` на Unix, `powershell.exe` на Windows) | Абсолютный путь до бинарника шелла. |
| `args` | `[string]` | `[]` | Аргументы, передаваемые шеллу. |
| `env` | `{string: string}` | `{}` | Дополнительные переменные окружения. Применяются ПОСЛЕ встроенных `TERM` / `COLORTERM` / `TERM_PROGRAM`, поэтому пользовательский ключ перекрывает дефолт. Родительские env-переменные при этом не сбрасываются — это аддитивная карта. |

Пример:

```toml
[shell]
program = "/bin/zsh"
args    = ["-l"]

[shell.env]
LANG          = "en_US.UTF-8"
RUST_BACKTRACE = "1"
```

### `[terminal]`

| Ключ | Тип | По умолчанию | Описание |
|------|-----|--------------|----------|
| `scrollback` | int | `10000` | Сколько строк хранить в скроллбэке каждой панели. `0` — отключить скроллбэк целиком. Память ≈ `lines × cols × 16` байт. |
| `save_scrollback_on_exit` | bool | `false` | На выходе скидывать скроллбэк фокусной панели в `$XDG_CACHE_HOME/rterm/scrollback-<ts>.txt`. Эквивалент ручного действия `save_scrollback`. |
| `restore_session` | bool | `false` | На выходе сохранять список вкладок (по одной cwd на вкладку) в `$XDG_CACHE_HOME/rterm/session.toml` и восстанавливать его на следующем старте. Сплиты не сохраняются — каждая вкладка возрождается как одна панель. |
| `scroll_on_output` | bool | `false` | При любом новом выводе шелл-а сбрасывать `scroll_offset = 0` (поведение «tail»). По умолчанию rterm НЕ дёргает скролл, пока пользователь читает старую строку. |
| `cursor_blink` | bool | `true` | Главный переключатель мигания курсора. `false` отрисовывает курсор без мигания вне зависимости от того, что просит шелл через DECSCUSR. |
| `show_scrollbar` | bool | `true` | Тонкий индикатор скролла справа. `false` прячет его полностью. |
| `tab_silence_ms` | int | `5000` | Сколько мс должно пройти после последнего вывода, чтобы для вкладки сработало edge-событие `tab.silence`. `0` отключает событие. |
| `bell_visual` | bool | `true` | Визуальная вспышка экрана на BEL (`\a`) или `rterm.bell()` из плагина. |
| `bell_urgent` | bool | `true` | Дёргать оконный менеджер (urgency-хинт / dock-badge) на BEL, когда окно не в фокусе. |
| `slow_command_ms` | int | `10000` | Порог для события `pane.slow_command`. Если команда, помеченная OSC 133;C / 133;D (то есть видна shell-integration), выполняется дольше — срабатывает edge-событие и, если окно не в фокусе, пингуется таскбар. `0` отключает. |

### `[colors]`

Все поля опциональные. Каждое — RGB-тройка байт `[r, g, b]`. Отсутствующий ключ означает «оставить встроенный xterm-дефолт».

| Ключ | Назначение |
|------|------------|
| `fg`, `bg` | Цвет текста и фона по умолчанию. |
| `cursor` | Фиксированный цвет блока курсора. Если не задан, курсор использует инвертированный фон ячейки (xterm-стиль). |
| `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white` | 8 базовых ANSI-цветов. |
| `bright_black`, `bright_red`, ..., `bright_white` | 8 «ярких» цветов (индексы 8..15). |

Пример «Solarized Dark»:

```toml
[colors]
fg      = [131, 148, 150]
bg      = [0,   43,  54]
cursor  = [220, 50,  47]

black   = [7,   54,  66]
red     = [220, 50,  47]
green   = [133, 153, 0]
yellow  = [181, 137, 0]
blue    = [38,  139, 210]
magenta = [211, 54,  130]
cyan    = [42,  161, 152]
white   = [238, 232, 213]

bright_black   = [0,   43,  54]
bright_red     = [203, 75,  22]
bright_green   = [88,  110, 117]
bright_yellow  = [101, 123, 131]
bright_blue    = [131, 148, 150]
bright_magenta = [108, 113, 196]
bright_cyan    = [147, 161, 161]
bright_white   = [253, 246, 227]
```

### `[[keybindings]]`

Массив таблиц. Каждая запись — пара `keys` + `action`. Пользовательские бинды перекрывают встроенные.

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

**Распознаваемые модификаторы**: `Ctrl` (или `Control`), `Shift`, `Alt` (или `Option`), `Super` (или `Cmd`, `Meta`, `Win`).

**Именованные клавиши**: `Enter`/`Return`, `Escape`/`Esc`, `Tab`, `Space`, `Backspace`, `Delete`/`Del`, `Insert`/`Ins`, `Home`, `End`, `PageUp`/`PgUp`, `PageDown`/`PgDn`, `Up`/`Down`/`Left`/`Right` (с префиксом `Arrow` тоже работает), `F1`..`F12`.

**Алиасы пунктуации** (если буквальный символ ломает парсер `Ctrl++` / `Ctrl+-`):

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

**Полный живой список доступных действий** — `rterm --list-actions` (или `--list-actions --labels` для русско/английских подписей в палитре команд). На момент написания доступны:

```
balance_panes        clear_scrollback     clear_selection    close_pane
close_tab            copy                 copy_hovered_url   focus_first_pane
focus_last_pane      focus_next_pane      focus_prev_pane    font_decrease
font_increase        font_reset           goto_first_tab     goto_last_tab
jump_next_command    jump_next_prompt     jump_prev_command  jump_prev_prompt
move_tab_left        move_tab_right       new_tab            next_tab
opacity_decrease     opacity_increase     opacity_reset      open_hovered_url
open_palette         paste                prev_tab           quit
reset_pane           resize_pane_down     resize_pane_left   resize_pane_right
resize_pane_up       save_scrollback      scroll_end         scroll_half_page_down
scroll_half_page_up  scroll_home          scroll_line_down   scroll_line_up
scroll_page_down     scroll_page_up       search             split_auto
split_horizontal     split_vertical       swap_pane_next     swap_pane_prev
toggle_bell_mute     toggle_help          toggle_last_tab    zoom_pane
```

Кастомные действия, зарегистрированные через `rterm.register_action(...)` в Lua, тоже валидны в `action`.

## Дефолтные кейбинды

| Сочетание | Действие |
|-----------|----------|
| `Ctrl+Shift+T` / `W` | Новая / закрыть вкладку |
| `Ctrl+Shift+←` / `→` | Переключение вкладок |
| `Ctrl+Shift+Tab` | К предыдущей вкладке |
| `Ctrl+Shift+,` / `.` | Сдвинуть вкладку влево / вправо |
| `Ctrl+Shift+D` / `E` | Горизонтальный / вертикальный сплит |
| `Ctrl+Shift+X` | Закрыть панель |
| `Ctrl+Shift+Z` | Раскрыть/свернуть фокусную панель (как `tmux zoom`) |
| `Ctrl+Shift+{` / `}` | Поменять панель с предыдущей / следующей |
| `Alt+←/↑/→/↓` | Фокус на соседнюю панель пространственно |
| `Alt+1..9` | Фокус на N-ю панель (DFS-порядок) |
| `Alt+Shift+←/↑/→/↓` | Изменить размер фокусной панели |
| `Ctrl+Shift+V` / `Insert` | Вставка |
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

### Темы и окно настроек

В rterm встроены готовые темы. Переключаются через палитру команд (`Ctrl+Shift+P` → `Theme: cycle …`) или живое окно настроек (`open_settings`).

| Действие | Что делает |
|----------|------------|
| `cycle_theme` | Следующая встроенная тема (default → dracula → solarized-dark → solarized-light → nord → gruvbox-dark → light → default …). |
| `cycle_theme_prev` | В обратную сторону. |
| `open_settings` | Открыть «терминатор-стиль» окно настроек поверх терминала. |

В окне настроек (`open_settings`):

| Клавиша | Эффект |
|---------|--------|
| `T` / `Shift+T` | Следующая / предыдущая тема |
| `F` / `Shift+F` | Размер шрифта `+` / `−` |
| `0` | Сбросить шрифт |
| `O` / `Shift+O` | Прозрачность `+` / `−` |
| `9` | Сбросить прозрачность |
| `B` | Включить/выключить мигание курсора |
| `S` | Показать/спрятать скроллбар |
| `?` | Переключиться в help-оверлей |
| `Esc` | Закрыть |

Чтобы повесить открытие настроек на клавишу:

```toml
[[keybindings]]
keys   = "Ctrl+Alt+,"
action = "open_settings"
```

Встроенные темы:
- `default` — стандартный тёмный
- `dark` — алиас на default
- `dracula` — фиолетово-розовая
- `solarized-dark` — синяя на тёмно-зелёном
- `solarized-light` — светлый Solarized
- `nord` — холодный синий
- `gruvbox-dark` — ретро тёплая
- `light` — общая светлая

Для произвольных RGB-цветов используйте секцию `[colors]` выше — она применяется поверх любой выбранной встроенной темы.

### Вкладки

Хедер выполнен в стиле Chrome/Firefox: каждая вкладка — отдельный «чип» с фоном, активная подсвечивается ярче и подчёркнута акцентной полосой. Высота хедера ≈ 1.6× от высоты строки, текст центрирован по вертикали.

**Действия с вкладками:**

- Левый клик по телу вкладки — переключиться.
- Левый клик по `×` справа — закрыть.
- **Двойной левый клик** или **`Rename tab…` в контекстном меню** — переименовать. Откроется модальное поле ввода: введи новое имя и нажми `Enter`, `Esc` отменяет, пустой ввод сбрасывает кастомное имя.
- Действие `rename_tab` (повесь на свою клавишу или вызови из палитры/Lua).
- Средний клик по вкладке — закрыть.
- Перетаскивание — изменить порядок.
- Правый клик — контекстное меню (Rename / Close / Move / Zoom / New).

### Управление окном

rterm работает **без системных декораций** по умолчанию (`[window].os_decorations = false`). Управление окном целиком на стороне приложения:

- **Title bar (header)** — клик и drag двигает окно (xdg_toplevel::move / X11 _NET_WM_MOVERESIZE).
- **Двойной клик в пустую часть header** — toggle maximize / restore.
- **Кнопки в правом верхнем углу**: `─` minimize, `▢` maximize/restore, `✕` close.
- **Resize**: подведи курсор к краю (6 px) или углу (12 px) окна — курсор меняется на соответствующий resize-arrow, появляется подсветка ребра/угла, drag меняет размер.
- **`+` за последней вкладкой** — новая вкладка (Chrome/Firefox).
- **`☰` слева** — app menu со всеми основными действиями.

#### Window snap (одинаково на Wayland, X11, Windows, macOS, FreeBSD)

Снап-действия доступны в app-menu и через палитру:

| Действие | Что делает |
|----------|------------|
| `snap_window_left` | Левая половина текущего монитора |
| `snap_window_right` | Правая половина |
| `snap_window_top` | Верхняя половина (на Wayland → maximize) |
| `snap_window_bottom` | Нижняя половина |
| `maximize_toggle` | Включить/выключить maximize |
| `minimize_window` | Свернуть |
| `restore_window` | Вернуть стартовый размер, центрировать |

**Платформенные нюансы**:
- **X11, Windows, macOS, FreeBSD-X11** — `set_outer_position` + `request_inner_size` работают, snap позиционирует окно точно.
- **Wayland** — апп не имеет права позиционировать своё окно (это design wayland). На Wayland snap делает только `request_inner_size` (компонент компоновки), а `Top` падает обратно к `set_maximized(true)`. В большинстве Wayland-композиторов (GNOME, KDE, Sway) есть **встроенный edge-snap при drag** — потяни окно за header в край экрана, и композитор сам разложит. У нас просто добавлены явные actions поверх этого.

Чтобы повесить snap на горячую клавишу:

```toml
[[keybindings]]
keys   = "Ctrl+Alt+Left"
action = "snap_window_left"

[[keybindings]]
keys   = "Ctrl+Alt+Right"
action = "snap_window_right"

[[keybindings]]
keys   = "Ctrl+Alt+Up"
action = "maximize_toggle"
```

Если родная decoration нужна (например, тайловый WM управляет компоновкой сам):

```toml
[window]
os_decorations = true
```

### Меню и контекстные меню

- **Кнопка `≡`** в самом левом углу хедера — открывает «app menu» со всеми основными действиями (новая вкладка, сплиты, поиск, тема, настройки, выход).
- **Правая кнопка мыши**:
  - В панели → меню: Copy / Paste / Clear selection / New tab / Split / Close pane / Search / Reset / Settings / Help.
  - На вкладке → меню: Close tab / Move left / Move right / Zoom / New tab.
  - В пустой части хедера → меню: New tab / Palette / Cycle theme / Settings / Help / Quit.
- В меню работают `↑/↓ Enter` для навигации с клавиатуры и `Esc` для закрытия.

### Сохранение темы между запусками

Секция `[appearance]` в `config.toml`:

```toml
[appearance]
# Имя встроенной темы — применяется при старте.
# default, dark, dracula, solarized-dark, solarized-light, nord,
# gruvbox-dark, light.
theme = "dracula"
```

Каждый раз, когда пользователь переключает тему (через `cycle_theme`, окно настроек, контекстное меню или `rterm.set_theme()` из Lua), это поле перезаписывается, и при следующем запуске тема восстановится автоматически. `[colors]` всё ещё работает поверх: явные RGB-значения накладываются на выбранную тему.

### Lua API для тем

```lua
local list = rterm.themes()              -- список встроенных тем
local now  = rterm.current_theme()       -- имя текущей темы
local ok   = rterm.set_theme("dracula")  -- применить тему, true/false
```

`set_theme` принимает имя без учёта регистра. Возвращает `false`, если имя неизвестно (применять ничего не будет). После успешного вызова всем плагинам приходит событие `theme` с новым именем, и тема записывается в `config.toml`.

## Переменные окружения

| Переменная | Назначение |
|------------|------------|
| `RTERM_CONFIG_PATH` | Альтернативный путь до `config.toml` (используется всеми CLI-флагами, которые его читают). |
| `RTERM_SMOKE_COMMAND` | В режиме `--smoke` заменяет встроенную команду `echo hello rterm`. |
| `RUST_LOG` | Фильтр трейсинга, например `RUST_LOG=rterm=info,wgpu_hal=warn`. |
| `WGPU_BACKEND` | `vulkan` / `gl` / `metal` / `dx12` / `primary` / `secondary`. На WSL2 авто-дефолт = `gl`, потому что Mesa Vulkan виснет в инициализации инстанса. |
| `WGPU_PRESENT_MODE` | `fifo` / `mailbox` / `immediate` / `autovsync` / `autonovsync`. На WSL2 авто-дефолт = `fifo`: llvmpipe может заклиниться на `AutoVsync`. |
| `WGPU_DEBUG` | `1` или `true` — включить validation-слои wgpu и debug-калбэки (по умолчанию выключено, иначе на старте сотни строк в stderr). |
| `WAYLAND_DISPLAY` | Если не задан → winit падает на X11. |
| `SHELL` | Резервный шелл, когда `[shell] program = ""`. |

## Горячая перезагрузка

rterm раз в секунду опрашивает mtime у `config.toml`, `init.lua` и всех файлов в `plugins/*.lua`. При изменении:

- `config.toml` — перечитывается, применяются новые цвета / шрифт / opacity / клавиши.
- `*.lua` — Lua-стейт пересоздаётся, прежние плагины отключаются, новые загружаются заново; всем регистрированным хукам приходит событие `reload`.

См. также [plugins.md](plugins.md) про события и API плагинов.
