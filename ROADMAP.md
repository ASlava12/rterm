# rterm — Roadmap

Живой план работ. Источник истины для «что делать дальше»: сессии
(человеческие и Claude — см. CLAUDE.md «How to continue iterations»)
берут следующий пункт отсюда, двигаясь сверху вниз внутри приоритета.

Статусы: `[ ]` не начато · `[~]` в работе · `[x]` сделано (переносится
в «Сделано» при релизе). У пунктов указаны привязки к коду и критерий
готовности (DoD), чтобы задачу можно было взять без археологии.

---

## P0 — Быстрые победы (≤ 1 день каждая)

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

- [ ] **Kitty keyboard protocol (CSI u, progressive enhancement).**
  Ждут neovim / helix / fish. Минимум: `CSI > flags u` push/pop stack,
  `CSI ? u` query, кодирование модификаторов по спеке в
  `forward_key_to_pty` при активных флагах.
  DoD: `kitten show-key` / nvim `:checkhealth` видят протокол;
  fallback на legacy при flags=0; unit на кодирование.

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

- [ ] **GIF-анимация.**
  Кадры декодирует crate `image`; нужен per-frame тайминг в
  image-pass + таймерные пробуждения (событийный цикл уже умеет
  `WaitUntil` — `schedule_after_frame`). Кэшировать кадры с бюджетом.
  DoD: анимированный GIF через iTerm2-протокол крутится; CPU в
  простое без анимаций не растёт.

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

- [ ] **Sixel-графика.** Главный пункт роадмапа.
  План (из CLAUDE.md): DCS-расширение парсера в rterm-core (Sixel идёт
  как `DCS P1;P2;P3 q ... ST`), потоковый декодер палитро-строк в
  RGBA, регистрация через существующий image store (`register_image`)
  и GPU image-pass; выравнивание по сетке при reflow. ReGIS —
  сознательно НЕ делаем (мёртвый формат).
  DoD: `img2sixel` / `lsix` отображают картинки; `cat` мусора с
  `\ePq` не крашит парсер (fuzz-тест); лимиты как у остальных
  протоколов (`IMAGE_MAX_PAYLOAD_BYTES`).

- [ ] **Профили и SSH-менеджер (WindTerm-режим).**
  Сохранённые подключения: `[[profiles]]` в конфиге (имя, команда/
  `ssh host`, cwd, тема, env), палитра «New tab with profile…»,
  быстрое переключение. Колонка `context` в history.db — готовый
  задел под per-host историю.
  DoD: профиль открывает таб с нужной командой/темой; история
  подсказок фильтруется по контексту хоста.

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

- [ ] Session-файл: межпроцессный лок или merge (сейчас две инстанции
  затирают сессии друг друга, last-writer-wins).
- [ ] Update-check: prerelease-тег, помеченный «latest», считается
  новее релиза (`parse_version` складывает `-rc.N` в число) —
  ужесточить при первом же rc-релизе.
- [ ] `PluginCmd`-канал: домигрировать легаси-очереди `pending_*`
  (архитектурная заметка в `rterm-plugin/src/lib.rs` у `cmd_tx`).
- [ ] Единый `enum ActiveOverlay` для клавиатуры/мыши/рендера
  (сейчас три рукописных порядка приоритета; расхождения закрыты
  точечными фиксами, но инвариант не enforced).
- [ ] Паста-секреты в history.db: опция redaction/opt-out для
  bracketed-paste payload'ов в `CommandCapture`.
- [ ] `[highlight]`: колонка `context`-стиль правил per-profile, когда
  появятся профили.

## P4 — Рекуррентные процессы (повторять периодически)

Эти пункты не «закрываются» — их прогоняют заново на каждом крупном
цикле работ (после серии фич, перед релизом, при заходе «делать
нечего — улучшай»). Отмечать `[x]` по факту последнего прогона с датой,
затем снова `[ ]` на следующем цикле.

- [ ] **Сверка фич: искать нереализованное.**
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
  Осталось (по желанию): распилить сам `window_event` на per-arm методы.
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
