# Документация rterm

Cross-platform GPU-ускоренный терминальный эмулятор с Lua-плагинами.

## Содержание

| Файл | О чём |
|------|-------|
| [Getting started](getting-started.md) | Установка, первый запуск, пути, hot-reload, переменные окружения. |
| [Configuration](config.md) | Полная схема `config.toml` со столбцом допустимых значений. |
| [Keybindings](keybindings.md) | Дефолтные сочетания, парсер `keys`, полный список actions. |
| [Themes & Appearance](themes.md) | Встроенные темы, `[appearance]`, кастомные `[colors]`, Lua API. |
| [UI tour](ui.md) | Вкладки, меню, контекстные меню, окно, snap. |
| [Plugins guide](plugins.md) | Жизненный цикл, события, идиомы, отладка. |
| [Plugins API reference](plugins-api.md) | Полный справочник по `rterm.*`. |

## С чего начать

1. Прочитай [Getting started](getting-started.md) — за пять минут получишь рабочий конфиг.
2. Если важна внешний вид — [Themes & Appearance](themes.md) → выбери встроенную тему.
3. Если нужны свои сочетания — [Keybindings](keybindings.md).
4. Хочешь автоматизировать поведение — [Plugins guide](plugins.md).

## Связанные ссылки

- `README.md` в корне репо — обзор архитектуры и список крейтов.
- `CLAUDE.md` — заметки для Claude Code (но содержат полезную картину архитектуры).
- `rterm --list-actions --json`, `rterm --list-events --json` — авторитетные источники для имён действий и событий.
- `rterm --print-default-config` — закомментированный шаблон `config.toml`.
- `rterm --check` — валидация конфига и Lua без запуска GUI.
