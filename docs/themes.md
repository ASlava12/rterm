# Themes & Appearance

В rterm встроены готовые темы. Переключаются через палитру команд (`Ctrl+Shift+P` → `Theme: cycle …`), окно настроек (`open_settings`), или Lua API.

## Встроенные темы

| Имя | Стиль |
|-----|-------|
| `default` | Стандартный тёмный (xterm-ish) |
| `dark` | Алиас на `default` |
| `dracula` | Фиолетово-розовая (Dracula scheme) |
| `solarized-dark` | Тёмная Solarized |
| `solarized-light` | Светлая Solarized |
| `nord` | Холодный синий Nord |
| `gruvbox-dark` | Ретро тёплая Gruvbox dark |
| `light` | Общая светлая |

## Действия

| Действие | Эффект |
|----------|--------|
| `cycle_theme` | Следующая тема (`default → dracula → solarized-dark → solarized-light → nord → gruvbox-dark → light → default …`) |
| `cycle_theme_prev` | В обратную сторону |
| `open_settings` | Открыть окно настроек поверх терминала |

## Окно настроек

В `open_settings` (горячие клавиши действуют пока окно открыто):

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

Чтобы повесить открытие настроек на свою клавишу:

```toml
[[keybindings]]
keys   = "Ctrl+Alt+,"
action = "open_settings"
```

## Сохранение темы между запусками

Секция `[appearance]` в `config.toml`:

```toml
[appearance]
theme = "dracula"
```

Каждое переключение темы (через `cycle_theme`, окно настроек, контекстное меню или `rterm.set_theme()` из Lua) **перезаписывает** это поле. При следующем запуске тема восстановится автоматически.

`[colors]` всё ещё работает поверх: явные RGB-значения накладываются на выбранную тему. Это удобно, если хочется «как Dracula, но `bg` чуть темнее».

## Lua API для тем

```lua
local list = rterm.themes()              -- список встроенных тем
local now  = rterm.current_theme()       -- имя текущей темы
local ok   = rterm.set_theme("dracula")  -- применить тему; true/false
```

`set_theme` принимает имя без учёта регистра. Возвращает `false`, если имя неизвестно (применять ничего не будет). После успешного вызова всем плагинам приходит событие `theme` с новым именем, и тема записывается в `config.toml`.

Пример «авто-светлая днём, тёмная ночью»:

```lua
local function pick_theme()
  local h = tonumber(os.date("%H"))
  if h >= 7 and h < 19 then
    rterm.set_theme("light")
  else
    rterm.set_theme("default")
  end
end

rterm.on("startup", pick_theme)

-- Триггерить раз в час: по событию frame.tick раз в час
local last_check = 0
rterm.on("frame.tick", function(epoch_str)
  local epoch = tonumber(epoch_str) or 0
  if epoch - last_check > 3600 then
    last_check = epoch
    pick_theme()
  end
end)
```

## Произвольные палитры через `rterm.set_palette`

Если встроенных тем мало, плагин может выкатить произвольную палитру одним вызовом:

```lua
rterm.set_palette {
  default_fg = {220, 220, 220},
  default_bg = {15, 18, 24},
  cursor     = {255, 204, 102},
  named      = {
    [1]  = {  0,   0,   0},  -- black
    [2]  = {255,  85,  85},  -- red
    [3]  = { 80, 250, 123},  -- green
    [4]  = {241, 250, 140},  -- yellow
    [5]  = {189, 147, 249},  -- blue
    [6]  = {255, 121, 198},  -- magenta
    [7]  = {139, 233, 253},  -- cyan
    [8]  = {248, 248, 242},  -- white
    [9]  = { 98, 114, 164},  -- bright_black
    [10] = {255, 110, 110},  -- bright_red
    -- … остальные слоты
  },
}
```

Применяется немедленно. Плагины могут вернуть исходную тему через `rterm.set_theme(rterm.current_theme())`.
