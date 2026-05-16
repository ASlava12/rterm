# UI tour

Обзор интерактивных элементов окна: вкладки, меню, кнопки управления окном, snap. Если ищешь, как сменить сочетания клавиш — см. [Keybindings](keybindings.md).

## Header bar

Сверху единая полоса с (слева направо):

- **`☰`** — hamburger button, открывает app-menu.
- **Вкладки** — каждая отдельный «чип» в стиле Chrome/Firefox с фоном; активная подсвечивается ярче и подчёркнута 2px акцентной полосой сверху.
- **`+`** — кнопка новой вкладки (если хватает места).
- **`─ ▢ ✕`** — управление окном: minimize / maximize-toggle / close. Видны только при `[window].os_decorations = false`.

## Вкладки

- **Левый клик** по телу — переключиться.
- **Левый клик** по `×` справа — закрыть.
- **Двойной клик** или `Rename tab…` в контекстном меню — переименовать. Откроется поле ввода; `Enter` — применить, `Esc` — отменить, пустой ввод — сбросить кастомное имя.
- **Средний клик** — закрыть.
- **Перетаскивание** — изменить порядок. «Phantom» чип следует за курсором, соседние плавно сдвигаются (`140ms` ease-out).
- **Правый клик** — контекстное меню (Rename / Close / Move left/right / Zoom / New tab).
- **Колесо** над tab-bar — переключение между вкладками.

## Меню

### App menu (`☰` слева)

Все основные действия: New tab / Split / Palette / Search / Save scrollback / Clear scrollback / Cycle theme / Settings / Help / Snap window / Toggle maximize / Minimize / Restore / Quit.

### Контекстное меню (правый клик)

| Где кликнули | Меню |
|--------------|------|
| В панели | Copy / Paste / Clear selection / New tab / Split horizontal / Split vertical / Close pane / Search / Reset / Settings / Help |
| На вкладке | Rename tab / Close tab / Move tab left/right / Zoom pane / New tab |
| В пустой части хедера | New tab / Palette / Cycle theme / Settings / Help / Quit |

В любом меню работает: `↑/↓` — навигация, `Enter` — выбор, `Esc` — закрыть, hover мышью подсвечивает строку.

## Управление окном

rterm работает **без системных декораций** по умолчанию (`[window].os_decorations = false`). Управление окном целиком на стороне приложения:

- **Drag окна** — клик и драг в пустой части header.
- **Двойной клик** в пустую часть header — toggle maximize / restore.
- **Кнопки** в правом верхнем углу: `─` minimize, `▢` maximize/restore, `✕` close.
- **Resize**: подведи курсор к краю (6 px) или углу (12 px) — курсор меняется на соответствующий resize-arrow, появляется подсветка ребра/угла, drag меняет размер.

Если родная decoration нужна (например, тайловый WM управляет компоновкой сам):

```toml
[window]
os_decorations = true
```

## Window snap

Снап-действия доступны в app-menu и через палитру. Работают одинаково на всех платформах через `Monitor::size()` + `Window::set_outer_position` / `Window::request_inner_size`:

| Действие | Что делает |
|----------|------------|
| `snap_window_left` | Левая половина текущего монитора |
| `snap_window_right` | Правая половина |
| `snap_window_top` | Верхняя половина (на Wayland → maximize, см. ниже) |
| `snap_window_bottom` | Нижняя половина |
| `maximize_toggle` | Включить/выключить maximize |
| `minimize_window` | Свернуть |
| `restore_window` | Стартовый размер, центрировать |

### Платформенные нюансы

- **X11, Windows, macOS, FreeBSD-X11** — `set_outer_position` + `request_inner_size` работают; snap позиционирует окно точно.
- **Wayland** — апп не имеет права позиционировать своё окно (design Wayland). Snap делает только `request_inner_size`, а `Top` падает обратно к `set_maximized(true)`. В большинстве Wayland-композиторов (GNOME, KDE, Sway) есть встроенный edge-snap при drag — потяни окно за header в край экрана, и композитор сам разложит. Наши actions дополняют, а не заменяют это.

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

## Guake-style drop-down

Опциональный режим: окно по нажатию бинда «выкатывается» поверх остальных, занимая часть экрана. Конфигурируется в `[guake]`-секции (`enabled`, `position`, `height_pct`, `width_pct`). Подробности и пример для `F11` — в [config.md `[guake]`](config.md#guake).

## Bottom bar

Узкая полоса внизу окна, видимая только в двух случаях:

- **Активен поиск** — `/{query}` слева, счётчик матчей `(N/M)` посередине, подсказка `[Esc] [Enter:next] [Ctrl+R:regex] [Ctrl+W:word] [Ctrl+U:clear]` справа.
- **Скроллбэк не в live-позиции** — `↑ off / total`, плюс подсказка `Shift+PgUp/PgDn · Shift+Home top · Shift+End live`.

Поиск **резервирует** место (панели сжимаются), скроллбэк-индикатор **плавает** поверх содержимого (панели не пересчитываются).

## Bell

При получении BEL (`\a`) или вызове `rterm.bell()`:

- `[terminal].bell_visual = true` — вспышка экрана.
- `[terminal].bell_urgent = true` — пинг таскбару / dock'у, когда окно не в фокусе.

Плагины могут заглушить bell для конкретной панели через `rterm.set_pane_bell_muted(true)` или действие `toggle_bell_mute`.

## Выделение, копирование, ссылки

- **Drag** — выделение по символам.
- **Двойной клик** — выделение слова.
- **Тройной клик** — выделение строки.
- **Shift+клик** — расширить выделение.
- **`Ctrl+Shift+C`** или `Ctrl+Insert` — копировать (auto-copy на выделении **выключен** намеренно).
- **`Ctrl+Shift+V`** или `Shift+Insert` или средний клик — вставка.
- **Ctrl+клик / Cmd+клик по URL** — открыть в браузере. URL подсвечиваются при наведении.
- **`Ctrl+Shift+Y`** — скопировать hovered URL.
