# rterm — Roadmap

Живой план работ. Источник истины для «что делать дальше»: сессии
(человеческие и Claude — см. CLAUDE.md «How to continue iterations»)
берут следующий пункт отсюда, двигаясь сверху вниз внутри приоритета.

Статусы: `[ ]` не начато · `[~]` в работе · `[x]` сделано (переносится
в «Сделано» при релизе). У пунктов указаны привязки к коду и критерий
готовности (DoD), чтобы задачу можно было взять без археологии.

> **Состояние релиза (2026-07):** **0.0.13 подготовлен** — версия
> workspace поднята 0.0.12→0.0.13 + добавлен `CHANGELOG.md` (`df1628d`).
> Это НЕ публикует бинарники: их собирает только тег `v*.*.*`
> (`.github/workflows/release.yml`). **Чтобы выпустить:** `git tag v0.0.13
> && git push --tags` — outward-facing, оставлено за мейнтейнером. Батч
> релиз-готов: 3 headline-фичи + профили + macOS-хоткей + IME + broadcast
> + подсветка, и волна фиксов из 3 проходов адверсариального ревью (11
> багов, 2 security/privacy). CI зелёный на Linux/macOS/Windows.
> **Пайплайн проверен (2026-07):** `release.yml` собирает 6 таргетов,
> gate = clippy+lib-tests на теге; MSI-путь проверен прошлыми релизами.
> Найдено и исправлено: отсутствовали `LICENSE-MIT`/`LICENSE-APACHE`
> (workflow копировал их через `|| true` → архивы всех прошлых релизов
> уходили без текста лицензий); добавлены (`204833c`).

---

## P0 — Быстрые победы (≤ 1 день каждая)

<!-- Пункты ниже (2026-07) — из feature-gap sweep (P4). Привязки к коду
     проверены чтением; DoD у каждого. -->

- [x] **Доки: три вводящих в заблуждение утверждения.** (2026-07)
  `open_settings` не имеет дефолтной клавиши (шаблоны EN+RU и 2 стейл-
  комментария врали про `Ctrl+Shift+,`, забинженный на move-tab); macOS-
  путь конфига `~/.config/rterm/`, а не `~/Library/Application Support`
  (`Config::default_path`); вставка — `Shift+Insert`, не `Insert`. Правки
  только в доках/комментах.

- [x] **Плагины: `run_action("custom")` из Lua теперь работает.** (2026-07)
  `PluginCmd::RunAction`-хендлер (`event_loop.rs`) в `else`-ветке (не-
  билтин) теперь зовёт `self.events.run_action(&name)` — тот же путь,
  что у палитры. `rterm.run_action("my_custom")` диспетчеризует
  действие, зарегистрированное через `rterm.register_action`.

- [x] **Плагины: событие `attention` теперь эмитится.** (2026-07)
  `emit("attention", "")` при дренаже `pending_attention` (до
  focus-gated taskbar-пинга, событие идёт независимо от фокуса).
  Добавлено в `builtin_event_names()` + закреплено в anchor-тесте.
  `rterm.on("attention", ...)` теперь срабатывает.

- [x] **Плагины: `add_match` `opts.on` колбэк теперь вызывается.** (2026-07)
  `MatchRule` хранит `on: Option<RegistryKey>`; `add_match` читает
  `opts.on` как `Function` и кладёт в реестр Lua. `match_output_line`
  извлекает колбэки под локом (дешёвый Lua-ref) и зовёт `on(line)` ПОСЛЕ
  освобождения лока (deadlock-safe, если колбэк дёргает
  `add_match`/`remove_match`). Сигнатура в доках выправлена на
  `function(text)` (text = совпавшая строка; row/col для line-based
  матча не предоставляются). Тест на срабатывание. Весь P0-plugin-
  кластер sweep'а закрыт.

- [x] **Плагины: bare/`_of`-формы panel-аксессоров.** (2026-07)
  Зарегистрированы 6 недостающих bare-форм (`idle` / `scrollback_len` /
  `foreground_process` / `foreground_pgid` / `bell_muted` / `progress`)
  — читают фокусную панель (`find(|p| p.focused)`, зеркало `_of`-тела),
  и 2 `_of`-алиаса (`terminal_text_of` / `copy_pane_of`) по 1-based
  индексам. Обещанный доками трио `X()`/`X_of`/`X_by_uid` теперь
  реально покрыт. Тест на резолв bare→фокусная + `_of`→индекс.

- [x] **Тумблер подсветки синтаксиса в Settings-оверлее.** (2026-07)
  Чекбокс `[x] Syntax highlighting` + клавиша `Y`; рантайм-флип через
  `highlight::set_enabled`/`is_enabled`, persist `[highlight].enabled`
  через `persist_config_value`. Тесты: toggle через глобал + round-trip
  persist.

- [x] **Kitty-графика: `X=`/`Y=` офсеты размещения.** (2026-07)
  Находка аудита оказалась устаревшей: `col`/`abs_row` уже
  вычислялись с `x_offset`/`y_offset` и использовались в placement с
  самого первого коммита Kitty-протокола. Убрал мёртвый `let _ = col;`
  и закрепил поведение unit-тестом (`kitty_x_y_offsets_shift_the_placement`).

- [x] **CLI: `--history` / `--shell-integration` с любым положением флага.** (2026-07)
  Новый `args_after_flag(_in)` берёт хвост от позиции флага вместо
  хардкода `.skip(2)`/`.nth(2)`. `rterm --config x.toml --history list`
  и `--shell-integration bash` после `--config` теперь работают.
  Unit на позиционную независимость + функциональная проверка.

- [x] **Panic-hook с записью в файл.** (2026-07)
  `install_panic_hook` дописывает панику + backtrace в
  `<cache>/rterm/panic.log` и чейнится к дефолтному хуку. Запись
  вынесена в чистый `write_panic_record` (тестируемо без глобального
  стейта). Unit на append + сохранение сообщения/backtrace.

- [x] **Дренировать OSC 52 у всех панелей, не только фокусной.** (2026-07)
  Новая свободная функция `drain_osc52` дренирует каждую панель каждый
  кадр (нет накопления), применяет только запрос фокусной, фоновые —
  сбрасывает с debug-логом. Тест на реальных `Terminal`: фокусная
  применяется, фоновая дропается, обе дренированы.

- [x] **`--smoke`: изоляция от пользовательского Lua.** (2026-07)
  Проверка `--smoke` хоистнута сразу после загрузки конфига (до
  `PluginHost::new`/`load_user_lua`/`spawn_watcher`) — смоук больше не
  исполняет init.lua и не спавнит watcher. `run_smoke` теперь делает
  ограниченный по времени join ридера (Windows/ConPTY не виснет).
  Integration-тест (`tests/smoke_isolation.rs`) через `CARGO_BIN_EXE`:
  init.lua с сентинелом не исполняется под `--smoke`.

- [x] **Гигиена документации.** (2026-07)
  Полностью удалён мёртвый `title_bar`-плюмбинг (структура
  `TitleBarDraw`, поле `title_bar_buffer` + инициализация + обновления
  шрифта, always-None binding, параметр через `prepare`/`render`).
  Стейл-комментарии «next commit» у Kitty APC поправлены. Индексы
  `idx_commands_count/last_used` удалены (`DROP INDEX IF EXISTS` чистит
  старые БД; план LIKE-запроса их не использовал) + поправлена дока
  «O(log N)». Всё зелёное, GUI рендерит.

## P1 — Средние (несколько дней каждая)

<!-- Пункты ниже (2026-07) — из feature-gap sweep (P4). -->

- [x] **Alt-scroll mode (DECSET `?1007`): «мёртвое колесо» в pager'ах.** (2026-07)
  Ядро: поле `Terminal.alternate_scroll` (дефолт on, xterm-конвенция) —
  DECSET/DECRST `?1007` + DECRQM-запрос + сброс на RIS, аксессор
  `alternate_scroll()`. Render: в alt-ветке `handle_scroll` при
  выключенном mouse-режиме и включённом `?1007` колесо транслируется в
  стрелки через чистую `alt_scroll_bytes(step, app_cursor)` — CSI
  (`\x1b[A/B`) или SS3 (`\x1bOA/OB`) по `app_cursor_keys`, N раз по
  величине шага. `less`/`man`/`git log`/`systemctl` скроллятся колесом.
  Тесты: core-toggle+DECRQM, render-трансляция.

- [x] **Кастомные plugin-действия в `[[keybindings]]`.** (2026-07)
  `UserBinding.action` теперь `Option<AppAction>` (`None` = кастомное имя,
  резолвится в рантайме). `from_config` возвращает `None` только при
  непарсящемся key-spec, а не-билтин-имя принимает как кастомное
  (плагины регистрируют действия после загрузки конфига, поэтому
  провалидировать имя нельзя — незарегистрированное просто no-op, как в
  палитре). `check_user_bindings`: `Some(a)` → `dispatch_action`, `None`
  → `self.events.run_action(name)`. `--list-keybindings`/`--check`
  различают «(invalid key)» / «(custom action)» / builtin (+ поле
  `"custom"` в JSON). Тест на приём кастомного имени.

- [x] **SGR-Pixels mouse (DECSET `?1016`).** (2026-07)
  Ядро: поле `Terminal.sgr_pixel_mouse` — `?1016` set форсит `sgr_mouse`
  (пиксель-репорты идут в SGR-фрейминге) + DECRQM + RIS-сброс; reset
  оставляет `?1006`. Аксессор `sgr_pixel_mouse()`. Render: `mouse_mode_for`
  → `(mode, sgr, pixel)`; новый `pixel_to_pane_px` (пиксельный офсет в
  пределах панели) + централизующий `mouse_report_coords(idx,x,y,pixel)`
  зовётся во всех 4 сайтах репорта (wheel/press/drag/release). По спеке
  пиксельные ординаты те же 1-based → `encode_mouse` не менялся. Тест на
  core-toggle+force+DECRQM.

- [x] **IME: ввод CJK / dead keys / long-press macOS.** (2026-07)
  `set_ime_allowed(true)` при создании окна; `WindowEvent::Ime` arm —
  `Ime::Commit` уходит в фокусную панель через `send_input` (日本語
  попадает в шелл), кандидатное окно позиционируется у курсора через
  `set_ime_cursor_area` (`update_ime_cursor_area` + чистая
  `ime_cursor_rect` с тестом). Composing показывает ОС-попап; Backspace
  во время композиции правит preedit (перехвачен ОС, не уходит в PTY).

- [x] **IME: инлайн-рендер preedit у курсора.** (2026-07)
  `Ime::Preedit` сохраняется в `App.ime_preedit`; отрисовывается inline
  у курсора акцентным цветом поверх solid-backdrop квада (легибельность
  над контентом панели). Реализовано через `PreeditDraw` + `ime_buffer`
  в `TextLayer` + staging в `prepare`/`render` (по образцу бывшего
  `title_bar`); ширина по `UnicodeWidthStr`. Компилируется/рендерит без
  паник; визуальную корректность проверять живым IME на macOS.

- [x] **Kitty keyboard protocol (CSI u, progressive enhancement).** (2026-07)
  Ядро: per-screen стек флагов (`kitty_kbd_stack: [Vec<u8>; 2]`),
  `CSI > flags u` push / `CSI < n u` pop / `CSI = flags;mode u` set /
  `CSI ? u` query→`CSI ? flags u`, cap 32, сброс на RIS, аксессор
  `kitty_keyboard_flags()`. Render: чистый `kitty_encode_key` — Escape
  всегда дизамбигуируется (`CSI 27u`), текстовые клавиши с
  ctrl/alt/super (или flag 8) → `CSI cp;mods u`, Enter/Tab/Backspace/
  Space с модификатором → CSI u; функциональные клавиши падают в legacy
  `named_key_bytes` (уже xterm-modifier-форма, kitty-совместимая).
  Поля event-type (`:2` repeat, flag 2) и text (flag 16) поддержаны;
  flags=0 → полностью legacy. Тесты: core stack/query/per-screen +
  render-кодирование (Ctrl+A/Escape/Shift+Tab/flag8/flag2/flag16).
  Известные ограничения (документированы, не corrupting): база клавиши
  из logical_key (шифт-символы верхнего ряда репортят шифт-codepoint),
  release-события не репортятся (key-up дропается до PTY-пути),
  alternate-keys (flag 4) не эмитятся — всё degrade gracefully.

- [x] **Инкрементальный поиск вне лока терминала.** (2026-07)
  `refresh_matches` снимает снимок строк (`Vec<Vec<char>>`) под коротким
  локом, затем матчит ВНЕ лока — ридер-поток больше не стоит на весь
  regex/substring-скан scrollback. Матчинг вынесен в чистую
  тестируемую `search_rows` (substring + regex, smart-case) с
  unit-тестом.

- [x] **Broadcast input (ввод во все панели таба).** (2026-07)
  Действие `AppAction::ToggleBroadcast` (алиасы `broadcast_input` /
  `synchronize_panes`) в палитре; `forward_key_to_pty` рефакторен —
  байты идут через `dispatch_input_bytes`, который при активном
  broadcast рассылает во ВСЕ панели активного таба. Статус-бар
  форсируется с маркером `⇉ BROADCAST`. Off по умолчанию, runtime-only.

- [x] **GIF-анимация.** (2026-07)
  Декодер (`image_decode`): мульти-кадровый GIF через
  `GifDecoder`+`AnimationDecoder` → `DecodedImage.frames`
  (`Vec<AnimFrame>{rgba, delay_ms}`), кадры уже композитны (disposal
  применён крейтом), бюджет `GIF_MAX_FRAMES=256` / 256 MiB, задержка 0
  → 100 ms (флор 20 ms). image-pass: `ImageCacheEntry.anim` держит кадры
  + курсор; `advance_animations(queue, now)` продвигает по времени и
  re-uploadит текущий кадр in-place (view/bind-group не инвалидируются),
  с resync-защитой от долгого простоя. Тайминг вынесен в чистую
  `advance_frame_cursor` (3 теста). Интеграция: `next_animation_deadline`
  → `schedule_after_frame` (`WaitUntil`) → `new_events(ResumeTimeReached)`
  → redraw. CPU в простое БЕЗ анимаций не растёт (нет анимированных GIF →
  `None` дедлайн → `Wait`). Тест на декод 2-кадрового GIF с задержками.

- [x] **`scroll_offset` u16 → u32.** (2026-07)
  `Pane.scroll_offset` → `AtomicU32`; core API (`visible_row` /
  `row_wrapped` / `hyperlink_at` / `detect_url_at`), `build_spans`,
  селекшн (`to_viewport_row` / `to_visible_norm` / `from_viewport`),
  снапшоты (`PaneSnapshotInfo` / `PaneDraw` / plugin `PaneInfo`) и все
  store/сатурация-сайты расширены. Хвост scrollback до потолка 1M
  строк теперь достижим (раньше > 65 535 недостижим). Тип-only,
  без изменения логики; `clamp_scroll_offset` сатурирует у u32::MAX.

- [x] **Хит-тест Confirm-кнопок paste-модалки без «бэндейджа».** (2026-07)
  Confirm-режим теперь рендерится wrap-off (как Edit/settings) —
  логические строки == визуальные, `button_row_index` точен и не
  съезжает под длинным превью. «Бэндейдж» ужат с 4 рядов до 1
  толеранса. Математика ряда вынесена в тестируемую
  `confirm_button_row_index` с unit-тестом.

- [x] **Мелкая консистентность UI** (пакетом). (2026-07)
  ✅ анимация у `switch_tab` (Ctrl+Shift+←/→) как у `select_tab`;
  ✅ middle-click close через `close_tab_at` (убрана инлайн-копия);
  ✅ `handle_key` из match-guard в обычную ветку;
  ✅ порог tab-drag: press ставит `tab_drag_pending`, реальный drag
  (и `tab.drag_start`) стартует только при смещении >
  `TAB_DRAG_THRESHOLD_PX` — плейн-клик больше не эмитит start/end
  пару. Чистая тестируемая `tab_drag_exceeds_threshold`.

## P2 — Крупные (недели)

- [~] **Глобальный хоткей: бэкенды macOS/Linux.** (macOS сделан, 2026-07)
  Был только Windows; `#[cfg(not(windows))]`-ветка делала no-op + warn.
  **macOS (сделано):** новый `macos_impl` в `global_hotkey.rs` —
  Carbon `RegisterEventHotKey` + `InstallEventHandler` на
  `GetEventDispatcherTarget()`. Carbon доставляет `kEventHotKeyPressed`
  на главный run-loop, который winit уже прокачивает, так что worker-
  поток НЕ нужен (в отличие от Windows-пути) — хендлер форвардит через
  `EventLoopProxy::send_event(GuakeGlobalHotkey)`. FFI hand-rolled (в
  духе hand-rolled Windows-бэкенда, без новых зависимостей; символы
  живы в 64-bit/Apple Silicon Carbon). RAII `MacHandle::drop` →
  `UnregisterEventHotKey`/`RemoveEventHandler` + освобождение boxed-
  прокси (после снятия хендлера — колбэк не увидит dangling). Маппинг:
  `named_to_macos_vk`/`char_to_macos_vk` (kVK_*-коды — НЕ ASCII, нужна
  таблица; покрыты буквы/цифры/пунктуация, включая backtick =
  `kVK_ANSI_Grave` для канонического `Super+\``), `mods_to_carbon`
  (cmd/shift/option/control маски из Events.h). Тесты: 4 юнита на
  маппинг (F-клавиши, case-insensitive char, grave, mods, round-trip
  через `parse_key_spec`). Проверено `--render-test` с guake-конфигом:
  регистрация + Drop-очистка без паники (OSStatus 0), небиндируемая
  клавиша → warn + graceful fallback. Живой захват хоткея (колбэк на
  реальном нажатии) headless не проверяется — нужна ручная проверка на
  Mac. Доки поправлены (модуль + `rterm-config` global_hotkey).
  **Linux (осталось):** X11 `XGrabKey` (+ путь под Wayland-протокол);
  сейчас всё ещё no-op + warn.

- [~] **Sixel-графика.** Главный пункт роадмапа. (функционален; полиш, 2026-07)
  План (из CLAUDE.md): DCS-расширение парсера в rterm-core (Sixel идёт
  как `DCS P1;P2;P3 q ... ST`), потоковый декодер палитро-строк в
  RGBA, регистрация через существующий image store (`register_image`)
  и GPU image-pass; выравнивание по сетке при reflow. ReGIS —
  сознательно НЕ делаем (мёртвый формат).
  DoD: `img2sixel` / `lsix` отображают картинки; `cat` мусора с
  `\ePq` не крашит парсер (fuzz-тест); лимиты как у остальных
  протоколов (`IMAGE_MAX_PAYLOAD_BYTES`).
  **Stage 1 (сделано):** чистый декодер `rterm_core::sixel::decode(data)
  → SixelImage{width,height,rgba}` — sixel-байты (`?`..`~`, 6 пикс/байт),
  палитра (`#Pc` select / `#Pc;Pu;Px;Py;Pz` define, RGB 0–100% + HLS),
  `$`/`-`/`!Pn`-повтор, raster-атрибуты `"`, дефолтная VT340-палитра;
  капы 4096²/16M пикс; мусор/пустой → None без паники. 6 юнит-тестов
  (колонка/прозрачность/RGB+repeat/банды/raster/junk). Pub-модуль, ещё
  не подключён к DCS (безопасно: поведение не меняется).
  **Stage 2 (сделано):** мост DCS→изображение. `TerminalPerform` уже
  имеет &mut-доступ к image-store, так что регистрация идёт прямо в
  `unhook` (не нужен отложенный буфер). `hook` детектит `q`-финал без
  intermediates (отличие от `$q`/`+q` запросов) → `dcs_is_sixel`; `put`
  копит тело в `dcs_buf` до `IMAGE_MAX_PAYLOAD_BYTES`; `unhook` →
  `commit_sixel` → `sixel::decode` → новый `place_decoded_image`
  (cols/rows-оценка 8×16 px как у iTerm2, `register_image_inline(Rgba8)`,
  `ImagePlacement` в позиции курсора, `linefeed`×rows). Рендер
  переиспользуется как есть (Rgba8 идёт через тот же image-pass).
  `img2sixel`/`lsix` теперь отображают картинки. Тесты: DCS→placement +
  DCS-мусор без паники/размещения. **Sixel функционален end-to-end.**
  **Stage 3 (частично):** fuzz-хардненинг — детерминированный LCG-fuzz
  декодера (3000 входов) + полного DCS-пути (400 входов), никогда не
  паникует, границы держатся, буфер = w*h*4. Поймал и пофиксил реальный
  memory-bomb: `!Pn` с огромным `Pn` крутил цикл до bounds-check → grid
  рос неограниченно; теперь повтор клампится по остатку ширины.
  Осталось (по желанию): точное reflow-выравнивание по сетке при
  ресайзе, P2-фон-режим (`?` background select), aspect-ratio из
  raster-атрибутов.

- [x] **Профили и SSH-менеджер (WindTerm-режим).** (2026-07)
  Сохранённые подключения: `[[profiles]]` в конфиге (имя, команда/
  `ssh host`, cwd, тема, env), палитра «New tab with profile…»,
  быстрое переключение. Колонка `context` в history.db — готовый
  задел под per-host историю.
  DoD: профиль открывает таб с нужной командой/темой; история
  подсказок фильтруется по контексту хоста.
  **Stage 1 (сделано):** схема `ProfileConfig` + `Config.profiles:
  Vec<ProfileConfig>` (name/program/args/cwd/env/theme) + `Config::
  profile(name)`. Запуск `rterm --profile <name>`: `GuiSpawner` держит
  `active_profile`, `spawn_pane` переопределяет program/args/cwd
  (`~`-expand)/env (поверх `[shell.env]`); тема профиля перебивает
  `[appearance].theme`. CLI `--list-profiles [--json]`. Доки в обоих
  шаблонах + `--help`. Тесты: парсинг+резолв профилей. Проверено:
  GUI-запуск с профилем спавнит его команду.
  **Stage 2 (сделано):** палитра «New tab with profile: X». `PaneSpawner`
  расширен `spawn_pane_with_profile(cwd, name)` (default → `spawn_pane`);
  `GuiSpawner` рефакторен — общий inherent `spawn_with_profile(cwd,
  profile)`, оба trait-метода делегируют. App держит `profile_names`
  (из `RunConfig`), палитра рендерит entry после builtins+plugin;
  индекс-маппинг `[builtins][custom][profiles]` централизован в
  `PaletteState::entry/label/len` (+`PaletteEntry` enum), обновлены все
  4 сайта (count/filter/dispatch/render). Выбор → `new_tab_with_profile`
  → новый таб через профиль (общий `push_new_tab`). Тест на индекс-
  маппинг. Профиль-табы подсвечены accent-цветом.
  **Stage 3 (сделано):** per-context история подсказок. history.db —
  композитный ключ `(text, context)` вместо `text PRIMARY KEY`
  (+in-place миграция старых БД, сохраняет строки, тест); `record(text,
  context)` / `suggest(prefix, limit, context)` бакетируют по контексту.
  Проброс: `CommandCapture.context` → `Pane::new(.., history_context)` →
  `GuiSpawner` ставит имя профиля (иначе `*`); `suggestion_popup::compute`
  берёт контекст фокусной панели. Профиль/SSH-панель видит свою историю,
  изолированную от локальной `*`. Тесты: изоляция + миграция.
  DoD выполнен. Осталось опционально: per-tab тема профиля (сейчас тема —
  только launch-time), `--history --context <name>` для инспекции бакетов.

- [ ] **Лигатуры (Fira Code / JetBrains Mono).**
  Сейчас `set_monospace_width` + пер-ячеечная сетка разбивают
  лигатуры. Нужен шейпинг по текстовым ранам с маппингом кластеров на
  колонки сетки (курсор/выделение поверх лигатуры — самое сложное).
  Исследовать: cosmic-text shaping runs vs glyphon позиционирование.
  DoD: `->` `=>` `!=` рендерятся лигатурами при включённой опции
  `font.ligatures = true` (по умолчанию false); курсор внутри
  лигатуры не ломает отрисовку.

- [ ] **Damage tracking (инкрементальный рендер).**
  Event-loop уже событийный, но каждый кадр перестраивает все спаны и
  решейпит буферы всех панелей. Грязные флаги от `Terminal::advance`
  (per-pane generation counter + грязные строки) → решейпить только
  изменённые панели/строки.
  DoD: `cat большого файла` в одной панели не решейпит соседнюю
  (счётчик в бенче); FPS при флуде не падает.

- [ ] **Session detach/attach (tmux-lite).** Самое амбициозное.
  Разделение владельца PTY и рендера на процессы (daemon держит PTY +
  Terminal, GUI подключается через IPC). Пререквизит: протокол
  сериализации грида. Рассматривать только при реальном спросе.

## P3 — Технический долг (низкий приоритет, по случаю)

- [x] Session-файл: две инстанции больше не затирают сессии. (2026-07)
  Merge вместо last-writer-wins, БЕЗ новой зависимости: `write_session`
  теперь append (`append_user_private`, `O_APPEND` — конкурентные
  аппенды атомарны на POSIX / `FILE_APPEND_DATA` на Windows), фокусный
  таб помечается per-block `active = true` (нет racy top-level ключа).
  `read_session` делает atomic-rename → read → delete: восстановление
  race-safe (in-flight аппенд уходит в свежий `session.toml`, не
  теряется) и чистит файл, чтобы табы не восстанавливались на каждом
  запуске. Старый формат (top-level `active = N`) читается как fallback.
  Тесты: per-tab active + merge аппендов + read-and-clear.
- [x] Update-check: prerelease-тег больше не считается новее релиза. (2026-07)
  `parse_version` теперь возвращает `(release, Option<prerelease>)`
  (раньше складывал `-rc.N` в тот же вектор → `[0,0,13,1] > [0,0,13]`).
  `is_newer` сравнивает release-ядро, затем по semver трактует
  prerelease < release (`v0.0.13-rc.1` НЕ новее `0.0.13`; стабильный
  релиз новее running prerelease; два rc — по номеру). Тесты на все
  случаи.
- [ ] `PluginCmd`-канал: домигрировать легаси-очереди `pending_*`
  (архитектурная заметка в `rterm-plugin/src/lib.rs` у `cmd_tx`).
- [ ] Единый `enum ActiveOverlay` для клавиатуры/мыши/рендера
  (сейчас три рукописных порядка приоритета; расхождения закрыты
  точечными фиксами, но инвариант не enforced).
- [x] Паста-секреты в history.db: опция redaction. (2026-07)
  `[history] redact_pasted` (дефолт false). `CommandBuffer` детектит
  bracketed-paste (`CSI 200 ~`) и метит строку; `take_command`/`feed`
  отдают `(cmd, had_paste)`; `CommandCapture` пропускает запись
  paste-команд при флаге. Проброс: `Pane::new(redact_pasted)` ←
  `GuiSpawner` ← `config.history.redact_pasted`. Ctrl+U/C сбрасывают
  taint. Тесты: детект paste (+ arrow-key ≠ paste) и пропуск записи.
  Discovery через `--print-config`.
- [ ] `[highlight]`: колонка `context`-стиль правил per-profile, когда
  появятся профили.
- [x] Минорные VT-моды (из sweep). (2026-07)
  `?1048` — реализован: set → `save_cursor()`, reset → `restore_cursor()`
  (как DECSC/DECRC, но по моду). `?3` DECCOLM — явно игнорируется
  (окно юзер-контролируемо; частичное honor'ение garble'ит) как
  kitty/alacritty. `?1005`/`?1015` — явные no-op арм с комментом
  (легаси mouse-кодировки почти вымерли, apps шлют и `?1006`, который
  поддержан; отдельный энкодер не оправдан). Тест на `?1048`.
- [ ] OSC 133 `;B`/`;C` шелл-интеграция: сейчас приняты молча
  (`terminal.rs:~3998`, только `;A`/`;D` обрабатываются). Захватывать
  границы command-input/output только если появится фича (фолдинг вывода
  по командам, точное выделение command-region).

<!-- Отложено из review-батча 3 (2026-07) — реальные, но узкие/спорные/
     требующие аккуратности; вынесены сюда, чтобы не терять. -->
- [x] **IME: чистить `ime_preedit` при смене фокуса.** (2026-07)
  Вместо разбросанных `clear()` по ~14 сайтам — центральный anchor-подход:
  `ime_anchor: Option<u64>` (UID панели-владельца, ставится на `Preedit`,
  чистится на `Commit`/`Disabled`); в redraw-пути preedit сбрасывается,
  если UID фокусной панели ≠ anchor. UID стабильны → чистка срабатывает
  только на реальной смене фокуса, обычная композиция на той же панели не
  задета. Живую IME-проверку (фокус-свитч посреди композиции) валидировать
  вручную на Mac/X11. (`ef5059d`)
- [x] **IME: клампить ширину preedit-рендера по панели.** (2026-07)
  `preedit_info` (`event_loop.rs`) теперь клампит ширину preedit-квада
  по правому краю фокусной панели (`max_w = rect.left + rect.width - x`),
  так что длинная композиция у правого края сплита не рисует backdrop +
  глифы поверх соседней панели. (`940b18a`)
- [x] **Mouse: ?1003 bare-hover motion.** (2026-07)
  Новый `report_hover_motion` (`input.rs`) в else-ветке CursorMoved-gate:
  когда нет drag'а и у панели под курсором активен ?1003 (any-event),
  шлёт motion-репорт (button 3 «no button» + 32 motion + модификаторы),
  дедуп по ячейке/пикселю (?1016), чтобы не флудить TUI. Инертен для
  прочих mouse-режимов. Тест на кодирование bare-hover кнопки (35). ?1002
  drag уже работал (батч 3). (`5f76e5e`)
- [x] **Restore: title применяется не к тому табу при провале spawn.** (2026-07)
  `new_tab_in` теперь возвращает `bool` (был push или нет); restore-цикл
  (`event_loop.rs`) при `false` делает `continue` — title и `tab.title`-
  эмит применяются только к реально добавленному табу. Узкий триггер
  (fd-исчерпание при spawn). (`940b18a`)
- [x] **Broadcast: рассылать paste во все панели.** (2026-07)
  Трактовка: broadcast = «мой ввод во все панели», паста — ввод; клавиши
  уже рассылались, паста была несогласованно focused-only. Direct
  (не-modal) паста при `broadcast_input` теперь фанится во все панели
  активного таба с обёрткой bracketed-paste **per-pane** (`?2004` у шеллов
  может различаться); modal-targeted паста (`target_uid`) остаётся
  привязанной. Нормализация/обёртка вынесены в чистые `normalize_paste`/
  `wrap_paste` + юнит-тест. Scoped к broadcast-on. (`db627c1`)

## P4 — Рекуррентные процессы (повторять периодически)

Эти пункты не «закрываются» — их прогоняют заново на каждом крупном
цикле работ (после серии фич, перед релизом, при заходе «делать
нечего — улучшай»). Отмечать `[x]` по факту последнего прогона с датой,
затем снова `[ ]` на следующем цикле.

- [x] **Сверка фич: искать нереализованное.** (прогон 2026-07)
  Пройтись по заявленным возможностям и найти дыры: (1) фичи из
  README / docs / `--help` / шаблонов конфига, не работающие или
  работающие частично; (2) VT/ANSI-последовательности, которые
  реальные программы шлют, а ядро молча глотает (сверять с
  `xterm ctlseqs`, vttest, `kitten show-key`, поведением neovim /
  tmux / htop); (3) заглушки в коде (`let _ =`, «future work»,
  «next commit», `#[allow(dead_code)]`), где задел есть, а
  реализации нет; (4) плагинный API — методы, объявленные в
  docs/plugins-api.md, но не подключённые. Найденное — новыми
  пунктами в P0–P2 этого файла с привязкой к коду и DoD.
  Метод: `grep` маркеров + прогон vttest/`show-key` + сверка
  README↔код; при масштабе — параллельные агенты-ревьюеры по зонам.
  Прогон 2026-07 (4 параллельных агента по зонам): docs↔код, VT/ANSI-
  полнота ядра, plugin API, code-стабы. Итог: ядро/доки/плагины в
  основном исчерпывающе подключены. Заведены новые пункты — P0: 3 doc-
  фикса (сделаны) + 4 мелких plugin-фикса (`run_action` custom,
  `attention`-эмит, `add_match` on-колбэк, bare-аксессоры); P1: alt-
  scroll `?1007` (мёртвое колесо в pager'ах), кастомные действия в
  `[[keybindings]]`, `?1016` pixel-mouse; P2: глобальный хоткей
  macOS/Linux; P3: минорные VT-моды (`?1048`/`?1005`/`?1015`/`?3`),
  OSC 133 `;B`/`;C`. Мёртвого кода нет (все `#[allow(dead_code)]`
  легитимны). Сбросить в `[ ]` на следующем крупном цикле.
  **Ре-прогон перед релизом (2026-07):** целевой self-sweep по сигналам
  неполноты (стабы/dead-code/`future work`/`let _ =`, honoring новых
  config-ключей, plugin-API). Единственная находка — стейл-маркер
  `#[allow(dead_code)]` + «reserved for follow-up» на `PasteConfirmation
  ::pane_uid`, хотя фича уже подключена (`commit_paste_now(_, Some(uid))`);
  снят (`c3228de`). Всё остальное: новые config-ключи (guake-layout,
  history-popup, профили) honored; прочие allow'ы легитимны (RAII-
  keepalive'ы, cfg-бэкенды, `PluginCmd`-migration-поле). Вывод: код
  полон и релиз-готов.

- [x] **Адверсариальное ревью нового/рискового кода перед релизом.** (прогон 2026-07)
  Не «дыры в фичах» (это отдельный пункт выше), а поиск БАГОВ в свежем
  и высокорисковом коде: unsafe/FFI, декодеры недоверенного ввода,
  concurrency. Метод: параллельные агенты-скептики по зонам, каждый
  требует конкретный failure-scenario; находки верифицируются вручную
  перед фиксом. Прогон 2026-07 (3 агента): (1) macOS Carbon FFI
  (`global_hotkey.rs`) — чисто (сигнатуры/memory-safety/kVK-таблица/
  threading верны). (2) Sixel-декодер+DCS-мост — **2 бага**: HLS-hue
  повёрнут на 120° (offset `+120`→`+240`; синий рендерился зелёным —
  RGB-путь не задет, потому и проскочило) + MAX_PIXELS (67MB) не
  выровнен с downstream-капом (16MB) → амплификация аллокации. Оба
  пофикшены (`dc6c50f`) + тесты. (3) Session-merge — **data-loss
  вектор**: весь файл парсился как один TOML-документ, один порванный
  `[[tab]]`-блок (SIGKILL/torn write/битый байт в cwd) стирал сессии
  всех окон безвозвратно. Пофикшено (`c9264bb`): resilient per-block
  парсинг + полный эскейп control-символов в cwd + безусловный 0o600 +
  честные комментарии про гонки.
  Прогон 2026-07, батч 2 (4 агента по ещё не проверенным зонам):
  (4) palette index-маппинг (builtins/custom/profiles ×4 сайта) — чисто
  (централизован через `PaletteState::entry`, профиль резолвится по
  имени). (5) command-capture/redaction — **утечка секретов**:
  `redact_pasted` был per-line, а bracketed-paste — per-payload span;
  многострочная вставка теряла taint на каждом `\n` → строки 2..N
  секрета писались в историю. Пофикшено (`82c858c`): span-трекинг
  `in_paste` + guard control-байтов + cap буфера. (6) Kitty keyboard —
  **2 бага**: RIS не сбрасывал флаг-стек (после `tput reset` плейн-ввод
  CSI-u-кодировался в мусор; DECSTR-то чистил) + flag-16 text-поле
  эмитилось для ctrl/alt/super-комбо (лишний вставляемый символ).
  Пофикшено (`de16801`). (7) GIF-декод — **OOM недоверенного ввода**:
  `decode_gif_animation` не ставил `image::Limits` (в отличие от
  `decode_still`) → кадр 65535² форсил ~16GiB аллок → abort/паника.
  Пофикшено (`cbc1453`).
  Прогон 2026-07, батч 3 (3 агента по зонам умеренного риска):
  (8) mouse-кодирование — **3 бага**: drag-motion reports были мёртвым
  кодом (press ставил `mouse_pty_pane`, но gate CursorMoved не звал
  `handle_drag` → drag-select в vim/tmux слал только press+release);
  колесо репортилось 3× приложениям (3× множитель локального scrollback
  тёк в report-цикл); модификаторы (Shift/Alt/Ctrl) не кодировались в
  кнопку. (9) IME — composition-guard отсутствовал (`forward_key_to_pty`
  слал в PTY во время композиции → backspace-corruption/double-input) +
  IME-commit шёл мимо broadcast. (10) broadcast+restore: IME-commit не
  рассылался (пофикшено вместе с IME); отложены — restore применял title
  не к тому табу при провале spawn (узкий триггер), paste не
  рассылается (спорный дизайн). Пофикшено (`916e06c`, 5 фиксов + тесты).
  Итог трёх батчей: **11 багов в 10 зонах** (2 security/privacy), все с
  тестами; отложено 5 пунктов (см. P3 ниже). Вывод: релиз-готовность
  существенно повышена. Сбросить в `[ ]` перед следующим релизом.

- [~] **Рефакторинг-проход по коду.** (текущий цикл: 2026-07)
  Пройтись по кодовой базе и снизить долг БЕЗ смены поведения:
  (1) распилить гигантский `rterm-render/src/lib.rs` (~16k строк) —
  выносить связные блоки в модули (кандидаты: `input.rs`
  клавиатура/мышь, `frame.rs` пайплайн RedrawRequested, `snapshot.rs`
  сбор состояния для плагинов), как уже вынесены `overlay`, `layout`,
  `window_ops`;
  Прогресс: `input.rs` (~690 строк) — вынесен весь горячий путь ввода
  как `impl App`: PTY-паста (`paste_clipboard` / `paste_primary` /
  `write_paste` / `commit_paste_now`), key→байты (`dispatch_input_bytes`
  / `forward_key_to_pty`), колесо (`handle_scroll`) и key-диспетчеры
  (`handle_key` / `handle_palette_key` / `handle_search_key`), плюс
  `handle_scroll_key` (Shift+PgUp/PgDn/Home/End) и `handle_rename_key`
  (редактор имени таба); затем оверлейные key-хендлеры
  `handle_settings_key` / `handle_context_menu_key` /
  `handle_paste_confirmation_key` / `handle_suggestion_popup_key`.
  `lib.rs` 16.0k → 14.8k. Все key-хендлеры теперь в `input.rs`. Плюс
  мышиные entry-хендлеры модалок/попапов
  (`handle_paste_confirmation_press` / `handle_paste_confirmation_wheel`
  / `handle_suggestion_popup_press` / `update_context_menu_hover`).
  `lib.rs` → 14.7k. Плюс ядро мышиного взаимодействия — `handle_press`
  (клик/фокус/старт-выделения/таб-хиты, ~385 стр) и `handle_drag`
  (drag-выделение + reorder табов). `lib.rs` → 14.2k (`input.rs` ~1826;
  весь путь клавиатуры+мыши теперь тут). Общая геометрия хит-теста
  (`pixel_to_cell` / `*_rect` / `abs_point` / `paste_modal_hit_test`)
  осознанно осталась в `lib.rs` — она общая с рендером; зовётся из
  `input.rs` как приватный-для-корня метод.
  Затем `event_loop.rs` — весь `impl ApplicationHandler<UserEvent> for
  App` (~2.8k строк: `resumed`/`new_events`/`user_event`/`exiting`/
  `window_event`, включая `RedrawRequested`-пайплайн со сборкой
  снапшота для плагинов и GPU prepare/render). `lib.rs` 14.2k → 11.4k.
  Этим закрыты оба кандидата `frame.rs` (RedrawRequested сидит в
  `window_event`) и `snapshot.rs` (снапшот строится там же инлайн).
  Затем `gpu.rs` — `struct GpuState` + весь его `impl` (~620 стр: wgpu
  surface/device/queue init с WSL2-оверрайдами, resize, opacity,
  per-frame `render` со сшивкой bg→image→glyph→overlay пассов). Поля
  `window`/`config` подняты до `pub(crate)` (читаются снаружи); GpuState
  реэкспортится `pub use gpu::GpuState`. `lib.rs` 11.4k → 10.8k.
  Затем `payload.rs` — 18 чистых `String`-билдеров payload'ов плагинных
  событий (`pane_*_payload` / `tab_*_payload` / `progress_*` /
  `pane_exit_payload` / `pane_split_payload`) + текст-снапшоты
  (`scrollback_text_snapshot(_capped)` / `grid_text_snapshot`) + мапы
  имя→код (`cursor_shape_code` / `mouse_mode_code`). Реэкспорт
  `pub(crate) use payload::*` — вызовы из `event_loop.rs` и тесты в
  `lib.rs` резолвятся без правок. `lib.rs` 10.8k → 10.5k.
  Затем input-кодировщики клавиатуры в `input.rs` как приватные:
  `named_key_bytes` + его CSI-хелперы (`xterm_mod_code` /
  `direction_letter` / `tilde_code` / `f1_f4_letter`),
  `is_bare_modifier_key`, `ctrl_byte` — вместе с их 3 юнит-тестами
  (переехали в `input::tests`, вызовы + тесты в одном модуле → ни
  `pub(crate)`, ни реэкспорт не нужны). `encode_mouse` осознанно
  оставлен в `lib.rs` — реально общий (есть mouse-release-вызов вне
  `input.rs`). `lib.rs` 10.5k → 10.3k.
  Затем `window_event`: гигантская ветка `RedrawRequested` (~2.4k строк:
  снапшот для плагинов + дренаж `PluginCmd` + GPU-кадр) вынесена в метод
  `App::on_redraw(event_loop)` — тело verbatim (дедент на один уровень;
  проверено: нет raw/многострочных строк, мин. отступ 16, `return` —
  только в комментариях). `window_event` теперь читается как список
  веток. Кандидат (1) распила `lib.rs`/`event_loop.rs` исчерпан.
  (2) чистая математика (геометрия, хит-тесты,
  кодирование) — в свободные функции ради юнит-тестов; (3) убрать
  дублирование (инлайн-копии `close_tab_at` и т.п.), стейл-комментарии,
  мёртвый код; (4) единообразить идиомы (poison-tolerant локи,
  `saturating_*`, обработку ошибок). Инвариант: `cargo test --workspace`
  и `clippy -D warnings` до и после зелёные; каждый шаг — отдельный
  коммит `refactor(...)`, поведение не меняется (проверять smoke +
  ключевые тесты). Начинать с самого крупного файла.
  Метод: маленькими проверяемыми шагами; не смешивать с фиксами
  поведения — рефактор и багфикс в разных коммитах.

## Сделано (важнейшее, для контекста)

- v0.0.12 + после: полный аудит-свип (атомарные записи конфига,
  kill-on-drop PTY, неблокирующий writer, Lua-вотчдог, капы очередей,
  лимиты декодера изображений, потолок scrollback), событийный
  event-loop (~0% CPU в простое), клиентская подсветка синтаксиса
  (`[highlight]`, WindTerm-style), HiDPI-шрифты, monospace_fallback.
- Исторически: VT-ядро (SGR/DECSET/OSC 0/2/4/7/8/9/10/11/52/104/133/
  633/777/1337, DECSCNM, ?2026 sync), табы + BSP-сплиты + zoom + swap,
  поиск (regex), подтверждение вставки с редактором, hot-reload
  конфига/Lua, Lua-плагины (~140 API), inline-изображения
  (iTerm2/Kitty raw+PNG), session restore, Guake-режим, темы,
  командная палитра, suggestion popup + SQLite-история.
