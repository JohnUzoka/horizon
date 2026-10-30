//! The interactive title before world resources exist. Uses only the menu
//! atlas/panorama; transfers the actual screen and queued entry to Game. A
//! controller works on it from its first frame (`poll_title_pad`).
use super::*;
use horizon_client::{
    clipboard::{self, AppendSelection, SystemClipboard},
    gamepad::{self, Button},
    options_ui::{Event, OptionsUi, Page},
    widgets::{self, Kind, Target},
};
use winit::{event::KeyEvent, keyboard::ModifiersState};

#[derive(Default)]
pub(super) struct StartupMenu {
    title: Option<Title>,
    options: OptionsUi,
    options_open: bool,
    /// The credits from Credits & Attribution, and their last frame.
    win: Option<(horizon_client::win_screen::WinScreen, Instant)>,
    root: PathBuf,
    cursor: (f32, f32),
    pointer: (f64, f64),
    modifiers: ModifiersState,
    physical_keys: HashSet<KeyCode>,
    dragging: bool,
    clipboard: SystemClipboard,
    selection: AppendSelection,
    pub pending: Option<PendingWorldEntry>,
    pub reload_packs: bool,
    pub perf: horizon_client::telemetry::Session,
    /// The controllers, opened with the title so they work from its first
    /// frame, and handed to `Game` with the screen.
    pads: super::pad::Pads,
    /// Where the pad is on the title.
    nav: TitleNav,
}

impl StartupMenu {
    pub fn new(options: &Options, pack_error: Option<&str>) -> Result<Self> {
        let pending = if let Some(address) = &options.server {
            Some(PendingWorldEntry::Server {
                address: address.clone(),
                name: options.player_name(),
                cookies: None,
            })
        } else {
            options
                .world
                .as_ref()
                .map(|folder| PendingWorldEntry::Local {
                    folder: folder.clone(),
                    name: None,
                    seed: Some(options.seed),
                    creative: options.creative,
                    create: None,
                })
        };
        let mut menu = Self {
            pending,
            ..Default::default()
        };
        menu.root = options.resources.clone();
        if let Some(error) = pack_error {
            menu.options.open_at(false, Page::Packs);
            menu.options.resource_error = Some(error.to_owned());
            menu.options_open = true;
        }
        if menu.pending.is_some() {
            menu.perf.world_opening();
        }
        Ok(menu)
    }

    pub(super) fn set_root(&mut self, root: PathBuf, settings: &Settings) {
        horizon_client::ui::load_metadata(&root);
        horizon_client::lang::set(horizon_client::lang::Language::load(
            &[&root],
            &settings.language,
        ));
        self.root = root;
        if self.title.is_none() {
            let mut title = Title::new(
                std::env::current_dir().unwrap_or_default().join("saves"),
                &self.root,
            );
            title.last_server = settings.file.string("lastServer");
            title.skip_multiplayer_warning = settings.file.bool("skipMultiplayerWarning");
            // Gui.addInitialScreens: the accessibility onboarding comes first.
            if settings.file.bool("onboardAccessibility") {
                title.start_onboarding();
            }
            self.title = Some(title);
        }
    }

    pub(super) fn handoff(&mut self, game: &mut Game<'_>) {
        game.title = self.title.take();
        game.pending_entry = self.pending.take();
        game.cursor = self.cursor;
        game.modifiers = self.modifiers;
        game.physical_keys = std::mem::take(&mut self.physical_keys);
        game.clipboard = std::mem::take(&mut self.clipboard);
        game.text_selection = std::mem::take(&mut self.selection);
        game.perf = std::mem::take(&mut self.perf);
        game.pad = std::mem::take(&mut self.pads);
        game.pad
            .menu
            .carry(self.nav.focus, self.nav.pos, self.nav.os_hidden);
        // Preserve an existing resource-error screen opened by start_game.
        if self.options_open {
            let resource_error = game.ui.options.resource_error.take();
            game.ui.options = std::mem::take(&mut self.options);
            game.ui.options.resource_error =
                game.ui.options.resource_error.take().or(resource_error);
            game.screen = Screen::Options;
            game.options_from_title = true;
            game.dragging = self.dragging;
        }
    }

    fn queue_entry(&mut self, action: Action, player_name: String) {
        let entry = match action {
            Action::Play { folder } => PendingWorldEntry::Local {
                folder,
                name: None,
                seed: None,
                creative: false,
                create: None,
            },
            Action::Create {
                folder,
                name,
                seed,
                creative,
                options,
            } => PendingWorldEntry::Local {
                folder,
                name: Some(name),
                seed,
                creative,
                create: Some(Box::new(options)),
            },
            Action::Connect { address } => PendingWorldEntry::Server {
                address,
                name: player_name,
                cookies: None,
            },
            _ => return,
        };
        self.pending = Some(entry);
        self.perf.world_opening();
    }

    fn shortcut(&mut self, event: &KeyEvent) -> Option<clipboard::EditShortcut> {
        let edit = if cfg!(target_os = "macos") {
            self.modifiers.super_key()
        } else {
            self.modifiers.control_key()
        };
        clipboard::edit_shortcut(
            clipboard::shortcut_letter(&event.logical_key, event.physical_key),
            edit,
            self.modifiers.shift_key(),
            self.modifiers.alt_key(),
        )
    }

    /// `Screen.keyPressed` on the title screens: the action a key starts (Enter confirms, see `confirm_key` in title.rs).
    fn title_key(&mut self, event: &KeyEvent) -> Action {
        let shortcut = self.shortcut(event);
        let Some(title) = &mut self.title else {
            return Action::None;
        };
        if let Some(shortcut) = shortcut {
            if title.has_caret_field() {
                title.field_shortcut(shortcut, &mut self.clipboard);
            } else if let Some((id, value, max)) = title.focused_field()
                && self
                    .selection
                    .apply(id, value, shortcut, &mut self.clipboard, |v, t| {
                        clipboard::append_limited(v, t, max)
                    })
            {
                title.search_changed();
            }
            return Action::None;
        }
        match event.physical_key {
            PhysicalKey::Code(KeyCode::Escape) => title.escape(),
            PhysicalKey::Code(KeyCode::F5) => title.refresh_servers(),
            PhysicalKey::Code(KeyCode::Tab) => title.tab(),
            PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter) => return title.confirm_key(),
            PhysicalKey::Code(KeyCode::Space) if title.list_focused() => {
                return title.confirm_key();
            }
            PhysicalKey::Code(
                key @ (KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::ArrowLeft
                | KeyCode::ArrowRight
                | KeyCode::Home
                | KeyCode::End),
            ) if title.has_caret_field() => {
                title.field_key(
                    key,
                    self.modifiers.control_key(),
                    self.modifiers.shift_key(),
                );
            }
            _ => {
                let cleared = !title.has_caret_field()
                    && title
                        .focused_field()
                        .is_some_and(|(id, value, _)| self.selection.take(id, value));
                if cleared {
                    title.search_changed();
                }
                if event.physical_key == PhysicalKey::Code(KeyCode::Backspace) {
                    if !cleared {
                        title.backspace();
                    }
                } else if let Some(text) = &event.text {
                    title.type_text(text);
                }
            }
        }
        Action::None
    }
}

/// A panic's message: its `&str` or `String` payload.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("Box<dyn Any>")
}

/// Where the pad is on the startup title: focus navigation, as the game's pad
/// menus do it in focus mode (`pad_menu.rs`); the title has no pointer mode of
/// its own.
#[derive(Default)]
struct TitleNav {
    /// The focused target as last drawn; None until the pad is used.
    focus: Option<Target>,
    /// Where the pad put the pointer (GUI pixels); a mouse or a touch
    /// elsewhere takes it back.
    pos: Option<(f32, f32)>,
    /// The OS pointer is hidden while the pad drives.
    os_hidden: bool,
    last_poll: Option<Instant>,
    /// The held direction, when it last stepped, and whether it repeats yet.
    repeat: Option<(Button, Instant, bool)>,
    /// Steps since the held direction went down (sliders speed up).
    repeats: u32,
    /// The left stick's step direction last poll.
    stick: Option<Button>,
    wheel: [f32; 2],
    /// The focused slider being nudged and where the pad put its handle.
    nudge: Option<(gamepad::Rect, f32)>,
    /// A step that ran off the visible rows of a list, which was scrolled:
    /// it goes on once the next frame is drawn.
    pending: Option<TitlePending>,
    /// Frames the title drew: each makes its widgets the last frame's
    /// (`widgets::end_frame`), the targets the pad steps between.
    frames: u64,
}

struct TitlePending {
    button: Button,
    from: gamepad::Rect,
    generation: u64,
    before: Vec<gamepad::Rect>,
}

/// A controller on the startup title, from its first frame: the pads open with
/// the title, the D-pad or the left stick moves focus between the widgets it
/// draws (vanilla's arrow-key step), A presses, B goes back, the bumpers switch
/// tabs, the right stick scrolls, and A on a text field opens the Switch's
/// keyboard there. It feeds `menu_event` the pointer events a mouse sends, as
/// the game's pad menus feed `window_event`, and the pads and the focus go to
/// `Game` with the screen (`StartupMenu::handoff`).
impl Loading {
    /// Once per loop iteration while loading.
    pub(super) fn poll_title_pad(&mut self, event_loop: &ActiveEventLoop) {
        if !self.settings.controller.enabled {
            return;
        }
        let edges = self.menu.pads.poll_title(&self.settings.controller);
        let now = Instant::now();
        let seconds = self
            .menu
            .nav
            .last_poll
            .map_or(0.0, |t| now.duration_since(t).as_secs_f32().min(0.05));
        self.menu.nav.last_poll = Some(now);
        if self.gui.is_none() || self.menu.win.is_some() {
            return;
        }
        // A world on its way: B cancels it, as Escape does.
        if self.menu.pending.is_some() {
            if edges
                .iter()
                .any(|&(b, down)| down && matches!(b, Button::East | Button::Start))
            {
                self.menu.pending = None;
                self.menu.perf.cancel_world_opening();
            }
            return;
        }
        // A mouse move or a touch takes the pointer back.
        if let Some(at) = self.menu.nav.pos
            && ((at.0 - self.menu.cursor.0).abs() > 0.5 || (at.1 - self.menu.cursor.1).abs() > 0.5)
        {
            self.menu.nav.pos = None;
            self.menu.nav.focus = None;
            self.menu.nav.pending = None;
            self.title_show_os_pointer(true);
        }
        let batch = HudBatch::new(self.menu_size());
        let targets = super::pad_menu::on_screen(super::pad_menu::widget_targets(), &batch);
        self.title_follow_focus(event_loop, &targets);
        for (button, down) in edges {
            self.title_pad_button(event_loop, button, down, &targets);
        }
        let (move_deadzone, look_deadzone) = (
            self.settings.controller.move_deadzone,
            self.settings.controller.look_deadzone,
        );
        let state = self.menu.pads.state;
        // The left stick steps like the D-pad.
        let dir = gamepad::stick_direction(
            gamepad::radial(state.left, move_deadzone),
            super::pad_menu::STICK_STEP,
        );
        if dir != self.menu.nav.stick {
            if let Some((b, ..)) = self.menu.nav.repeat
                && Some(b) == self.menu.nav.stick
            {
                self.menu.nav.repeat = None;
            }
            self.menu.nav.stick = dir;
            if let Some(button) = dir {
                self.menu.nav.repeats = 0;
                self.title_step(event_loop, button, &targets);
                self.menu.nav.repeat = Some((button, now, false));
            }
        }
        // The right stick scrolls.
        let scroll = gamepad::radial(state.right, look_deadzone);
        let wheel = &mut self.menu.nav.wheel;
        wheel[0] += scroll[0] * super::pad_menu::SCROLL_SPEED * seconds;
        wheel[1] += scroll[1] * super::pad_menu::SCROLL_SPEED * seconds;
        if scroll == [0.0, 0.0] {
            *wheel = [0.0, 0.0];
        }
        let lines = [wheel[0].trunc(), wheel[1].trunc()];
        if lines != [0.0, 0.0] {
            wheel[0] -= lines[0];
            wheel[1] -= lines[1];
            // Stick right scrolls right: the wheel's x runs the other way.
            self.title_event(event_loop, super::pad_menu::wheel(-lines[0], lines[1]));
        }
        // A held direction repeats.
        if let Some((button, since, repeating)) = self.menu.nav.repeat {
            let wait = if repeating {
                super::pad_menu::REPEAT_EVERY
            } else {
                super::pad_menu::REPEAT_DELAY
            };
            if now.duration_since(since).as_secs_f32() >= wait {
                self.menu.nav.repeat = Some((button, now, true));
                self.menu.nav.repeats = self.menu.nav.repeats.saturating_add(1);
                self.title_step(event_loop, button, &targets);
            }
        }
    }

    fn title_pad_button(
        &mut self,
        event_loop: &ActiveEventLoop,
        button: Button,
        down: bool,
        targets: &[Target],
    ) {
        if !down {
            if self.menu.nav.repeat.is_some_and(|r| r.0 == button) {
                self.menu.nav.repeat = None;
            }
            return;
        }
        match button {
            Button::East | Button::Start => self.title_back(),
            Button::South => match self.menu.nav.focus {
                // The first press shows where focus is.
                None => self.title_focus_first(event_loop, targets),
                Some(focus) => match focus.kind {
                    // A click would jump the handle; left/right move it.
                    Kind::Slider { .. } => {}
                    Kind::Field => {
                        self.title_click(event_loop, focus.center());
                        self.title_keyboard();
                    }
                    _ => self.title_click(event_loop, focus.center()),
                },
            },
            Button::DPadUp | Button::DPadDown | Button::DPadLeft | Button::DPadRight => {
                self.menu.nav.repeats = 0;
                self.title_step(event_loop, button, targets);
                self.menu.nav.repeat = Some((button, Instant::now(), false));
            }
            Button::LeftBumper | Button::RightBumper => {
                self.title_tab(event_loop, button == Button::RightBumper, targets);
            }
            _ => {}
        }
    }

    /// B and Start: what Escape does on the title.
    fn title_back(&mut self) {
        if self.menu.options_open {
            if let Some(gui) = &self.gui {
                let events =
                    self.menu
                        .options
                        .key(&mut self.settings, &gui.atlas, KeyCode::Escape, None);
                self.menu_option_events(events);
            }
        } else if let Some(title) = &mut self.menu.title {
            title.escape();
            self.save_menu_address();
        }
    }

    /// A on a text field (on a Switch): its keyboard, starting from the field's
    /// text, which what comes back replaces, as in the game's menus. Elsewhere
    /// the field takes focus for a real keyboard.
    fn title_keyboard(&mut self) {
        #[cfg(target_os = "horizon")]
        {
            use horizon_client::clipboard::EditShortcut::SelectAll;
            if self.menu.options_open {
                if let Some(text) = horizon_nx::software_keyboard("")
                    && let Some(gui) = &self.gui
                {
                    self.menu.options.type_text(&gui.atlas, &text);
                }
                return;
            }
            let Some(title) = self.menu.title.as_mut() else {
                return;
            };
            let Some((_, value, _)) = title.focused_field() else {
                return;
            };
            let initial = value.clone();
            let Some(text) = horizon_nx::software_keyboard(&initial) else {
                return;
            };
            title.field_shortcut(SelectAll, &mut self.menu.clipboard);
            if text.is_empty() {
                title.backspace();
            } else {
                title.type_text(&text);
            }
            self.save_menu_address();
        }
    }

    /// One step of the D-pad or the stick: focus moves (a focused slider's
    /// value moves left and right instead).
    fn title_step(&mut self, event_loop: &ActiveEventLoop, button: Button, targets: &[Target]) {
        let Some(focus) = self.menu.nav.focus else {
            self.title_focus_first(event_loop, targets);
            return;
        };
        if let Kind::Slider { value, track } = focus.kind
            && matches!(button, Button::DPadLeft | Button::DPadRight)
        {
            let from = match self.menu.nav.nudge {
                Some((rect, x)) if rect == focus.rect => x,
                _ => gamepad::slider_x(track, value),
            };
            let step = gamepad::slider_step(self.menu.nav.repeats);
            let x = (from
                + if button == Button::DPadLeft {
                    -step
                } else {
                    step
                })
            .clamp(track[0], track[0] + track[1]);
            self.menu.nav.nudge = Some((focus.rect, x));
            self.title_click(event_loop, (x, focus.center().1));
            return;
        }
        let rects: Vec<gamepad::Rect> = targets.iter().map(|t| t.rect).collect();
        let vertical = matches!(button, Button::DPadUp | Button::DPadDown);
        let step = if vertical {
            gamepad::focus_step_strict(focus.rect, button, &rects)
        } else {
            gamepad::focus_step_rect(focus.rect, button, &rects)
        };
        match step {
            Some(i) => self.title_focus_on(event_loop, targets[i]),
            // Off the visible rows of a list: scroll it and go on next frame.
            None if vertical => {
                let up = button == Button::DPadUp;
                self.title_event(
                    event_loop,
                    super::pad_menu::wheel(0.0, if up { 1.0 } else { -1.0 }),
                );
                self.menu.nav.pending = Some(TitlePending {
                    button,
                    from: focus.rect,
                    generation: widgets::generation(),
                    before: rects,
                });
            }
            None => {}
        }
    }

    /// A bumper: the next or previous tab (world creation, the options).
    fn title_tab(&mut self, event_loop: &ActiveEventLoop, next: bool, targets: &[Target]) {
        let tabs: Vec<(gamepad::Rect, bool)> = targets
            .iter()
            .filter_map(|t| match t.kind {
                Kind::Tab(selected) => Some((t.rect, selected)),
                _ => None,
            })
            .collect();
        let Some(i) = gamepad::tab_step(&tabs, next) else {
            return;
        };
        let rect = tabs[i].0;
        self.title_click(event_loop, (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0));
        // The page changed under the old focus: start again on the new one.
        self.menu.nav.focus = Some(Target {
            rect,
            kind: Kind::Tab(true),
        });
    }

    /// Each poll: finish a step that scrolled a list, find the focused widget
    /// in the new frame (a screen changed: the first one), and keep the
    /// hidden pointer on it.
    fn title_follow_focus(&mut self, event_loop: &ActiveEventLoop, targets: &[Target]) {
        let rects: Vec<gamepad::Rect> = targets.iter().map(|t| t.rect).collect();
        if self
            .menu
            .nav
            .pending
            .as_ref()
            .is_some_and(|p| widgets::generation() > p.generation)
            && let Some(p) = self.menu.nav.pending.take()
        {
            // Go on from where the focused row moved, or take vanilla's vague
            // pass when nothing scrolled.
            let step = match gamepad::scroll_shift(&p.before, &rects) {
                Some(dy) => {
                    let mut from = p.from;
                    from[1] += dy;
                    if let Some(i) = gamepad::refind(from, &rects, 2.0) {
                        self.menu.nav.focus = Some(targets[i]);
                    }
                    gamepad::focus_step_strict(from, p.button, &rects)
                }
                None => gamepad::focus_step_vague(p.from, p.button, &rects),
            };
            if let Some(i) = step {
                self.title_focus_on(event_loop, targets[i]);
                return;
            }
        }
        if self.menu.nav.pending.is_some() {
            return;
        }
        let Some(focus) = self.menu.nav.focus else {
            return;
        };
        let found = gamepad::refind(focus.rect, &rects, 12.0).map(|i| targets[i]);
        let Some(focus) = found.or_else(|| super::pad_menu::first_button(targets)) else {
            return;
        };
        self.menu.nav.focus = Some(focus);
        let at = match self.menu.nav.nudge {
            Some((rect, x)) if rect == focus.rect => (x, focus.center().1),
            _ => focus.center(),
        };
        if (at.0 - self.menu.cursor.0).abs() > 0.5 || (at.1 - self.menu.cursor.1).abs() > 0.5 {
            self.title_pointer_to(event_loop, at);
        }
    }

    fn title_focus_first(&mut self, event_loop: &ActiveEventLoop, targets: &[Target]) {
        if let Some(target) = super::pad_menu::first_button(targets) {
            self.title_focus_on(event_loop, target);
        }
    }

    fn title_focus_on(&mut self, event_loop: &ActiveEventLoop, target: Target) {
        self.menu.nav.focus = Some(target);
        self.menu.nav.pending = None;
        self.menu.nav.nudge = None;
        self.title_pointer_to(event_loop, target.center());
    }

    fn title_event(&mut self, event_loop: &ActiveEventLoop, event: WindowEvent) {
        if let Err(error) = self.menu_event(event_loop, event) {
            self.error = Some(error);
            event_loop.exit();
        }
    }

    /// A pointer move to `at` (GUI pixels), as the mouse sends it.
    fn title_pointer_to(&mut self, event_loop: &ActiveEventLoop, at: (f32, f32)) {
        let batch = HudBatch::new(self.menu_size());
        self.title_event(event_loop, super::pad_menu::pointer_moved(&batch, at));
        // Read back what the menu made of it, so a real mouse move shows.
        self.menu.nav.pos = Some(self.menu.cursor);
        self.title_show_os_pointer(false);
    }

    /// A left click at `at`, the pointer staying there.
    fn title_click(&mut self, event_loop: &ActiveEventLoop, at: (f32, f32)) {
        self.title_pointer_to(event_loop, at);
        self.title_event(event_loop, super::pad_menu::pointer_button(false, true));
        self.title_event(event_loop, super::pad_menu::pointer_button(false, false));
    }

    fn title_show_os_pointer(&mut self, show: bool) {
        if self.menu.nav.os_hidden != show {
            return;
        }
        self.menu.nav.os_hidden = !show;
        if let Some(window) = &self.window {
            window.set_cursor_visible(show);
        }
    }
}

/// Each generation has its own channel and progress. Pack changes wait for the
/// old tail before starting another generation, including cache/global writes.
pub(super) fn spawn_loader(
    options: &Options,
    settings: &Settings,
    vanilla_root: &std::path::Path,
    started: Instant,
    progress: &Progress,
) -> Result<(Receiver<LoaderMsg>, JoinHandle<()>)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let settings = settings.clone();
    let vanilla_root = vanilla_root.to_path_buf();
    let dev_mods = options.dev_mods.clone();
    let progress = progress.clone();
    let handle = std::thread::Builder::new()
        .name("resource-loader".into())
        .spawn(move || {
            let mut pack_error = None;
            let root = packs::startup_root(&vanilla_root, &settings.resource_packs, &[])
                .unwrap_or_else(|error| {
                    pack_error = Some(error.to_string());
                    vanilla_root.clone()
                });
            let root = mods::content_root(&root, &settings.mods.enabled, &dev_mods);
            let result = load_resources(
                root,
                &vanilla_root,
                &settings,
                started,
                true,
                false,
                &progress,
                &mut |gui, root| {
                    let _ = sender.send(LoaderMsg::Ui(gui, root.to_path_buf()));
                },
            )
            .map(|mut loaded| {
                loaded.pack_error = loaded.pack_error.or(pack_error);
                loaded
            })
            .map_err(|error| error.to_string());
            let _ = sender.send(LoaderMsg::Done(result));
        })?;
    Ok((receiver, handle))
}

impl Loading {
    pub(super) fn poll_menu_resources(&mut self) -> Result<()> {
        // Once `Done` arrived the generation is over: the caller may have taken
        // its result, and the finished loader's channel is closed.
        if self.loaded.is_none() && !self.delivered {
            loop {
                match self.receiver.try_recv() {
                    Ok(LoaderMsg::Ui(gui, root)) => self.set_ui(gui, root),
                    Ok(LoaderMsg::Done(result)) => {
                        self.loaded = Some(result);
                        self.delivered = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        // The loader sends `Done` before it returns: it panicked.
                        let reason = match self.handle.take().map(JoinHandle::join) {
                            Some(Err(payload)) => panic_text(payload.as_ref()).to_owned(),
                            Some(Ok(())) => "it returned without a result".into(),
                            None => "no loader thread was running".into(),
                        };
                        return Err(
                            format!("resource loader stopped before finishing: {reason}").into(),
                        );
                    }
                }
            }
        }
        self.restart_menu_resources()
    }
    pub(super) fn apply_menu_settings(&mut self) {
        if let Some(renderer) = &mut self.renderer {
            renderer.gamma = self.settings.gamma;
            renderer.set_present_mode(self.settings.vsync, self.settings.horizon.present_mode);
        }
        horizon_client::hud::GUI_SCALE.store(self.settings.gui_scale, Ordering::Relaxed);
        if let Some(gui) = &mut self.gui {
            gui.atlas.font.uniform = self.settings.force_unicode;
            gui.atlas.font.japanese = self.settings.japanese_glyphs;
        }
        if let Some(window) = self.window.clone() {
            let modes = window
                .current_monitor()
                .or_else(|| window.primary_monitor())
                .map(|m| horizon_client::window::monitor_video_modes(&m))
                .unwrap_or_default();
            let preferred = self
                .settings
                .file
                .fullscreen_resolution
                .as_deref()
                .and_then(horizon_client::window::VideoMode::read);
            self.fullscreen.apply(
                &window,
                self.settings.fullscreen,
                self.settings.file.bool("exclusiveFullscreen"),
                preferred,
                false,
            );
            self.settings.video_modes = modes;
        }
        self.update_menu_cursor();
    }

    fn update_menu_cursor(&mut self) {
        let batch = HudBatch::new(self.menu_size());
        self.menu.cursor = (
            self.menu.pointer.0 as f32 / batch.scale,
            self.menu.pointer.1 as f32 / batch.scale,
        );
    }

    fn menu_size(&self) -> [u32; 2] {
        self.renderer
            .as_ref()
            .map_or([1280, 720], |r| [r.config.width, r.config.height])
    }

    fn save_menu_address(&mut self) {
        if self.menu.title.as_ref().is_some_and(|title| {
            title.last_server != self.settings.file.string("lastServer")
                || title.skip_multiplayer_warning
                    != self.settings.file.bool("skipMultiplayerWarning")
        }) {
            self.save_menu_settings();
        }
    }

    fn save_menu_settings(&mut self) {
        if let Some(title) = &self.menu.title {
            self.settings.file.set(
                "lastServer",
                horizon_client::options_file::V::Str(title.last_server.clone()),
            );
            self.settings.file.set(
                "skipMultiplayerWarning",
                horizon_client::options_file::V::Bool(title.skip_multiplayer_warning),
            );
        }
        self.settings.save(&Game::options_path());
    }

    fn open_menu_options(&mut self, page: Page) {
        self.menu.options.open_at(false, page);
        self.menu.options.languages = horizon_client::lang::available(&[&self.menu.root]);
        self.menu.options.pack_infos = packs::pack_infos(&self.vanilla_root);
        self.menu.options.pack_selection = self.settings.resource_packs.clone();
        self.menu.options.pack_screen = None;
        self.menu.options_open = true;
    }

    fn menu_option_events(&mut self, events: Vec<Event>) {
        for event in events {
            match event {
                Event::Changed => {
                    self.save_menu_settings();
                    self.apply_menu_settings();
                }
                Event::SliderDrag => {
                    self.apply_menu_settings();
                }
                Event::Close if self.menu.options.resource_error.is_some() => {}
                Event::Close => {
                    self.menu.options_open = false;
                    self.menu.dragging = false;
                    self.save_menu_settings();
                }
                Event::Language(code) => {
                    horizon_client::lang::set(horizon_client::lang::Language::load(
                        &[&self.menu.root],
                        &code,
                    ));
                    self.save_menu_settings();
                }
                Event::Packs(_) => {
                    self.save_menu_settings();
                    self.menu.reload_packs = true;
                }
                Event::OpenPackFolder => {
                    let dir = packs::packs_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    packs::open_folder(&dir);
                }
                Event::DataPacks(ids) => {
                    if let Some(title) = &mut self.menu.title {
                        title.set_data_packs(ids);
                    }
                }
                Event::OpenDataPackFolder(dir) => {
                    let _ = std::fs::create_dir_all(&dir);
                    packs::open_folder(&dir);
                }
                Event::OpenModFolder => {
                    let dir = Game::options_path().with_file_name(horizon_client::mod_list::DIR);
                    let _ = std::fs::create_dir_all(&dir);
                    packs::open_folder(&dir);
                }
                Event::ListShaderPacks => {
                    self.settings.shader_packs =
                        horizon_client::shader_pack_list::scan(&Game::shader_packs_dir());
                }
                Event::ShaderPack => self.save_menu_settings(),
                Event::OpenShaderPackFolder => {
                    let dir = Game::shader_packs_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    packs::open_folder(&dir);
                }
                Event::Credits => {
                    if let Some(gui) = &self.gui {
                        let name = self.options.as_ref().map_or_else(
                            || horizon_client::offline_name::generate(0),
                            Options::player_name,
                        );
                        let screen = horizon_client::win_screen::WinScreen::new(
                            false,
                            &self.menu.root,
                            &name,
                            &gui.atlas.font,
                        );
                        self.menu.win = Some((screen, Instant::now()));
                    }
                }
                // Mods' settings pages open only in a running game.
                Event::NativeSettings(_) | Event::NativeButton(..) => {}
                // The startup menu runs before the game opens a control socket,
                // and before its sound engine (no clicks, no volume previews).
                Event::ButtonClick | Event::CopyAgentDetails | Event::Volume(..) => {}
            }
        }
    }

    fn menu_options_click(&mut self, drag: bool, right: bool) {
        let size = self.menu_size();
        let batch = HudBatch::new(size);
        let Some(gui) = &self.gui else { return };
        let events = self.menu.options.click(
            &mut self.settings,
            &gui.atlas,
            self.menu.cursor.0,
            self.menu.cursor.1,
            (batch.width, batch.height),
            [size[0] as i32, size[1] as i32],
            drag,
            right,
        );
        self.menu_option_events(events);
    }

    fn menu_action(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        match action {
            Action::Quit => event_loop.exit(),
            Action::Options => self.open_menu_options(Page::Main),
            Action::Accessibility => self.open_menu_options(Page::Accessibility),
            Action::Language => self.open_menu_options(Page::Language),
            Action::Sounds => self.open_menu_options(Page::Sound),
            Action::Credits => self.open_menu_options(Page::Credits),
            Action::DataPacks { dir, selected } => {
                let resources = self.vanilla_root.clone();
                self.menu.options.open_data_packs(dir, selected, resources);
                self.menu.options_open = true;
            }
            action => {
                let name = self
                    .options
                    .as_ref()
                    .map_or_else(|| "Player".into(), |o| o.player_name());
                self.menu.queue_entry(action, name);
            }
        }
        self.save_menu_address();
    }

    pub(super) fn menu_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: WindowEvent,
    ) -> Result<()> {
        if let WindowEvent::KeyboardInput { event, .. } = &event
            && let PhysicalKey::Code(key) = event.physical_key
        {
            if event.state == ElementState::Pressed {
                self.menu.physical_keys.insert(key);
            } else {
                self.menu.physical_keys.remove(&key);
            }
        }
        match event {
            WindowEvent::CloseRequested => {
                if !self.settings.quit_shortcuts
                    && horizon_client::input::is_quit_shortcut_down(
                        self.menu.modifiers,
                        &self.menu.physical_keys,
                    )
                {
                    return Ok(());
                }
                self.save_menu_settings();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                self.apply_menu_settings();
            }
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::ModifiersChanged(modifiers) => self.menu.modifiers = modifiers.state(),
            WindowEvent::Focused(false) => {
                self.menu.modifiers = ModifiersState::empty();
                self.menu.physical_keys.clear();
                self.menu.dragging = false;
                if let Some(title) = &mut self.menu.title {
                    title.release();
                    title.release_field();
                }
                let events = self.menu.options.release();
                self.menu_option_events(events);
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && self.menu.pending.is_some() =>
            {
                if event.physical_key == PhysicalKey::Code(KeyCode::Escape) {
                    self.menu.pending = None;
                    self.menu.perf.cancel_world_opening();
                }
            }
            _ if self.gui.is_none() || self.menu.pending.is_some() => {}
            WindowEvent::KeyboardInput { event, .. } if self.menu.win.is_some() => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    let pressed = event.state == ElementState::Pressed;
                    if code == KeyCode::Escape && pressed {
                        self.menu.win = None;
                    } else if let Some(key) = horizon_client::win_screen::Key::from_code(code)
                        && let Some((win, _)) = &mut self.menu.win
                    {
                        win.key(key, pressed);
                    }
                }
            }
            WindowEvent::MouseInput { .. } | WindowEvent::MouseWheel { .. }
                if self.menu.win.is_some() => {}
            WindowEvent::CursorMoved { position, .. } => {
                self.menu.pointer = (position.x, position.y);
                self.update_menu_cursor();
                if self.menu.options_open && self.menu.dragging {
                    self.menu_options_click(true, false);
                } else if let Some(title) = &mut self.menu.title {
                    let size = self
                        .renderer
                        .as_ref()
                        .map_or([1280, 720], |r| [r.config.width, r.config.height]);
                    let b = HudBatch::new(size);
                    title.drag(self.menu.cursor, (b.width, b.height));
                    title.drag_field(&self.gui.as_ref().unwrap().atlas, self.menu.cursor.0);
                }
            }
            WindowEvent::MouseInput { state, button, .. } if self.menu.options_open => {
                if state == ElementState::Pressed && self.menu.options.selected_key.is_some() {
                    let events = self.menu.options.bind_mouse(&mut self.settings, button);
                    self.menu_option_events(events);
                } else if matches!(button, MouseButton::Left | MouseButton::Right) {
                    if state == ElementState::Pressed {
                        self.menu.dragging = button == MouseButton::Left;
                        self.menu_options_click(false, button == MouseButton::Right);
                    } else if button == MouseButton::Left {
                        self.menu.dragging = false;
                        let events = self.menu.options.release();
                        self.menu_option_events(events);
                    }
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let batch = HudBatch::new(self.menu_size());
                if let (Some(title), Some(gui)) = (&mut self.menu.title, &self.gui) {
                    if state == ElementState::Released {
                        title.release();
                        title.release_field();
                    } else {
                        let (action, _) = title.click(
                            self.menu.cursor.0,
                            self.menu.cursor.1,
                            (batch.width, batch.height),
                        );
                        title.press_field(
                            &gui.atlas,
                            self.menu.cursor,
                            (batch.width, batch.height),
                            self.menu.modifiers.shift_key(),
                        );
                        title.press_rules(
                            &gui.atlas,
                            self.menu.cursor,
                            (batch.width, batch.height),
                        );
                        self.menu_action(event_loop, action);
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let raw = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 30.0) as f32,
                };
                let amount = if self.settings.discrete_scroll {
                    horizon_client::mouse::java_signum(f64::from(raw)) as f32
                } else {
                    raw
                } * self.settings.mouse_wheel_sensitivity;
                let size = self.menu_size();
                let batch = HudBatch::new(size);
                if self.menu.options_open {
                    let events = self.menu.options.wheel(
                        &mut self.settings,
                        (batch.width, batch.height),
                        self.menu.cursor,
                        [size[0] as i32, size[1] as i32],
                        amount,
                    );
                    self.menu_option_events(events);
                } else if let Some(title) = &mut self.menu.title {
                    title.scroll_worlds(amount, self.menu.cursor, (batch.width, batch.height));
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if event.physical_key == PhysicalKey::Code(KeyCode::F11) {
                    self.settings.fullscreen = !self.settings.fullscreen;
                    self.apply_menu_settings();
                    self.save_menu_settings();
                } else if self.menu.options_open {
                    let shortcut = self.menu.shortcut(&event);
                    let atlas = &self.gui.as_ref().unwrap().atlas;
                    let handled = shortcut.is_some_and(|s| {
                        self.menu
                            .options
                            .shortcut(atlas, s, &mut self.menu.clipboard)
                    });
                    if !handled && let PhysicalKey::Code(key) = event.physical_key {
                        let events = self.menu.options.key(
                            &mut self.settings,
                            atlas,
                            key,
                            event.text.as_deref(),
                        );
                        self.menu_option_events(events);
                    }
                } else {
                    let action = self.menu.title_key(&event);
                    self.menu_action(event_loop, action);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn draw_menu(&mut self, batch: &mut HudBatch) {
        let Some(gui) = &self.gui else { return };
        if self.menu.pending.is_some() {
            horizon_client::level_loading::draw_progress(
                batch,
                &gui.atlas,
                self.progress.line(),
                self.progress.fraction(),
            );
        } else if let Some((win, frame)) = &mut self.menu.win {
            let now = Instant::now();
            // 20 ticks a second: the startup menu runs no tick rate.
            let delta_ticks = now.duration_since(*frame).as_secs_f32() * 20.0;
            *frame = now;
            win.draw(batch, &gui.atlas, delta_ticks, Default::default());
            if win.finished(batch.height) {
                self.menu.win = None;
            }
        } else if self.menu.options_open {
            self.menu
                .options
                .draw(&self.settings, batch, &gui.atlas, self.menu.cursor);
        } else if let Some(title) = &mut self.menu.title {
            title.poll_backup();
            if std::mem::take(&mut title.onboarding_finished) {
                // Options.onboardingAccessibilityFinished.
                self.settings.file.set(
                    "onboardAccessibility",
                    horizon_client::options_file::V::Bool(false),
                );
                self.settings.save(&Game::options_path());
            }
            let _ = title.update_toast(self.settings.notification_display_time);
            title.draw(
                &gui.atlas,
                batch,
                self.menu.cursor,
                self.settings.file.bool("hideSplashTexts"),
            );
        }
        if self.menu.pending.is_none()
            && self.menu.win.is_none()
            && self.menu.nav.pos.is_some()
            && let Some(focus) = self.menu.nav.focus
        {
            super::pad_menu::draw_focus(batch, &gui.atlas, focus);
        }
        // The game rotates the widget registry in its control poll, which doesn't
        // run before the handoff: without this the pad finds no targets here.
        self.menu.nav.frames += 1;
        widgets::end_frame(self.menu.nav.frames);
    }

    pub(super) fn restart_menu_resources(&mut self) -> Result<()> {
        if !self.menu.reload_packs || self.loaded.is_none() {
            return Ok(());
        }
        if let Some(Ok(loaded)) = &mut self.loaded {
            if let Some(loader) = &loaded.rest_loader
                && !loader.ready()?
            {
                return Ok(());
            }
            if let Some(loader) = loaded.rest_loader.take()
                && let Err(error) = loader.finish()
            {
                log::warn!("Horizon: replaced resource generation failed: {error}");
            }
        }
        self.loaded = None;
        self.delivered = false;
        if let Some(handle) = self.handle.take() {
            handle.join().map_err(|payload| {
                format!("resource loader panicked: {}", panic_text(payload.as_ref()))
            })?;
        }
        self.progress = Progress::new();
        let options = self.options.as_ref().ok_or("loading already finished")?;
        let (receiver, handle) = spawn_loader(
            options,
            &self.settings,
            &self.vanilla_root,
            self.started,
            &self.progress,
        )?;
        self.receiver = receiver;
        self.handle = Some(handle);
        self.menu.reload_packs = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gui(root: &std::path::Path) -> UiResources {
        UiResources {
            atlas: HudAtlas::fonts(root).unwrap(),
            panorama: Arc::new(horizon_client::gfx::panorama::Faces {
                size: 1,
                pixels: [96, 110, 111, 255].repeat(6),
            }),
        }
    }

    fn loading(receiver: Receiver<LoaderMsg>) -> Loading {
        Loading {
            window: None,
            renderer: None,
            gui: None,
            settings: Settings::default(),
            fullscreen: horizon_client::window::FullscreenState::new(false),
            options: None,
            vanilla_root: PathBuf::new(),
            pack_error: None,
            started: Instant::now(),
            panorama_rotation: Default::default(),
            progress: Progress::new(),
            receiver,
            loaded: None,
            delivered: false,
            handle: None,
            shown: false,
            menu: StartupMenu::default(),
            error: None,
        }
    }

    #[test]
    fn menu_is_usable_while_the_core_worker_is_blocked() {
        let root = std::env::temp_dir().join("horizon-startup-menu-no-world-resources");
        let (sender, receiver) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let (published, published_rx) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            sender
                .send(LoaderMsg::Ui(gui(&worker_root), worker_root))
                .unwrap();
            published.send(()).unwrap();
            wait.recv().unwrap();
            sender
                .send(LoaderMsg::Done(Err("core failure".into())))
                .unwrap();
        });
        published_rx.recv().unwrap();
        let mut loading = loading(receiver);
        loading.poll_menu_resources().unwrap();
        assert!(loading.gui.is_some());
        assert!(loading.loaded.is_none());
        assert!(!worker.is_finished());
        // Navigate and enter a world name without a registry, biomes, pack
        // tables or a Game. Replacing fallback artwork preserves this state.
        let title = loading.menu.title.as_mut().unwrap();
        title.mode = horizon_client::title::Mode::Create {
            name: String::new(),
            seed: "42".into(),
            creative: true,
            focus: 0,
            options: Default::default(),
        };
        title.type_text("My world");
        loading.set_ui(gui(&root), root);
        let action = loading.menu.title.as_ref().unwrap().create_action();
        loading.menu.queue_entry(action, "Player".into());
        assert!(
            matches!(&loading.menu.pending, Some(PendingWorldEntry::Local {
            name: Some(name), seed: Some(42), creative: true, create: Some(_), ..
        }) if name == "My world")
        );
        let mut batch = HudBatch::new([1280, 720]);
        loading.draw_menu(&mut batch);
        assert!(
            !batch.vertices.is_empty(),
            "queued entry draws loading progress"
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        loading.poll_menu_resources().unwrap();
        assert!(matches!(loading.loaded, Some(Err(ref error)) if error == "core failure"));
    }

    #[test]
    fn queued_server_keeps_address_and_player_identity() {
        let mut menu = StartupMenu::default();
        menu.queue_entry(
            Action::Connect {
                address: "localhost:25565".into(),
            },
            "Guest".into(),
        );
        assert!(
            matches!(menu.pending.take(), Some(PendingWorldEntry::Server { address, name, .. })
            if address == "localhost:25565" && name == "Guest")
        );
        assert!(menu.pending.take().is_none(), "an entry is consumed once");
    }

    #[test]
    fn a_failed_generation_keeps_its_error_after_the_loader_ends() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender
                .send(LoaderMsg::Done(Err("missing facts.json".into())))
                .unwrap();
        });
        let mut loading = loading(receiver);
        loading.handle = Some(worker);
        while loading.loaded.is_none() {
            loading.poll_menu_resources().unwrap();
        }
        // App::about_to_wait takes the result to report it; the loop may poll
        // once more before it exits, with the loader gone.
        assert!(matches!(loading.loaded.take(), Some(Err(e)) if e == "missing facts.json"));
        while !loading.handle.as_ref().is_none_or(JoinHandle::is_finished) {
            std::thread::yield_now();
        }
        loading.poll_menu_resources().unwrap();
        assert!(loading.loaded.is_none());
    }

    #[test]
    fn a_panicking_loader_reports_its_panic() {
        let (sender, receiver) = std::sync::mpsc::channel::<LoaderMsg>();
        let worker = std::thread::spawn(move || {
            let _sender = sender;
            panic!("atlas has no fonts");
        });
        while !worker.is_finished() {
            std::thread::yield_now();
        }
        let mut loading = loading(receiver);
        loading.handle = Some(worker);
        assert_eq!(
            loading.poll_menu_resources().unwrap_err().to_string(),
            "resource loader stopped before finishing: atlas has no fonts"
        );
    }

    #[test]
    fn disconnected_core_worker_is_reported_instead_of_waiting_forever() {
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        let mut loading = loading(receiver);
        assert_eq!(
            loading.poll_menu_resources().unwrap_err().to_string(),
            "resource loader stopped before finishing: no loader thread was running"
        );
    }
}
